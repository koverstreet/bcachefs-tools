// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::util::vstructs::{vstruct, Ffi};
use cstruct_macros::{counted, unsized_vstruct, CStruct, Plain, TypeInfo};

#[repr(C, packed)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
pub struct bch_replicas_entry_v0 {
    pub data_type: u8,
    pub nr_devs: u8,
    #[c("__u8 devs[] __counted_by(nr_devs)")]
    pub devs: [u8; 0],
}

#[unsized_vstruct(tail = [u64])]
#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
#[c_packed]
pub struct bch_sb_field_replicas_v0 {
    pub field: Ffi<c::bch_sb_field>,
    pub entries: [c::bch_replicas_entry_v0; 0],
}
vstruct!(unsized bch_sb_field_replicas_v0, field.u64s, start = 0);

#[counted(nr_devs)]
#[repr(C, packed)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
pub struct bch_replicas_entry_v1 {
    pub data_type: u8,
    pub nr_devs: u8,
    pub nr_required: u8,
    /* No __counted_by in C: bch_replicas_cpu entries are all the size of the biggest entry */
    pub devs: [u8; 0],
}

#[unsized_vstruct(tail = [u64])]
#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
#[c_packed]
pub struct bch_sb_field_replicas {
    pub field: Ffi<c::bch_sb_field>,
    pub entries: [Ffi<c::bch_replicas_entry_v1>; 0],
}
vstruct!(unsized bch_sb_field_replicas, field.u64s, start = 0);

