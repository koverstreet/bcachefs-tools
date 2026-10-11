// SPDX-License-Identifier: GPL-2.0

//! What Rust calls of fs/namei.h - C gets it as prototypes, in fs/namei_gen.h,
//! which is generated from this file: see fs/types/lib.rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use core::ffi::{c_int, c_uint};
use cstruct_macros::{c_const, c_extern};

// bch2_create_trans()'s flags:
c_const! { pub const BCH_CREATE_TMPFILE: u32 = 1 << 0; }
c_const! { pub const BCH_CREATE_SUBVOL: u32 = 1 << 1; }
c_const! { pub const BCH_CREATE_SNAPSHOT: u32 = 1 << 2; }
c_const! { pub const BCH_CREATE_SNAPSHOT_RO: u32 = 1 << 3; }

c_extern! {
    pub fn bch2_create_trans(trans: *mut c::btree_trans, dir: c::subvol_inum, dir_u: *mut c::bch_inode_unpacked, new_inode: *mut c::bch_inode_unpacked, new_subvol: *mut c::bch_subvolume, name: *const c::qstr, uid: c::uid_t, gid: c::gid_t, mode: c::umode_t, rdev: c::dev_t, default_acl: *mut c::posix_acl, acl: *mut c::posix_acl, snapshot_src: c::subvol_inum, flags: c_uint) -> c_int;
    pub fn bch2_link_trans(trans: *mut c::btree_trans, dir: c::subvol_inum, dir_u: *mut c::bch_inode_unpacked, inum: c::subvol_inum, inode_u: *mut c::bch_inode_unpacked, name: *const c::qstr) -> c_int;
    pub fn bch2_unlink_trans(trans: *mut c::btree_trans, dir: c::subvol_inum, dir_u: *mut c::bch_inode_unpacked, inode: c::subvol_inum, inode_u: *mut c::bch_inode_unpacked, name: *const c::qstr, deleting_subvol: bool) -> c_int;
    pub fn bch2_rename_trans(trans: *mut c::btree_trans, src_dir: c::subvol_inum, src_dir_u: *mut c::bch_inode_unpacked, dst_dir: c::subvol_inum, dst_dir_u: *mut c::bch_inode_unpacked, src_inode_u: *mut c::bch_inode_unpacked, dst_inode_u: *mut c::bch_inode_unpacked, src_name: *const c::qstr, dst_name: *const c::qstr, mode: c::bch_rename_mode, src_opt_change: *mut c::inode_opt_change, dst_opt_change: *mut c::inode_opt_change) -> c_int;
    pub fn bch2_inum_to_path(trans: *mut c::btree_trans, inum: c::subvol_inum, out: *mut c::printbuf) -> c_int;
    pub fn bch2_inum_to_path_in_subvol(trans: *mut c::btree_trans, inum: c::subvol_inum, stop_subvol: u32, flags: c_uint, out: *mut c::printbuf) -> c_int;
    pub fn bch2_inum_is_descendant(trans: *mut c::btree_trans, inum: c::subvol_inum, ancestor: c::subvol_inum) -> c_int;
    pub fn bch2_inum_snapshot_to_path(trans: *mut c::btree_trans, inum: u64, snapshot: u32, snapshot_overwrites: *mut c::snapshot_id_list, out: *mut c::printbuf) -> c_int;
    pub fn __bch2_check_dirent_target(trans: *mut c::btree_trans, dirent_iter: *mut c::btree_iter, d: c::bkey_s_c_dirent, target: *mut c::bch_inode_unpacked, in_fsck: bool) -> c_int;
}
