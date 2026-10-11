// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]
// The VFS's: kernel only, as C's #ifndef NO_BCACHEFS_FS
#![cfg(not(NO_BCACHEFS_FS))]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::CStruct;
use typeinfo_macros::TypeInfo;

#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_writepage_io {
    pub inode: *mut c::bch_inode_info,

    /* must be last: */
    pub op: c::bch_write_op,
}
c_default!(bch_writepage_io);
