// SPDX-License-Identifier: GPL-2.0

//! The data types of alloc/replicas_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::CStruct;
use typeinfo_macros::TypeInfo;

/* unsized - bch_replicas_entry_v1 is variable length */
#[repr(C)]
#[derive(Clone, Copy, CStruct, TypeInfo)]
pub struct bch_replicas_entry_cpu {
    pub ref_: c::atomic_t,
    pub e: c::bch_replicas_entry_v1,
}
c_default!(bch_replicas_entry_cpu);

#[repr(C)]
#[derive(Clone, Copy, CStruct, TypeInfo)]
pub struct bch_replicas_cpu {
    pub nr: core::ffi::c_uint,
    pub entry_size: core::ffi::c_uint,
    pub entries: *mut c::bch_replicas_entry_cpu,
}
c_default!(bch_replicas_cpu);

#[repr(C)]
#[derive(Clone, Copy, CStruct, TypeInfo)]
pub union bch_replicas_padded {
    #[c("u8 bytes[struct_size_t(struct bch_replicas_entry_v1, devs, BCH_BKEY_PTRS_MAX)]")]
    pub bytes: [u8; core::mem::size_of::<c::bch_replicas_entry_v1>()
                    + c::BCH_BKEY_PTRS_MAX as usize * core::mem::size_of::<u8>()],
    pub e: c::bch_replicas_entry_v1,
}
c_default!(bch_replicas_padded);
