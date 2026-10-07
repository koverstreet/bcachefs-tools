// SPDX-License-Identifier: GPL-2.0

//! The data types of alloc/disk_groups_defs.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::{c_enum, CStruct};
use nestify::nest;

/* What a target is: C's is anonymous, in its type field */
c_enum! {
    #[closed]
    pub enum bch_target_type: u32 {
        TARGET_NULL,
        TARGET_DEV,
        TARGET_GROUP,
    }
}

nest! {
    #[derive(Clone, Copy, CStruct)]*
    #[repr(C)]*
    pub struct target {
        pub type_: c::bch_target_type,

        #[c_anon]
        pub id: pub union target_id {
            pub dev: core::ffi::c_uint,
            pub group: core::ffi::c_uint,
        },
    }
}
c_default!(target);
