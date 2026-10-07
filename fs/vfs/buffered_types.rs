// SPDX-License-Identifier: GPL-2.0

//! The data types of vfs/buffered_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::CStruct;
use typeinfo_macros::TypeInfo;

#[cfg(not(NO_BCACHEFS_FS))]
#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_writepage_io {
    pub inode: *mut c::bch_inode_info,

    /* must be last: */
    pub op: c::bch_write_op,
}
#[cfg(not(NO_BCACHEFS_FS))]
c_default!(bch_writepage_io);
