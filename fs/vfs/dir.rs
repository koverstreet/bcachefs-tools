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
//! The 32-bit readdir puts its own DirEmit, Readdir32, in front of the VFS's
//! dir_context: it translates each entry and hands it on.
//!
//! Dir's FileOps are the directory file_operations, DIR_FILE_OPS;
//! fsync is still C's.

use core::ffi::{c_int, c_uint};
use core::pin::Pin;
use kernel::ffi::{c_long, c_ulong};

use crate::c;
use crate::format::StrHashKind;
use crate::posix::str_hash::bch_hash_info;
use crate::posix::dirent::{self, DirEmit};
use crate::os::dir_context::DirContext;
use crate::errcode::{BchError, Errcode};
use crate::locking::{Guard, LockedBy};
use crate::posix::str_hash;
use crate::util::alloc::KBox;
use kernel::alloc::flags;
use crate::util::ffi::Opaque;
use crate::os::file::{File, FileOps, FileVTable, FPos, PosLocked, UserBuf};
use crate::types::CStatic;
use cstruct_macros::vtable;

use super::io::fsync;
use super::inode::InodeInfo;
use super::ioctl::fs_file_ioctl;

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
         (cfg!(target_pointer_width = "32") || crate::os::kernel::in_compat_syscall()))
}

fn cookie_shift(hash: &bch_hash_info) -> u32 {
    if hash.is_31bit {
        0
    } else if hash.type_ == StrHashKind::crc32c {
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

/// A 32-bit reader's readdir, in front of the VFS's dir_context: each entry
/// handed on with its position a cookie and its inode number truncated.
struct Readdir32<'a, 'c> {
    dst:    DirContext<'c>,
    pos:    u64,
    cookie: &'a mut Cookie,
    shift:  u32,
    /// An entry was refused: the readdir stopped at a full buffer, not the end.
    full:   bool,
}

impl DirEmit for Readdir32<'_, '_> {
    fn pos(&self) -> u64 {
        self.pos
    }

    fn set_pos(&mut self, pos: u64) {
        self.pos = pos;
    }

    fn emit(&mut self, name: &[u8], offset: u64, ino: u64, d_type: u32) -> bool {
        let cookie = pos_to_32(self.shift, offset);
        let ino = (ino as u32).max(1);

        let emitted = self.dst.emit(name, cookie as u64, ino as u64, d_type);
        if emitted {
            *self.cookie = Cookie { valid: true, cookie, next_pos: offset + 1 };
        } else {
            self.full = true;
        }
        emitted
    }
}

impl DirEmit for DirContext<'_> {
    fn pos(&self) -> u64 {
        DirContext::pos(self)
    }

    fn set_pos(&mut self, pos: u64) {
        DirContext::set_pos(self, pos)
    }

    fn emit(&mut self, name: &[u8], pos: u64, ino: u64, d_type: u32) -> bool {
        DirContext::emit(self, name, pos, ino, d_type)
    }
}

/// "." and "..", where @e's position is before them - false once it's full:
/// dir_emit_dots().
fn emit_dots(file: &File<Dir>, e: &mut impl DirEmit) -> bool {
    let dentry = file.dentry();
    if e.pos() == 0 {
        if !e.emit(b".", 0, file.inode().vfs().ino(), c::DT_DIR) {
            return false;
        }
        e.set_pos(1);
    }
    if e.pos() == 1 {
        // a root is its own parent
        let ino = dentry.with_parent_inode(|p| p.map(|p| p.vfs().ino())).unwrap_or(file.inode().vfs().ino());
        if !e.emit(b"..", 1, ino, c::DT_DIR) {
            return false;
        }
        e.set_pos(2);
    }
    true
}

pub(crate) struct Dir(LockedBy<Cookie, (), FPos>);

#[vtable]
impl FileOps for Dir {
    type Data = Self;
    type Inode = InodeInfo;

    // As a regular file's (file.rs): no module reference by each open
    const MODULE_REF: bool = false;

    // #[vtable] sees compat_ioctl() whether or not it's built
    const HAS_COMPAT_IOCTL: bool = cfg!(CONFIG_COMPAT);

    fn open(_inode: &Opaque<c::inode>, _file: &File) -> Result<Pin<KBox<Self>>, BchError> {
        Ok(KBox::pin(Dir(LockedBy::new(Cookie::default(), ())), flags::GFP_KERNEL)?)
    }

    /// generic_read_dir()'s: read(2) of a directory is EISDIR.
    fn read(&self, _file: &File<Self>, _buf: &mut UserBuf) -> Result<(), BchError> {
        Err(Errcode::EISDIR_read_dir.into())
    }

    /// A 32-bit reader's positions are cookies, up to READDIR_32_EOF.
    fn llseek(&self, file: &File<Self>, fpos: &mut PosLocked<'_, Self>, offset: i64, whence: c_int)
        -> Result<u64, BchError>
    {
        let max = if dirents_32bit(file) { READDIR_32_EOF as u64 } else { u64::MAX };

        fpos.get_mut_of(&self.0).valid = false;
        file.generic_llseek_size(offset, whence, max, max)
    }

    fn iterate_shared(&self, file: &File<Self>, fpos: &mut PosLocked<'_, Self>, mut ctx: DirContext<'_>)
        -> Result<(), BchError>
    {
        let inode = file.inode();
        let fs = inode.fs();
        let hash = &mut str_hash::hash_info_init(fs, &inode.inode())?;

        let ret = if !dirents_32bit(file) {
            match emit_dots(file, &mut ctx) {
                true  => dirent::readdir(fs, inode.inum(), hash, ctx),
                false => Ok(()),
            }
        } else {
            let cookie = fpos.get_mut_of(&self.0);
            let pos = ctx.pos() as i64;
            let shift = cookie_shift(hash);
            let start = if cookie.valid && cookie.cookie == pos {
                cookie.next_pos
            } else {
                pos_from_32(shift, pos)
            };

            let mut r = Readdir32 { dst: ctx, pos: start, cookie, shift, full: false };
            let ret = match emit_dots(file, &mut r) {
                true  => dirent::readdir(fs, inode.inum(), hash, &mut r),
                false => Ok(()),
            };

            // Out of dirents, or stopped at a full buffer - the dots' too: only
            // the emit knows
            let eof = ret.is_ok() && !r.full;
            let pos = if eof { READDIR_32_EOF } else { pos_to_32(shift, r.pos) };
            r.dst.set_pos(pos as u64);
            ret
        };
        crate::bch_err_fn!(fs, ret)
    }

    fn fsync(&self, file: &File<Self>, start: i64, end: i64, datasync: c_int) -> Result<(), BchError> {
        fsync(file.inode(), file, start, end, datasync)
    }

    fn unlocked_ioctl(&self, file: &File<Self>, cmd: c_uint, arg: c_ulong) -> c_long {
        fs_file_ioctl(file, file.inode(), None, cmd, arg)
    }

    #[cfg(CONFIG_COMPAT)]
    fn compat_ioctl(&self, file: &File<Self>, cmd: c_uint, arg: c_ulong) -> c_long {
        super::ioctl::fs_file_compat_ioctl(file, file.inode(), None, cmd, arg)
    }
}

pub static DIR_FILE_OPS: CStatic<c::file_operations, Dir> = FileVTable::<Dir>::STATIC;
