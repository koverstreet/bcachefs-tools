// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use core::ffi::c_int;
use cstruct_macros::{c_enum, c_extern};
use crate::cstructs::c;

c_enum! {
    #[closed]
    pub enum bch_rename_mode: u32 {
        BCH_RENAME,
        BCH_RENAME_OVERWRITE,
        BCH_RENAME_EXCHANGE,
    }
}

c_extern! {
    pub fn bch2_dirent_get_name(d: c::bkey_s_c_dirent) -> c::qstr;
    pub fn bch2_dirent_lookup_key(trans: *mut c::btree_trans, iter: *mut c::btree_iter,
                                  dir: c::subvol_inum, hash_info: *const c::bch_hash_info,
                                  name: *const c::qstr) -> c::bkey_s_c;
    pub fn bch2_dirent_read_target(trans: *mut c::btree_trans, dir: c::subvol_inum,
                                   d: c::bkey_s_c_dirent, target: *mut c::subvol_inum) -> c_int;
    pub fn bch2_dirent_lookup(c: *mut c::bch_fs, dir: c::subvol_inum,
                              hash_info: *const c::bch_hash_info, name: *const c::qstr,
                              inum: *mut c::subvol_inum) -> c_int;
    pub fn bch2_readdir(c: *mut c::bch_fs, dir: c::subvol_inum, hash_info: *mut c::bch_hash_info,
                        ctx: *mut c::dir_context) -> c_int;
    pub fn bch2_readdir_fault_in(ctx: *mut c::dir_context);
    pub fn bch2_dir_emit(trans: *mut c::btree_trans, ctx: *mut c::dir_context,
                         d: c::bkey_s_c_dirent, target: c::subvol_inum) -> c_int;
}
