// SPDX-License-Identifier: GPL-2.0

//! The data types of alloc/replicas_format.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use cstruct_macros::{c_verbatim, CStruct};
use typeinfo_macros::TypeInfo;

#[repr(C, packed)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_replicas_entry_v0 {
    pub data_type: u8,
    pub nr_devs: u8,
    #[c("__u8 devs[] __counted_by(nr_devs)")]
    pub devs: [u8; 0],
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
#[c_packed]
pub struct bch_sb_field_replicas_v0 {
    pub field: c::bch_sb_field,
    pub entries: [c::bch_replicas_entry_v0; 0],
}

#[repr(C, packed)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_replicas_entry_v1 {
    pub data_type: u8,
    pub nr_devs: u8,
    pub nr_required: u8,
    /* No counted_by: bch_replicas_cpu entries are all the size of the biggest entry */
    pub devs: [u8; 0],
}

#[repr(C, align(8))]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
#[c_packed]
pub struct bch_sb_field_replicas {
    pub field: c::bch_sb_field,
    pub entries: [c::bch_replicas_entry_v1; 0],
}

c_verbatim!(r#"
#define replicas_entry_bytes(_i)					\
	(offsetof(typeof(*(_i)), devs) + (_i)->nr_devs)

#define replicas_entry_add_dev(e, d) ({					\
	(e)->nr_devs++;							\
	(e)->devs[(e)->nr_devs - 1] = (d);				\
})
"#);
