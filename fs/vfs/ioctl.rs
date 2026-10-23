// SPDX-License-Identifier: GPL-2.0

//! A mounted filesystem's file ioctls, and their dispatch,
//! bch2_fs_file_ioctl() - the filesystem's file_operations'. Here are those
//! tied to the VFS: to the file, its inode, the mount. Those that only need
//! the filesystem are fs/fs/ioctl.rs's; what isn't converted yet is handed
//! back to C, __bch2_fs_file_ioctl(), whose own default is the filesystem's
//! dispatch, bch2_fs_ioctl().
//!
//! FS_IOC_GETVERSION and FS_IOC_{GET,SET}FSLABEL are the VFS's numbers, bound
//! from the kernel's headers, so their Ioctl impls are here: bcachefs_ioctl.rs's
//! c_ioctl! table is of numbers it defines, for C too.

#![allow(non_camel_case_types)]

use crate::btree::iter::{commit_do, CommitFlags, TransAttempt};
use crate::c;
use crate::errcode::{bch_errcode, ret_to_result_void, BchError};
use crate::fs::Fs;
use crate::ioctl::{get_damage, get_label, readdir_flags, reflink_p_propagate_opts, reflink_p_set_may_update_opts, snapshot_tree_v1, snapshot_tree_v2, subvolume_list, subvolume_to_path, unpoison};
use crate::init::chardev::ioctl_ret;
use crate::init::damage;
use crate::namei;
use crate::util::alloc::{flags::GFP_KERNEL, KVVec};
use crate::util::file::File;
use crate::util::ioctl::{arg, dir, input, no_arg, output, vstruct, Ioctl, UserArg};
use crate::util::kernel::{capable, Capability};
use crate::util::log::LogMsg;
use crate::vfs::inode::{InodeInfo, InodeRef};
use crate::{bch_err, btree_trans, prt};
use core::marker::PhantomData;
use core::ptr::read_volatile;
use cstruct_macros::{c_export, c_extern};
use kernel::ffi::{c_int, c_long, c_uint, c_ulong};
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
    pub fn __bch2_fs_file_ioctl(file: *mut c::file, cmd: c_uint, arg: c_ulong) -> c_long;
}

fn file_ioctl(file: &File, cmd: c_uint, user: UserPtr) -> Result<c_long, BchError> {
    // SAFETY: an open file of ours: its inode's referenced while it's open
    let inode = unsafe { InodeInfo::from_inode((*file.as_ptr()).f_inode) };
    let fs = inode.fs();
    let subvol = inode.inum().subvol as u32;

    match cmd {
        <FS_IOC_GETVERSION as Ioctl>::OPCODE =>
            output::<FS_IOC_GETVERSION>(user, || Ok(unsafe { (*inode.vinode()).i_generation })),
        <FS_IOC_GETFSLABEL as Ioctl>::OPCODE =>
            arg::<FS_IOC_GETFSLABEL, _>(user, |a| get_label(fs, a)),
        <FS_IOC_SETFSLABEL as Ioctl>::OPCODE =>
            arg::<FS_IOC_SETFSLABEL, _>(user, |a| set_label(fs, file, a)),
        <c::FS_IOC_GOINGDOWN as Ioctl>::OPCODE =>
            arg::<c::FS_IOC_GOINGDOWN, _>(user, |a| goingdown(fs, a)),
        <c::BCHFS_IOC_GET_DAMAGE as Ioctl>::OPCODE =>
            vstruct::<c::BCHFS_IOC_GET_DAMAGE, _, _>(user, |a| get_damage(fs, inode.inum(), a)),
        <c::BCHFS_IOC_CLEAR_DAMAGE as Ioctl>::OPCODE =>
            no_arg::<c::BCHFS_IOC_CLEAR_DAMAGE, _>(|| clear_damage(file, &inode)),
        <c::BCHFS_IOC_READDIR_FLAGS as Ioctl>::OPCODE => {
            let (dir, idmap) = (inode.inum(), MntIdmap::of(file));
            let is_dir = unsafe { (*inode.vinode()).i_mode } as u32 & c::S_IFMT == c::S_IFDIR;
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
                if !MntIdmap::of(file).owner_or_capable(&inode) && !capable(Capability::SysAdmin) {
                    return fs.throw(bch_errcode::BCH_ERR_EPERM_non_admin_or_owner);
                }
                reflink_p_propagate_opts(fs, inode.inum())
            }),
        <c::BCHFS_IOC_UNPOISON as Ioctl>::OPCODE =>
            input::<c::BCHFS_IOC_UNPOISON, _>(user, |a| unpoison(fs, inode.inum(),
                || MntIdmap::of(file).owner_or_capable(&inode), a)),
        _ => {
            let ret = unsafe { __bch2_fs_file_ioctl(file.as_ptr(), cmd, user.as_mut_ptr() as c_ulong) };
            if ret < 0 { Err(BchError::from_raw(-ret as i32)) } else { Ok(ret) }
        }
    }
}

/// # Safety
/// @file is an open file of ours.
#[c_export]
pub unsafe fn bch2_fs_file_ioctl(file: *mut c::file, cmd: c_uint, arg: c_ulong) -> c_long {
    ioctl_ret(file_ioctl(unsafe { File::from_raw(file) }, cmd, UserPtr::from_addr(arg as usize)))
}

/* The handlers */

/// The idmap of the mount a file was opened through, for as long as the file
/// is borrowed: an open file pins its mount. Ownership and permission are
/// as it maps the inode's ids.
#[derive(Clone, Copy)]
struct MntIdmap<'f>(*mut c::mnt_idmap, PhantomData<&'f File>);

impl<'f> MntIdmap<'f> {
    /// file_mnt_idmap()
    fn of(file: &'f File) -> Self {
        let file = file.as_ptr();
        // SAFETY: an open file's mount. READ_ONCE(), as mnt_idmap(): the
        // idmap is published by do_idmap_mount()'s smp_store_release()
        let idmap = unsafe {
            // 6.18 made f_path a const union member, __f_path its writable alias
            #[cfg(linux_ge = "6.18")]
            let mnt = (*file).__bindgen_anon_1.f_path.mnt;
            #[cfg(not(linux_ge = "6.18"))]
            let mnt = (*file).f_path.mnt;
            read_volatile(&raw const (*mnt).mnt_idmap)
        };
        MntIdmap(idmap, PhantomData)
    }

    /// Whether the caller owns @inode, or has CAP_FOWNER.
    fn owner_or_capable(self, inode: &InodeInfo<'_>) -> bool {
        unsafe { c::inode_owner_or_capable(self.0, inode.vinode()) }
    }

    /// Whether the caller has @mask - MAY_* - on @inode, by the whole VFS
    /// stack: ACLs and LSMs too.
    fn permission(self, inode: &InodeInfo<'_>, mask: u32) -> bool {
        unsafe { c::inode_permission(self.0, inode.vinode(), mask as c_int) == 0 }
    }
}

/// Write access to a file's mount, for as long as this lives:
/// mnt_want_write_file(), and mnt_drop_write_file() when it drops.
struct WriteAccess<'a>(&'a File);

impl<'a> WriteAccess<'a> {
    fn get(file: &'a File) -> Result<Self, BchError> {
        ret_to_result_void(unsafe { c::mnt_want_write_file(file.as_ptr()) })?;
        Ok(WriteAccess(file))
    }
}

impl Drop for WriteAccess<'_> {
    fn drop(&mut self) {
        unsafe { c::mnt_drop_write_file(self.0.as_ptr()) }
    }
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

fn clear_damage(file: &File, inode: &InodeInfo<'_>) -> Result<(), BchError> {
    // forensic metadata: erasing it takes ownership, as chattr does
    if !MntIdmap::of(file).owner_or_capable(inode) {
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
    let get = |inum| match InodeRef::get_trans(t, inum) {
        Err(e) if e.matches(c::ENOENT) => Ok(None),
        r => r.map(Some),
    };
    let parent = |i: &InodeRef, inum| namei::parent_inum(inum, i.info().inode());

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
        let info = i.info();
        let bi = info.inode();
        (bi.bi_subvol, bi.bi_parent_subvol, bi.bi_dir)
    };

    let (_, parent_sv, mut dir) = up(&InodeRef::get_trans(t, root)?);
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
        let i = InodeRef::get_trans(t, c::subvol_inum { subvol: parent_sv.into(), inum: dir })?;
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
        if !idmap.permission(&i.info(), mask) {
            return Ok(false);
        }
    }
    t.relock()?;
    Ok(true)
}
