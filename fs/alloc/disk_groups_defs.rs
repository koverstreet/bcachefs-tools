// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use core::ffi::c_uint;
use cstruct_macros::{c_enum, CStruct};
use nestify::nest;

/* target.type_: what a target names */
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
            pub dev: c_uint,
            pub group: c_uint,
        },
    }
}
c_default!(target);
