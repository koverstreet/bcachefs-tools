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

use crate::c;
use crate::errcode::{bch_errcode, ret_to_result_void, BchError};
use crate::fs::Fs;
use crate::ioctl::get_label;
use crate::init::chardev::ioctl_ret;
use crate::util::file::File;
use crate::util::ioctl::{arg, dir, output, Ioctl, UserArg};
use crate::util::kernel::{capable, Capability};
use crate::util::log::LogMsg;
use crate::vfs::inode::InodeInfo;
use crate::{bch_err, prt};
use cstruct_macros::{c_export, c_extern};
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
    pub fn __bch2_fs_file_ioctl(file: *mut c::file, cmd: c_uint, arg: c_ulong) -> c_long;
}

fn file_ioctl(file: &File, cmd: c_uint, user: UserPtr) -> Result<c_long, BchError> {
    // SAFETY: an open file of ours: its inode's referenced while it's open
    let inode = unsafe { InodeInfo::from_inode((*file.as_ptr()).f_inode) };
    let fs = inode.fs();

    match cmd {
        <FS_IOC_GETVERSION as Ioctl>::OPCODE =>
            output::<FS_IOC_GETVERSION>(user, || Ok(unsafe { (*inode.vinode()).i_generation })),
        <FS_IOC_GETFSLABEL as Ioctl>::OPCODE =>
            arg::<FS_IOC_GETFSLABEL, _>(user, |a| get_label(fs, a)),
        <FS_IOC_SETFSLABEL as Ioctl>::OPCODE =>
            arg::<FS_IOC_SETFSLABEL, _>(user, |a| set_label(fs, file, a)),
        <c::FS_IOC_GOINGDOWN as Ioctl>::OPCODE =>
            arg::<c::FS_IOC_GOINGDOWN, _>(user, |a| goingdown(fs, a)),
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
