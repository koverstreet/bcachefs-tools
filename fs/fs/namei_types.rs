// SPDX-License-Identifier: GPL-2.0

//! What Rust calls of fs/namei.h - C gets it as prototypes, in fs/namei_gen.h,
//! which is generated from this file: see fs/types/lib.rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use cstruct_macros::{c_const, c_extern};

// bch2_create_trans()'s flags:
c_const! { pub const BCH_CREATE_TMPFILE: u32 = 1 << 0; }
c_const! { pub const BCH_CREATE_SUBVOL: u32 = 1 << 1; }
c_const! { pub const BCH_CREATE_SNAPSHOT: u32 = 1 << 2; }
c_const! { pub const BCH_CREATE_SNAPSHOT_RO: u32 = 1 << 3; }

// What Rust calls of fs/namei.h: C gets these as prototypes, in fs/namei_gen.h
// with the types - see c_extern! in types/cstruct.rs.
c_extern! {
    pub fn bch2_create_trans(arg1: *mut c::btree_trans, arg2: c::subvol_inum, arg3: *mut c::bch_inode_unpacked, arg4: *mut c::bch_inode_unpacked, arg5: *mut c::bch_subvolume, arg6: *const c::qstr, arg7: c::uid_t, arg8: c::gid_t, arg9: c::umode_t, arg10: c::dev_t, arg11: *mut c::posix_acl, arg12: *mut c::posix_acl, arg13: c::subvol_inum, arg14: core::ffi::c_uint) -> core::ffi::c_int;
    pub fn bch2_link_trans(arg1: *mut c::btree_trans, arg2: c::subvol_inum, arg3: *mut c::bch_inode_unpacked, arg4: c::subvol_inum, arg5: *mut c::bch_inode_unpacked, arg6: *const c::qstr) -> core::ffi::c_int;
    pub fn bch2_unlink_trans(arg1: *mut c::btree_trans, arg2: c::subvol_inum, arg3: *mut c::bch_inode_unpacked, arg4: c::subvol_inum, arg5: *mut c::bch_inode_unpacked, arg6: *const c::qstr, arg7: bool) -> core::ffi::c_int;
    pub fn bch2_rename_trans(arg1: *mut c::btree_trans, arg2: c::subvol_inum, arg3: *mut c::bch_inode_unpacked, arg4: c::subvol_inum, arg5: *mut c::bch_inode_unpacked, arg6: *mut c::bch_inode_unpacked, arg7: *mut c::bch_inode_unpacked, arg8: *const c::qstr, arg9: *const c::qstr, arg10: c::bch_rename_mode, arg11: *mut c::inode_opt_change, arg12: *mut c::inode_opt_change) -> core::ffi::c_int;
    pub fn bch2_inum_to_path(arg1: *mut c::btree_trans, arg2: c::subvol_inum, arg3: *mut c::printbuf) -> core::ffi::c_int;
    pub fn bch2_inum_to_path_in_subvol(arg1: *mut c::btree_trans, arg2: c::subvol_inum, arg3: u32, arg4: core::ffi::c_uint, arg5: *mut c::printbuf) -> core::ffi::c_int;
    pub fn bch2_inum_is_descendant(arg1: *mut c::btree_trans, arg2: c::subvol_inum, arg3: c::subvol_inum) -> core::ffi::c_int;
    pub fn bch2_inum_snapshot_to_path(arg1: *mut c::btree_trans, arg2: u64, arg3: u32, arg4: *mut c::snapshot_id_list, arg5: *mut c::printbuf) -> core::ffi::c_int;
    pub fn __bch2_check_dirent_target(arg1: *mut c::btree_trans, arg2: *mut c::btree_iter, arg3: c::bkey_s_c_dirent, arg4: *mut c::bch_inode_unpacked, arg5: bool) -> core::ffi::c_int;
}
