// SPDX-License-Identifier: GPL-2.0

//! A directory as an open file: readdir, and the position it's up to.
//!
//! A readdir position, ctx->pos, is a dirent's offset in the dirents btree -
//! its name's hash, 64 bits - with 0 and 1 for "." and "..". The 32-bit
//! getdents ABIs round-trip d_off through a signed long and d_ino through an
//! unsigned long, so a 32-bit reader gets cookies: the position shifted down
//! by the hash's spare bits - none for a 31-bit hash, one for crc32c, 32 for
//! the 64-bit ones - and kept in [READDIR_32_FIRST, READDIR_32_EOF), and the
//! inode number truncated, 0 made 1. READDIR_32_EOF is no position's: a
//! readdir sets it only when it runs out of dirents, so a stateless reader -
//! NFS - stops, rather than rescanning the last cookie's granule forever.
//!
//! A cookie is lossy, so the open directory - Dir, the file's private data -
//! keeps the last one it handed out and the exact position after it, its
//! Cookie, and a readdir that resumes at that cookie resumes there. A seek
//! forgets it.
//!
//! The Cookie is changed only by readdir and llseek, which the VFS serializes
//! on one open file: under f_pos_lock where the file is shared - fdget_pos()
//! always takes it for a file with iterate_shared - and by its one user where
//! it isn't, as nfsd's and overlayfs's own files are. So it's LockedBy that,
//! not a lock of its own, and there's no lock to assert.
//!
//! The 32-bit readdir puts its own dir_context, Readdir32's, in front of the
//! VFS's: the actor, emit32(), translates each entry and hands it on.
//!
//! Dir's FileOps are the directory file_operations, bch2_dir_file_ops;
//! fsync and the compat ioctl are still C's.

use core::ffi::{c_int, c_uint};
use core::pin::Pin;
use kernel::ffi::{c_long, c_ulong};

use crate::c;
use crate::dirent::{self, DirContext};
use crate::errcode::{ret_to_result_void, BchError};
use crate::locking::LockedBy;
use crate::str_hash;
use crate::util::alloc::{flags, KBox};
use crate::util::ffi::{c_char, Opaque};
use crate::util::file::{File, FileOps, FileVTable, UserBuf};
use cstruct_macros::{c_export, c_extern, vtable};

use super::inode::InodeInfo;
use super::ioctl::fs_file_ioctl;

c_extern! {
    // io.c's, until it's Rust:
    fn bch2_fsync(file: *mut c::file, start: c::loff_t, end: c::loff_t, datasync: c_int) -> c_int;
}

#[cfg(CONFIG_COMPAT)]
c_extern! {
    // ioctl.c's, until it's Rust - built only with CONFIG_COMPAT:
    fn bch2_compat_fs_ioctl(file: *mut c::file, cmd: c_uint, arg: c_ulong) -> c_long;
}

const READDIR_32_FIRST: i64 = 2;
const READDIR_32_EOF: i64 = i32::MAX as i64;

#[derive(Default)]
struct Cookie {
    valid:    bool,
    cookie:   i64,
    next_pos: u64,
}

/// Whether @file's reader takes 32-bit cookies: as the file was opened, or
/// as the syscall is.
fn dirents_32bit(file: &File) -> bool {
    let mode = file.mode();
    mode & c::FMODE_32BITHASH != 0 ||
        (mode & c::FMODE_64BITHASH == 0 &&
         (cfg!(target_pointer_width = "32") || unsafe { c::in_compat_syscall() }))
}

fn cookie_shift(hash: &c::bch_hash_info) -> u32 {
    if hash.is_31bit {
        0
    } else if hash.type_ as u32 == c::bch_str_hash_type::BCH_STR_HASH_crc32c.0 {
        1
    } else {
        32
    }
}

fn pos_to_32(shift: u32, pos: u64) -> i64 {
    if pos < READDIR_32_FIRST as u64 || shift == 0 {
        return (pos as i64).min(READDIR_32_EOF - 1);
    }
    ((pos >> shift) as i64).clamp(READDIR_32_FIRST, READDIR_32_EOF - 1)
}

fn pos_from_32(shift: u32, pos: i64) -> u64 {
    if pos < READDIR_32_FIRST || shift == 0 {
        pos as u64
    } else if pos == READDIR_32_FIRST {
        READDIR_32_FIRST as u64
    } else if pos >= READDIR_32_EOF {
        u64::MAX
    } else {
        (pos as u64) << shift
    }
}

/// @ctx first: emit32() is given it, and takes it to the whole.
#[repr(C)]
struct Readdir32<'a> {
    ctx:   c::dir_context,
    dst:   *mut c::dir_context,
    cookie: &'a mut Cookie,
    shift: u32,
    /// An entry was refused: the readdir stopped at a full buffer, not the end.
    full:  bool,
}

unsafe extern "C" fn emit32(ctx: *mut c::dir_context, name: *const c_char, namelen: c_int,
                            offset: c::loff_t, ino: u64, d_type: c_uint) -> bool {
    // SAFETY: a Readdir32's ctx, given as the whole Readdir32's pointer
    let r = unsafe { &mut *ctx.cast::<Readdir32<'_>>() };
    let cookie = pos_to_32(r.shift, offset as u64);
    let ino = (ino as u32).max(1);

    // SAFETY: the VFS's dir_context, and its actor
    let emitted = unsafe {
        let actor = (*r.dst).actor.expect("a dir_context's actor");
        actor(r.dst, name, namelen, cookie, ino as u64, d_type)
    };
    if emitted {
        *r.cookie = Cookie { valid: true, cookie, next_pos: offset as u64 + 1 };
    } else {
        r.full = true;
    }
    emitted
}

pub(crate) struct Dir(LockedBy<Cookie>);

impl Dir {
    /// # Safety
    /// In readdir or llseek, which the VFS serializes on one open file.
    unsafe fn cookie(&self) -> &mut Cookie {
        unsafe { self.0.protected_mut() }
    }
}

#[vtable]
impl FileOps for Dir {
    // #[vtable] sees compat_ioctl() whether or not it's built
    const HAS_COMPAT_IOCTL: bool = cfg!(CONFIG_COMPAT);

    fn open(_inode: &Opaque<c::inode>, _file: &File) -> Result<Pin<KBox<Self>>, BchError> {
        Ok(KBox::pin(Dir(LockedBy::new(Cookie::default(), ())), flags::GFP_KERNEL)?)
    }

    /// generic_read_dir()'s: read(2) of a directory is EISDIR.
    fn read(&self, _file: &File, _buf: &mut UserBuf) -> Result<(), BchError> {
        Err(BchError::from(c::EISDIR))
    }

    /// A 32-bit reader's positions are cookies, up to READDIR_32_EOF.
    fn llseek(&self, file: &File, offset: i64, whence: c_int) -> c::loff_t {
        let max = if dirents_32bit(file) { READDIR_32_EOF } else { i64::MAX };

        // SAFETY: llseek
        unsafe { self.cookie() }.valid = false;
        unsafe { c::generic_file_llseek_size(file.as_ptr(), offset, whence, max, max) }
    }

    fn iterate_shared(&self, file: &File, ctx: *mut c::dir_context) -> Result<(), BchError> {
        // SAFETY: an open file's inode
        let inode = unsafe { InodeInfo::from_inode((*file.as_ptr()).f_inode) };
        let fs = inode.fs();
        let hash = &mut str_hash::hash_info_init(fs, &inode.inode())?;

        let ret = if !dirents_32bit(file) {
            // SAFETY: the VFS's, for the call
            match unsafe { c::dir_emit_dots(file.as_ptr(), ctx) } {
                true  => dirent::readdir(fs, inode.inum(), hash, unsafe { DirContext::from_raw(ctx) }),
                false => Ok(()),
            }
        } else {
            // SAFETY: readdir; and the VFS's ctx
            let (cookie, pos) = unsafe { (self.cookie(), (*ctx).pos) };
            let shift = cookie_shift(hash);
            let start = if cookie.valid && cookie.cookie == pos {
                cookie.next_pos
            } else {
                pos_from_32(shift, pos)
            };

            let mut r = Readdir32 {
                ctx:    c::dir_context { actor: Some(emit32), pos: start as c::loff_t, ..Default::default() },
                dst:    ctx,
                cookie,
                shift,
                full:   false,
            };
            // Its actor reaches the rest of it: the whole Readdir32's pointer
            let r32 = (&raw mut r).cast::<c::dir_context>();

            let ret = match unsafe { c::dir_emit_dots(file.as_ptr(), r32) } {
                true  => dirent::readdir(fs, inode.inum(), hash, unsafe { DirContext::from_raw(r32) }),
                false => Ok(()),
            };

            // Out of dirents, or stopped at a full buffer - the dots' too: only
            // the actor knows
            let eof = ret.is_ok() && !r.full;
            // SAFETY: the VFS's
            unsafe { (*ctx).pos = if eof { READDIR_32_EOF } else { pos_to_32(shift, r.ctx.pos as u64) } };
            ret
        };
        crate::bch_err_fn!(fs, ret)
    }

    fn fsync(&self, file: &File, start: i64, end: i64, datasync: c_int) -> Result<(), BchError> {
        ret_to_result_void(unsafe { bch2_fsync(file.as_ptr(), start, end, datasync) })
    }

    fn unlocked_ioctl(&self, file: &File, cmd: c_uint, arg: c_ulong) -> c_long {
        fs_file_ioctl(file, cmd, arg)
    }

    #[cfg(CONFIG_COMPAT)]
    fn compat_ioctl(&self, file: &File, cmd: c_uint, arg: c_ulong) -> c_long {
        unsafe { bch2_compat_fs_ioctl(file.as_ptr(), cmd, arg) }
    }
}

#[c_export]
pub static bch2_dir_file_ops: c::file_operations = FileVTable::<Dir>::OPS;
