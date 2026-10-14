// SPDX-License-Identifier: GPL-2.0

//! The VFS, for Rust: shims out of line in vfs/fs.c, as util/locking.c's are.
//! C gets the prototypes, in vfs/rust_gen.h. Kernel only, as the VFS is.
//!
//! struct bch_inode_info is kernel types all the way down - struct inode,
//! locks, the pagecache - so Rust doesn't bind its layout: it's opaque there,
//! and reached through these. Nor does Rust call kernel functions that aren't
//! in the kernel crate's bindings, posix_acl's among them: those are here too.

#![allow(non_camel_case_types)]
// The VFS's: kernel only, as C's #ifndef NO_BCACHEFS_FS
#![cfg(not(NO_BCACHEFS_FS))]

use crate::cstructs::c;
use core::ffi::{c_int, c_uint, c_void};
use cstruct_macros::c_extern;

c_extern! {
    /* bch_inode_info: */
    pub fn rust_to_bch_ei(inode: *mut c::inode) -> *mut c::bch_inode_info;
    pub fn rust_ei_vinode(ei: *mut c::bch_inode_info) -> *mut c::inode;
    pub fn rust_ei_fs(ei: *mut c::bch_inode_info) -> *mut c::bch_fs;
    pub fn rust_ei_inode(ei: *mut c::bch_inode_info) -> *mut c::bch_inode_unpacked;
    pub fn rust_ei_inum(ei: *mut c::bch_inode_info) -> c::subvol_inum;
    pub fn rust_ei_update_lock(ei: *mut c::bch_inode_info);
    pub fn rust_ei_update_unlock(ei: *mut c::bch_inode_info);
    pub fn rust_ei_set_projid(c: *mut c::bch_fs, ei: *mut c::bch_inode_info, projid: u32) -> c_int;

    /* dentries: */
    pub fn rust_dentry_ei(dentry: *mut c::dentry) -> *mut c::bch_inode_info;
    pub fn rust_dentry_parent_inode_opt(dentry: *mut c::dentry, opt: c_uint, v: *mut u64) -> bool;
    pub fn rust_dir_casefold_changed(dentry: *mut c::dentry);

    /* xattr handlers: */
    pub fn rust_xattr_handler_flags(handler: *const c::xattr_handler) -> c_int;

    /* posix ACLs: */
    pub fn posix_acl_alloc(count: c_uint, flags: c::gfp_t) -> *mut c::posix_acl;
    pub fn rust_posix_acl_count(acl: *const c::posix_acl) -> c_uint;
    pub fn rust_posix_acl_entry(acl: *const c::posix_acl, i: c_uint, tag: *mut u16, perm: *mut u16,
                                id: *mut u32);
    pub fn rust_posix_acl_set_entry(acl: *mut c::posix_acl, i: c_uint, tag: u16, perm: u16,
                                    id: u32);
    pub fn rust_posix_acl_dup(acl: *mut c::posix_acl) -> *mut c::posix_acl;
    pub fn rust_posix_acl_release(acl: *mut c::posix_acl);
    pub fn rust_set_cached_acl(inode: *mut c::inode, type_: c_int, acl: *mut c::posix_acl);
    pub fn rust_posix_acl_update_mode(idmap: *mut c::mnt_idmap, inode: *mut c::inode,
                                      mode: *mut c::umode_t, acl: *mut *mut c::posix_acl) -> c_int;
    pub fn __posix_acl_chmod(acl: *mut *mut c::posix_acl, gfp: c::gfp_t, mode: c::umode_t) -> c_int;
}
