// SPDX-License-Identifier: GPL-2.0

//! The VFS's types, for userspace: as much of linux/fs.h's as the code both
//! builds share names - the kernel's objects, which userspace has no VFS to
//! make, and the dirent types, which are the on-disk format's too.

#![allow(non_camel_case_types)]

use core::ffi::{c_char, c_int, c_uint, c_ulong, c_void};

pub type loff_t = i64;

pub const DT_UNKNOWN: u32 = 0;
pub const DT_FIFO:    u32 = 1;
pub const DT_CHR:     u32 = 2;
pub const DT_DIR:     u32 = 4;
pub const DT_BLK:     u32 = 6;
pub const DT_REG:     u32 = 8;
pub const DT_LNK:     u32 = 10;
pub const DT_SOCK:    u32 = 12;
pub const DT_WHT:     u32 = 14;

#[repr(C)]
pub struct module {
    _unused: [u8; 0],
}

#[repr(C)]
#[derive(Debug, Copy, Clone, ::pin_init::MaybeZeroable)]
pub struct super_block {
    pub s_fs_info: *mut c_void,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, ::pin_init::MaybeZeroable)]
pub struct inode {
    pub i_ino:     c_ulong,
    pub i_size:    loff_t,
    pub i_sb:      *mut super_block,
    pub mode:      c_uint,
    pub i_private: *mut c_void,
    pub i_state:   c_uint,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, ::pin_init::MaybeZeroable)]
pub struct file {
    pub f_inode:      *mut inode,
    pub private_data: *mut c_void,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, ::pin_init::MaybeZeroable)]
pub struct file_operations {
    pub owner:   *mut module,
    pub open:    Option<unsafe extern "C" fn(*mut inode, *mut file) -> c_int>,
    pub release: Option<unsafe extern "C" fn(*mut inode, *mut file) -> c_int>,
    pub read:    Option<unsafe extern "C" fn(*mut file, *mut c_char, usize, *mut loff_t) -> isize>,
}

/*
 * This is the "filldir" function type, used by readdir() to let
 * the kernel specify what kind of dirent layout it wants to have.
 * This allows the kernel to read directories into kernel space or
 * to have different dirent layouts depending on the binary type.
 */
pub type filldir_t = Option<unsafe extern "C" fn(*mut dir_context, *const c_char, c_int, loff_t, u64, c_uint) -> c_int>;

#[repr(C)]
#[derive(Debug, Default, Copy, Clone, ::pin_init::MaybeZeroable)]
pub struct dir_context {
    pub actor: filldir_t,
    pub pos:   u64,
}
