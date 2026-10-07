// SPDX-License-Identifier: GPL-2.0

//! The data types of fs/quota_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{GenRadix, c_default};
use cstruct_macros::{c_enum, c_typedef, CStruct};
use typeinfo_macros::TypeInfo;


#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_qid {
    pub q: [u32; c::QTYP_NR as usize],
}

c_enum! {
    #[closed]
    pub enum quota_acct_mode: u32 {
        KEY_TYPE_QUOTA_PREALLOC,
        KEY_TYPE_QUOTA_WARN,
        KEY_TYPE_QUOTA_NOCHECK,
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct memquota_counter {
    pub v: u64,
    pub hardlimit: u64,
    pub softlimit: u64,
    pub timer: i64,
    pub warns: core::ffi::c_int,
    pub warning_issued: core::ffi::c_int,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_memquota {
    pub c: [c::memquota_counter; c::Q_COUNTERS as usize],
}

c_typedef! {
    #[c("GENRADIX(struct bch_memquota) bch_memquota_table")]
    pub type bch_memquota_table = GenRadix<c::bch_memquota>;
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct quota_limit {
    pub timelimit: u32,
    pub warnlimit: u32,
}

#[repr(C)]
#[derive(Clone, Copy, CStruct, TypeInfo)]
pub struct bch_memquota_type {
    pub limits: [c::quota_limit; c::Q_COUNTERS as usize],
    pub table: c::bch_memquota_table,
    pub lock: c::mutex,
}
c_default!(bch_memquota_type);
