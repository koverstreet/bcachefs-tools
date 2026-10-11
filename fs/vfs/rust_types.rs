// SPDX-License-Identifier: GPL-2.0

//! What Rust calls of vfs/rust.h - the VFS shims, and vfs/fs.c's functions -
//! declared to Rust: C gets the prototypes, in vfs/rust_gen.h, which rust.h
//! includes. Kernel only, as the VFS is.

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

    /* vfs/fs.c: */
    pub fn bch2_inode_update_after_write(trans: *mut c::btree_trans, ei: *mut c::bch_inode_info,
                                         bi: *mut c::bch_inode_unpacked, fields: c_uint);
    pub fn bch2_write_inode(c: *mut c::bch_fs, ei: *mut c::bch_inode_info,
                            set: Option<unsafe extern "C" fn(trans: *mut c::btree_trans, ei: *mut c::bch_inode_info, bi: *mut c::bch_inode_unpacked, p: *mut c_void) -> c_int>,
                            p: *mut c_void, fields: c_uint) -> c_int;
    pub fn bch2_inode_or_descendents_is_open(trans: *mut c::btree_trans, pos: c::bpos) -> c_int;

    /* xattr handlers: */
    pub fn rust_xattr_handler_flags(handler: *const c::xattr_handler) -> c_int;

    /* posix ACLs: */
    pub fn rust_posix_acl_alloc(trans: *mut c::btree_trans, nr: c_uint) -> *mut c::posix_acl;
    pub fn rust_posix_acl_count(acl: *const c::posix_acl) -> c_uint;
    pub fn rust_posix_acl_entry(acl: *const c::posix_acl, i: c_uint, tag: *mut u16, perm: *mut u16,
                                id: *mut u32);
    pub fn rust_posix_acl_set_entry(acl: *mut c::posix_acl, i: c_uint, tag: u16, perm: u16,
                                    id: u32);
    pub fn rust_posix_acl_release(acl: *mut c::posix_acl);
    pub fn rust_set_cached_acl(inode: *mut c::inode, type_: c_int, acl: *mut c::posix_acl);
    pub fn rust_posix_acl_update_mode(idmap: *mut c::mnt_idmap, inode: *mut c::inode,
                                      mode: *mut c::umode_t, acl: *mut *mut c::posix_acl) -> c_int;
    pub fn rust_posix_acl_chmod(trans: *mut c::btree_trans, acl: *mut *mut c::posix_acl,
                                mode: c::umode_t) -> c_int;
}
