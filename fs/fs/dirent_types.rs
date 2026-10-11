// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use core::ffi::c_int;
use cstruct_macros::{c_enum, c_extern, rust_c_extern};
use crate::cstructs::c;
use crate::dirent::format::{bch2_dirent_to_text, bch2_dirent_validate};

c_enum! {
    #[closed]
    pub enum bch_rename_mode: u32 {
        BCH_RENAME,
        BCH_RENAME_OVERWRITE,
        BCH_RENAME_EXCHANGE,
    }
}

// bkey_ops' methods, Rust's: defined with C's signatures - see rust_c_extern!.
rust_c_extern! {
    pub fn bch2_dirent_validate(arg1: *mut c::bch_fs, arg2: c::bkey_s_c, arg3: *const c::bkey_validate_context) -> c_int;
    pub fn bch2_dirent_to_text(arg1: *mut c::printbuf, arg2: *mut c::bch_fs, arg3: c::bkey_s_c);
}

c_extern! {
    pub fn bch2_dirent_get_name(arg1: c::bkey_s_c_dirent) -> c::qstr;
    pub fn bch2_dirent_lookup_key(arg1: *mut c::btree_trans, arg2: *mut c::btree_iter, arg3: c::subvol_inum, arg4: *const c::bch_hash_info, arg5: *const c::qstr) -> c::bkey_s_c;
    pub fn bch2_dirent_read_target(arg1: *mut c::btree_trans, arg2: c::subvol_inum, arg3: c::bkey_s_c_dirent, arg4: *mut c::subvol_inum) -> c_int;
    pub fn bch2_dirent_lookup(arg1: *mut c::bch_fs, arg2: c::subvol_inum, arg3: *const c::bch_hash_info, arg4: *const c::qstr, arg5: *mut c::subvol_inum) -> c_int;
    pub fn bch2_readdir(arg1: *mut c::bch_fs, arg2: c::subvol_inum, arg3: *mut c::bch_hash_info, arg4: *mut c::dir_context) -> c_int;
    pub fn bch2_readdir_fault_in(arg1: *mut c::dir_context);
    pub fn bch2_dir_emit(arg1: *mut c::btree_trans, arg2: *mut c::dir_context, arg3: c::bkey_s_c_dirent, arg4: c::subvol_inum) -> c_int;
}
