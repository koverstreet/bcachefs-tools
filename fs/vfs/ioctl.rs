// SPDX-License-Identifier: GPL-2.0

//! A mounted filesystem's file ioctls, and their dispatch,
//! bch2_fs_file_ioctl() - the filesystem's file_operations'. Here are those
//! tied to the VFS: to the file, its inode, the mount. Those that only need
//! the filesystem are fs/fs/ioctl.rs's, and the rest go to the filesystem's
//! dispatch, fs_ioctl().
//!
//! A subvolume is created as mkdir(2) makes a directory - the same lookup,
//! permission and security checks, the caller's umask - and destroyed as
//! rmdir(2) removes one, with MAY_WRITE on the subvolume's root; the path
//! handling is path.rs's. Each takes create_lock, for write, around the
//! create: a snapshot is of the subvolume as written, so new dirtiers are
//! held off and the dirty pages flushed - writeback write-protects every
//! mapping of a dirty folio, so a store through mmap traps to page_mkwrite,
//! which takes create_lock for read, and waits.
//!
//! FS_IOC_GETVERSION and FS_IOC_{GET,SET}FSLABEL are the VFS's numbers, bound
//! from the kernel's headers, so their Ioctl impls are here: bcachefs_ioctl.rs's
//! c_ioctl! table is of numbers it defines, for C too.

#![allow(non_camel_case_types)]

use crate::btree::iter::{commit_do, lockrestart_do, CommitFlags, TransAttempt};
use crate::c;
use crate::dirent;
use crate::errcode::{bch_errcode, ret_to_result_void, BchError};
use crate::fs::Fs;
use crate::inode_opts::reinherit_attrs as reinherit;
use crate::str_hash;
use crate::util::os_str::{OsStr, OsStrExt};
use crate::ioctl::{get_damage, get_label, readdir_flags, reflink_p_propagate_opts, reflink_p_set_may_update_opts, snapshot_tree_v1, snapshot_tree_v2, subvolume_list, subvolume_to_path, unpoison};
use crate::init::chardev::ioctl_ret;
use crate::init::damage;
use crate::namei;
use crate::util::alloc::{flags::GFP_KERNEL, KVVec};
use crate::util::file::File;
use crate::util::ioctl::{arg, dir, input, no_arg, output, vstruct, Ioctl, UserArg, UserStr};
use crate::util::kernel::{capable, current_fsgid, current_fsuid, Capability};
use crate::util::log::LogMsg;
use crate::util::printbuf::Printbuf;
use crate::init::chardev::{copy_err_msg, fs_ioctl, ErrMsg, V2};
use crate::locking::{Lock, RawLock};
use crate::vfs::dentry::Dentry;
use crate::vfs::idmap::MntIdmap;
use crate::vfs::inode::{InodeInfo, InodeRef};
use crate::vfs::mount::WriteAccess;
use crate::vfs::path::{Creating, Removing, UserPath};
use crate::{bch_err, btree_trans, prt};
use core::ffi::{c_int, c_void};
use core::mem::{offset_of, MaybeUninit};
use cstruct_macros::c_extern;
use kernel::ffi::{c_long, c_uint, c_ulong};
use kernel::uaccess::UserPtr;

const FSLABEL_MAX: usize = c::FSLABEL_MAX as usize;
const LABEL_SIZE: usize = c::BCH_SB_LABEL_SIZE as usize;

pub struct FS_IOC_GETVERSION;
impl Ioctl for FS_IOC_GETVERSION {
    const OPCODE: u32 = c::FS_IOC_GETVERSION;
    /// A long, by the number: every filesystem's written a u32.
    type Arg = u32;
    type Dir = dir::R;
}

pub struct FS_IOC_GETFSLABEL;
impl Ioctl for FS_IOC_GETFSLABEL {
    const OPCODE: u32 = c::FS_IOC_GETFSLABEL;
    type Arg = [u8; FSLABEL_MAX];
    type Dir = dir::R;
}

pub struct FS_IOC_SETFSLABEL;
impl Ioctl for FS_IOC_SETFSLABEL {
    const OPCODE: u32 = c::FS_IOC_SETFSLABEL;
    type Arg = [u8; FSLABEL_MAX];
    type Dir = dir::W;
}

/* The dispatch */

c_extern! {
    // direct.c's, until it's Rust:
    fn bch2_direct_IO_read(req: *mut c::kiocb, iter: *mut c::iov_iter, flags: c::bch_read_flags,
                           err_report: *mut c::bch_read_err_report) -> c_int;
}

fn file_ioctl(file: &File, inode: &InodeInfo, cmd: c_uint, user: UserPtr) -> Result<c_long, BchError> {
    let fs = inode.fs();
    let subvol = inode.inum().subvol as u32;

    match cmd {
        <FS_IOC_GETVERSION as Ioctl>::OPCODE =>
            output::<FS_IOC_GETVERSION>(user, || Ok(inode.vfs().generation())),
        <FS_IOC_GETFSLABEL as Ioctl>::OPCODE =>
            arg::<FS_IOC_GETFSLABEL, _>(user, |a| get_label(fs, a)),
        <FS_IOC_SETFSLABEL as Ioctl>::OPCODE =>
            arg::<FS_IOC_SETFSLABEL, _>(user, |a| set_label(fs, file, a)),
        <c::FS_IOC_GOINGDOWN as Ioctl>::OPCODE =>
            arg::<c::FS_IOC_GOINGDOWN, _>(user, |a| goingdown(fs, a)),
        <c::BCHFS_IOC_GET_DAMAGE as Ioctl>::OPCODE =>
            vstruct::<c::BCHFS_IOC_GET_DAMAGE, _, _>(user, |a| get_damage(fs, inode.inum(), a)),
        <c::BCHFS_IOC_CLEAR_DAMAGE as Ioctl>::OPCODE =>
            no_arg::<c::BCHFS_IOC_CLEAR_DAMAGE, _>(|| clear_damage(file, inode)),
        <c::BCHFS_IOC_READDIR_FLAGS as Ioctl>::OPCODE => {
            let (dir, idmap) = (inode.inum(), MntIdmap::of(file));
            let is_dir = inode.vfs().is_dir();
            arg::<c::BCHFS_IOC_READDIR_FLAGS, _>(user, |a| readdir_flags(fs, dir, is_dir,
                |t, n| path_accessible(t, idmap, n, dir), a))
        }
        <c::BCH_IOCTL_SUBVOLUME_LIST as Ioctl>::OPCODE => {
            let idmap = MntIdmap::of(file);
            arg::<c::BCH_IOCTL_SUBVOLUME_LIST, _>(user, |a| subvolume_list(fs, subvol,
                |t, root, parent| subvol_accessible(t, idmap, root, parent), a))
        }
        <c::BCH_IOCTL_SUBVOLUME_TO_PATH as Ioctl>::OPCODE =>
            arg::<c::BCH_IOCTL_SUBVOLUME_TO_PATH, _>(user, |a| subvolume_to_path(fs, a)),
        <c::BCH_IOCTL_SNAPSHOT_TREE as Ioctl>::OPCODE =>
            arg::<c::BCH_IOCTL_SNAPSHOT_TREE, _>(user, |a| snapshot_tree_v1(fs, subvol, a)),
        <c::BCH_IOCTL_SNAPSHOT_TREE_v2 as Ioctl>::OPCODE =>
            arg::<c::BCH_IOCTL_SNAPSHOT_TREE_v2, _>(user, |a| snapshot_tree_v2(fs, subvol, a)),
        <c::BCHFS_IOC_SET_REFLINK_P_MAY_UPDATE_OPTS as Ioctl>::OPCODE =>
            no_arg::<c::BCHFS_IOC_SET_REFLINK_P_MAY_UPDATE_OPTS, _>(||
                reflink_p_set_may_update_opts(fs, inode.inum())),
        <c::BCHFS_IOC_PROPAGATE_REFLINK_P_OPTS as Ioctl>::OPCODE =>
            no_arg::<c::BCHFS_IOC_PROPAGATE_REFLINK_P_OPTS, _>(|| {
                if !MntIdmap::of(file).owner_or_capable(inode.vfs()) && !capable(Capability::SysAdmin) {
                    return fs.throw(bch_errcode::BCH_ERR_EPERM_non_admin_or_owner);
                }
                reflink_p_propagate_opts(fs, inode.inum())
            }),
        <c::BCHFS_IOC_UNPOISON as Ioctl>::OPCODE =>
            input::<c::BCHFS_IOC_UNPOISON, _>(user, |a| unpoison(fs, inode.inum(),
                || MntIdmap::of(file).owner_or_capable(inode.vfs()), a)),
        // The argument is the name's address
        <c::BCHFS_IOC_REINHERIT_ATTRS as Ioctl>::OPCODE => {
            let name = UserStr::read(user.as_mut_ptr() as u64, c::BCH_NAME_MAX as usize + 1)?;
            reinherit_attrs(file, inode, OsStr::from_bytes(name.as_cstr().to_bytes()))
        }
        c::FS_IOC_SETVERSION => Err(c::ENOTTY.into()),
        <c::BCH_IOCTL_SUBVOLUME_CREATE as Ioctl>::OPCODE =>
            input::<c::BCH_IOCTL_SUBVOLUME_CREATE, _>(user, |a| subvolume_create(fs, file, a, ErrMsg::Log)),
        <c::BCH_IOCTL_SUBVOLUME_CREATE_v2 as Ioctl>::OPCODE =>
            input::<c::BCH_IOCTL_SUBVOLUME_CREATE_v2, _>(user, |a| {
                let (a, err) = a.split();
                subvolume_create(fs, file, a, err)
            }),
        <c::BCH_IOCTL_SUBVOLUME_DESTROY as Ioctl>::OPCODE =>
            input::<c::BCH_IOCTL_SUBVOLUME_DESTROY, _>(user, |a| subvolume_destroy(fs, file, a, ErrMsg::Log)),
        <c::BCH_IOCTL_SUBVOLUME_DESTROY_v2 as Ioctl>::OPCODE =>
            input::<c::BCH_IOCTL_SUBVOLUME_DESTROY_v2, _>(user, |a| {
                let (a, err) = a.split();
                subvolume_destroy(fs, file, a, err)
            }),
        <c::BCHFS_IOC_PREAD_RAW as Ioctl>::OPCODE =>
            arg::<c::BCHFS_IOC_PREAD_RAW, _>(user, |a| pread_raw(file, inode, a)),
        _ => fs_ioctl(fs, cmd, user),
    }
}

pub(crate) fn fs_file_ioctl(file: &File, cmd: c_uint, arg: c_ulong) -> c_long {
    // SAFETY: an open file of ours: its inode's referenced while it's open
    let inode = unsafe { InodeInfo::from_inode((*file.as_ptr()).f_inode) };
    ioctl_ret(file_ioctl(file, inode, cmd, UserPtr::from_addr(arg as usize)))
}

/// A 32-bit caller's: the ioctls whose numbers differ only by their
/// argument's size - misnamed, they take an int either way - as the native
/// ones, and the label's.
#[cfg(CONFIG_COMPAT)]
pub(crate) fn fs_file_compat_ioctl(file: &File, cmd: c_uint, arg: c_ulong) -> c_long {
    let cmd = match cmd {
        c::FS_IOC32_GETFLAGS   => c::FS_IOC_GETFLAGS,
        c::FS_IOC32_SETFLAGS   => c::FS_IOC_SETFLAGS,
        c::FS_IOC32_GETVERSION => c::FS_IOC_GETVERSION,
        c::FS_IOC_GETFSLABEL | c::FS_IOC_SETFSLABEL => cmd,
        _ => return -(c::ENOIOCTLCMD as c_long),
    };
    fs_file_ioctl(file, cmd, unsafe { c::rust_compat_ptr(arg as u32) } as c_ulong)
}

/* The handlers */

/// BCHFS_IOC_REINHERIT_ATTRS: the inode named @name in @dir takes on again the
/// options it would inherit from @dir - and the project, quota and all.
fn reinherit_attrs(file: &File, dir: &InodeInfo, name: &OsStr) -> Result<c_long, BchError> {
    let fs = dir.fs();
    let hash = str_hash::hash_info_init(fs, &dir.inode())?;
    let inum = dirent::lookup(fs, dir.inum(), &hash, name)?;
    let trans = btree_trans!(fs);
    let inode = lockrestart_do(&trans, |t| InodeInfo::get_trans(t, inum))?;
    // Ended before the update locks, which are taken outside transactions
    drop(trans);

    let _write = WriteAccess::get(file)?;
    let locks = InodeInfo::update_lock_all([dir, &inode]);
    let lock = locks.get(&inode);

    lock.inherit_project(dir)?;
    lock.write_inode_if(0, |_, bi| Ok(reinherit(bi, &dir.inode())))?;
    // C's "did work": always, as bch2_write_inode() returned 0 whether or
    // not it wrote
    Ok(1)
}

impl V2 for c::bch_ioctl_subvolume_v2 {
    type V1 = c::bch_ioctl_subvolume;
    fn split(self) -> (c::bch_ioctl_subvolume, ErrMsg) {
        (c::bch_ioctl_subvolume { flags: self.flags, dirfd: self.dirfd, mode: self.mode, pad: self.pad,
                                  dst_ptr: self.dst_ptr, src_ptr: self.src_ptr },
         ErrMsg::User(self.err))
    }
}

fn subvolume_create(fs: &Fs, file: &File, a: c::bch_ioctl_subvolume, err: ErrMsg)
    -> Result<(), BchError>
{
    let mut msg = Printbuf::new();
    let ret = create_subvolume(fs, file, a, &mut msg);
    err.report(&mut msg, ret, |ret, msg| crate::bch_err_msg!(fs, ret, "{}", msg))
}

/// BCH_IOCTL_SUBVOLUME_CREATE: a subvolume at @a.dst_ptr - with
/// BCH_SUBVOL_SNAPSHOT_CREATE a snapshot, of the subvolume @a.src_ptr is in,
/// or else the one the new directory's in.
fn create_subvolume(fs: &Fs, file: &File, mut a: c::bch_ioctl_subvolume, err: &mut Printbuf)
    -> Result<(), BchError>
{
    use bch_errcode as E;
    use c::{BCH_SUBVOL_SNAPSHOT_CREATE as SNAPSHOT, BCH_SUBVOL_SNAPSHOT_RO as RO};

    if a.flags & !(SNAPSHOT | RO) != 0 {
        prt!(err, "invalid flags");
        return fs.throw(E::BCH_ERR_EINVAL_subvol_create_bad_flags);
    }
    if a.flags & SNAPSHOT == 0 && (a.src_ptr != 0 || a.flags & RO != 0) {
        prt!(err, "invalid flags");
        return fs.throw(E::BCH_ERR_EINVAL_subvol_create_flags_mismatch);
    }

    let mut create_flags = c::BCH_CREATE_SUBVOL;
    if a.flags & SNAPSHOT != 0 {
        create_flags |= c::BCH_CREATE_SNAPSHOT;
    }
    if a.flags & RO != 0 {
        create_flags |= c::BCH_CREATE_SNAPSHOT_RO;
    }

    // Held until the snapshot's made, as C held it
    let src = (a.src_ptr != 0).then(|| UserPath::at(a.dirfd as c_int, a.src_ptr, c::LOOKUP_FOLLOW))
        .transpose()?;
    let mut snapshot_src = c::subvol_inum::default();
    if let Some(src) = &src {
        if !src.on(fs) {
            prt!(err, "src_path not on dst filesystem");
            return Err(c::EXDEV.into());
        }
        // SAFETY: a positive dentry of ours, held by @src
        snapshot_src = unsafe { InodeInfo::from_inode((*src.dentry()).d_inode) }.inum();
    }

    let dst = Creating::start(a.dirfd as c_int, a.dst_ptr)?;
    if !dst.on(fs) {
        prt!(err, "dst_path not on dst filesystem");
        return Err(c::EXDEV.into());
    }
    // SAFETY: held by @dst, which holds its parent, @dir, locked
    let (dentry, dir) = unsafe {
        (Dentry::borrow_raw(dst.dentry), InodeInfo::from_inode((*dst.path.dentry).d_inode))
    };
    if dentry.inode_opt().is_some() {
        return fs.throw(E::BCH_ERR_EEXIST_subvolume_create);
    }
    if dir.vfs().flag(c::S_DEAD) {
        return fs.throw(E::BCH_ERR_ENOENT_directory_dead);
    }

    // SAFETY: a mounted superblock's
    let sb = unsafe { &*(*dir.vfs().as_ptr()).i_sb };
    if !unsafe { c::kuid_has_mapping(sb.s_user_ns, current_fsuid()) &&
                 c::kgid_has_mapping(sb.s_user_ns, current_fsgid()) } {
        prt!(err, "current uid/gid not mapped into fs namespace");
        return Err(c::EOVERFLOW.into());
    }

    let idmap = MntIdmap::of(file);
    idmap.permission(dir.vfs(), c::MAY_WRITE | c::MAY_EXEC)?;

    if sb.s_flags as u64 & c::SB_POSIXACL == 0 {
        a.mode &= !(unsafe { c::current_umask() } as u16);
    }
    ret_to_result_void(unsafe { c::security_path_mkdir(&dst.path, dst.dentry, a.mode) })?;

    if a.flags & SNAPSHOT != 0 && a.src_ptr == 0 {
        snapshot_src.subvol = dir.inum().subvol;
    }

    let inode = {
        let _create = fs.snapshots.create_lock.write();
        if a.flags & SNAPSHOT != 0 {
            // SAFETY: the mounted superblock's, the kernel's rw_semaphore
            let s_umount = unsafe { &*(&raw const (*fs.vfs_sb).s_umount).cast::<c::rw_semaphore>() };
            let _umount = Lock::from_c(s_umount).read();
            unsafe { c::sync_inodes_sb(fs.vfs_sb) };
        }
        dir.create(idmap, dentry, a.mode | c::S_IFDIR as u16, 0, snapshot_src, create_flags)?
    };

    dentry.set_casefold_ops(inode.vfs());
    dentry.instantiate(inode);
    unsafe { c::fsnotify_mkdir(dir.vfs().as_ptr(), dst.dentry) };
    Ok(())
}

/// BCH_IOCTL_SUBVOLUME_DESTROY: the subvolume whose root is @a.dst_ptr,
/// unlinked, and deleted.
fn subvolume_destroy(fs: &Fs, file: &File, a: c::bch_ioctl_subvolume, err: ErrMsg)
    -> Result<(), BchError>
{
    let ret = (|| {
        if a.flags != 0 {
            return fs.throw(bch_errcode::BCH_ERR_EINVAL_subvol_destroy_bad_flags);
        }

        let r = Removing::start(a.dirfd as c_int, a.dst_ptr)?;
        if !r.on(fs) {
            return Err(c::EXDEV.into());
        }
        // SAFETY: held by @r, which holds its parent, @dir, locked
        let (victim, dir) = unsafe {
            (Dentry::borrow_raw(r.victim), InodeInfo::from_inode((*r.path.dentry).d_inode))
        };

        MntIdmap::of(file).permission(victim.inode().vfs(), c::MAY_WRITE)?;
        dir.unlink(victim, true)?;
        unsafe { c::fsnotify_rmdir(dir.vfs().as_ptr(), r.victim) };
        victim.invalidate();
        Ok(())
    })();
    // Nothing here says more than the error: no message, to log or copy
    err.report(&mut Printbuf::new(), ret, |ret, _| ret)
}

/// BCHFS_IOC_PREAD_RAW: an O_DIRECT read that reports the errors it hit -
/// checksum, I/O, decompression - rather than failing on them, and with
/// BCH_PREAD_RAW_no_poison_check, reads poisoned extents. The bytes read.
fn pread_raw(file: &File, inode: &InodeInfo, user: UserArg<c::bch_ioctl_pread_raw>)
    -> Result<c_long, BchError>
{
    let fs = inode.fs();
    let a = user.read()?;
    if a.flags & !c::BCH_PREAD_RAW_no_poison_check != 0 || a.err.pad != 0 {
        return Err(c::EINVAL.into());
    }
    if a.len == 0 {
        return Ok(0);
    }
    if file.flags() & c::O_DIRECT == 0 {
        return Err(c::EINVAL.into());
    }
    if !MntIdmap::of(file).owner_or_capable(inode.vfs()) {
        return fs.throw(bch_errcode::BCH_ERR_EPERM_non_admin_or_owner);
    }

    let pos = a.offset as c::loff_t;
    ret_to_result_void(unsafe { c::rw_verify_area(c::READ as c_int, file.as_ptr(), &pos, a.len as usize) })?;

    let mut iter = MaybeUninit::<c::iov_iter>::uninit();
    ret_to_result_void(unsafe {
        c::import_ubuf(c::ITER_DEST as c_int, a.buf as usize as *mut c_void, a.len as usize, iter.as_mut_ptr())
    })?;
    let mut kiocb = MaybeUninit::<c::kiocb>::uninit();
    unsafe {
        c::init_sync_kiocb(kiocb.as_mut_ptr(), file.as_ptr());
        (*kiocb.as_mut_ptr()).ki_pos = pos;
    }

    let flags = if a.flags & c::BCH_PREAD_RAW_no_poison_check != 0 {
        c::bch_read_flags::BCH_READ_no_poison_check
    } else {
        c::bch_read_flags::empty()
    };

    // The reads' completions add to it, under its lock: initialized where it
    // stays, for the call
    let mut report = c::bch_read_err_report::default();
    report.msg = c::printbuf::new();
    unsafe { <c::mutex as RawLock>::init(&raw mut report.lock, c"bch_read_err_report", crate::new_lock!(@key)) };

    let ret = unsafe { bch2_direct_IO_read(kiocb.as_mut_ptr(), iter.as_mut_ptr(), flags, &mut report) };
    let mut ret = if ret < 0 { Err(BchError::from_raw(-ret)) } else { Ok(ret as c_long) };

    if user.write_at(offset_of!(c::bch_ioctl_pread_raw, errors), &report.errors).is_err() {
        ret = Err(c::EFAULT.into());
    }
    // SAFETY: the report's, ours again
    let msg = unsafe { Printbuf::borrow_raw(&mut report.msg) };
    // The message's copy fails the call only if the read succeeded but read
    // nothing, as C had it
    if let (Err(e), Ok(0)) = (copy_err_msg(&a.err, msg, ret.map(|_| ())), &ret) {
        ret = Err(e);
    }
    ret
}

fn set_label(fs: &Fs, file: &File, user: UserArg<[u8; FSLABEL_MAX]>) -> Result<(), BchError> {
    if !capable(Capability::SysAdmin) {
        return fs.throw(bch_errcode::BCH_ERR_EPERM_non_admin);
    }

    let mut label = [0u8; LABEL_SIZE];
    user.bytes_at(0, LABEL_SIZE).reader().read_slice(&mut label)?;
    let Some(len) = label.iter().position(|&b| b == 0) else {
        bch_err!(fs, "unable to set label with more than {} bytes", LABEL_SIZE - 1);
        return fs.throw(bch_errcode::BCH_ERR_EINVAL_setlabel_too_long);
    };

    let _write = WriteAccess::get(file)?;

    let mut sb = fs.sb_lock();
    // strscpy(): the string and its NUL, what's after left as it was
    sb.sb_mut().label[..=len].copy_from_slice(&label[..=len]);
    fs.write_super_ret(&mut sb)
}

fn goingdown(fs: &Fs, user: UserArg<u32>) -> Result<(), BchError> {
    if !capable(Capability::SysAdmin) {
        return fs.throw(bch_errcode::BCH_ERR_EPERM_non_admin);
    }
    let flags = user.read()?;

    let mut msg = LogMsg::new(fs);
    msg.set_suppressed(true); /* cleared by ERO */
    prt!(&mut *msg, "shutdown by ioctl type {flags}");

    match flags {
        c::FSOP_GOING_FLAGS_DEFAULT => {
            // SAFETY: a mounted filesystem's superblock, and its device
            let bdev = unsafe { (*fs.vfs_sb).s_bdev };
            ret_to_result_void(unsafe { c::bdev_freeze(bdev) })?;

            let _ = fs.journal_flush();
            fs.emergency_read_only(&mut msg);

            unsafe { c::bdev_thaw(bdev) };
            Ok(())
        }
        c::FSOP_GOING_FLAGS_LOGFLUSH => {
            let _ = fs.journal_flush();
            fs.emergency_read_only(&mut msg);
            Ok(())
        }
        c::FSOP_GOING_FLAGS_NOLOGFLUSH => {
            fs.emergency_read_only(&mut msg);
            Ok(())
        }
        _ => fs.throw(bch_errcode::BCH_ERR_EINVAL_goingdown_bad_flags),
    }
}

fn clear_damage(file: &File, inode: &InodeInfo) -> Result<(), BchError> {
    // forensic metadata: erasing it takes ownership, as chattr does
    if !MntIdmap::of(file).owner_or_capable(inode.vfs()) {
        return Err(c::EPERM.into());
    }

    let _write = WriteAccess::get(file)?;

    let trans = btree_trans!(inode.fs());
    commit_do(&trans, None, CommitFlags::NO_ENOSPC, |t| damage::clear(t, inode.inum()))
}

/// Whether the caller may see @n, which a recursive READDIR_FLAGS found under
/// directory @dir without walking down to it: by the permissions that walk
/// would have checked - MAY_READ on @n's parent, which lists it, and MAY_EXEC
/// on each directory above that, up to @dir, which open() checked - the
/// whole VFS stack. Not if the walk up doesn't get to @dir: a rename raced
/// it, or an inode's gone.
fn path_accessible(t: &TransAttempt<'_, '_>, idmap: MntIdmap<'_>,
                   n: c::subvol_inum, dir: c::subvol_inum) -> Result<bool, BchError> {
    let get = |inum| match InodeInfo::get_trans(t, inum) {
        Err(e) if e.matches(c::ENOENT) => Ok(None),
        r => r.map(Some),
    };
    let parent = |i: &InodeRef, inum| namei::parent_inum(inum, &i.inode());

    let Some(inode) = get(n)? else { return Ok(false) };
    let mut p = parent(&inode, n);
    drop(inode);

    let mut dirs: KVVec<InodeRef> = KVVec::new();
    while !namei::subvol_inum_eq(p, dir) {
        // inum_is_descendant() found no loop: the cap is for a rename since
        if p.inum == 0 || dirs.len() >= 4096 {
            return Ok(false);
        }
        let Some(i) = get(p)? else { return Ok(false) };
        p = parent(&i, p);
        dirs.push(i, GFP_KERNEL)?;
    }

    permitted(t, idmap, &dirs, c::MAY_READ)
}

/// Whether the caller may reach subvolume root @root from the root of its
/// parent, @parent: MAY_EXEC on each directory of @parent's on the way. Not
/// if the walk up from @root doesn't get there.
fn subvol_accessible(t: &TransAttempt<'_, '_>, idmap: MntIdmap<'_>,
                     root: c::subvol_inum, parent: u32) -> Result<bool, BchError> {
    let up = |i: &InodeRef| {
        let bi = i.inode();
        (bi.bi_subvol, bi.bi_parent_subvol, bi.bi_dir)
    };

    let (_, parent_sv, mut dir) = up(&InodeInfo::get_trans(t, root)?);
    if parent_sv == 0 {
        return Err(c::EIO.into());
    }

    /*
     * No loop detection yet: a bi_dir loop, from corruption, walks until
     * memory runs out. bi_depth will make it cheap at runtime - every
     * directory is deeper than its parent, so a step up that isn't
     * shallower is the loop - but nothing checks it here yet.
     */
    let mut dirs: KVVec<InodeRef> = KVVec::new();
    while dir != 0 {
        let i = InodeInfo::get_trans(t, c::subvol_inum { subvol: parent_sv.into(), inum: dir })?;
        let (subvol, _, next) = up(&i);
        dirs.push(i, GFP_KERNEL)?;
        if subvol == parent {
            return permitted(t, idmap, &dirs, c::MAY_EXEC);
        }
        dir = next;
    }
    Ok(false)
}

/// Whether the caller has @first on dirs[0] and MAY_EXEC on the rest, checked
/// with the transaction unlocked: an ACL is read in a transaction of its own.
fn permitted(t: &TransAttempt<'_, '_>, idmap: MntIdmap<'_>, dirs: &[InodeRef], first: u32)
    -> Result<bool, BchError>
{
    t.unlock();
    for (k, i) in dirs.iter().enumerate() {
        let mask = if k == 0 { first } else { c::MAY_EXEC };
        if idmap.permission(i.vfs(), mask).is_err() {
            return Ok(false);
        }
    }
    t.relock()?;
    Ok(true)
}
