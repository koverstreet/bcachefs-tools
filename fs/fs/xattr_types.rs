// SPDX-License-Identifier: GPL-2.0

// For listxattr, as xattr.h has it: kernel only.
#[cfg(not(NO_BCACHEFS_FS))]
use {crate::cstructs::c, crate::util::ffi::c_char, cstruct_macros::c_extern};

#[cfg(not(NO_BCACHEFS_FS))]
c_extern! {
    pub fn bch2_xattr_list_prefix(ty: c::bch_xattr_type, dentry: *mut c::dentry) -> *const c_char;
}
