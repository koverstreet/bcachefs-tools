// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use core::ffi::c_int;
use cstruct_macros::{c_enum, c_extern};
use crate::cstructs::c;
use crate::util::Printbuf;

c_enum! {
    #[closed]
    pub enum bch_rename_mode: u32 {
        BCH_RENAME,
        BCH_RENAME_OVERWRITE,
        BCH_RENAME_EXCHANGE,
    }
}

c_extern! {
    pub fn bch2_readdir_fault_in(ctx: *mut c::dir_context);
    pub fn bch2_dir_emit(trans: *mut c::btree_trans, ctx: *mut c::dir_context,
                         d: c::bkey_s_c_dirent, target: c::subvol_inum) -> c_int;
    pub fn bch2_filldir64_specialization_to_text(out: *mut c::printbuf);
}

/// Whether readdir's specialized filldir64 is in use, and why not.
pub fn filldir64_specialization_to_text(out: &mut Printbuf) {
    unsafe { bch2_filldir64_specialization_to_text(out.as_raw()) }
}
