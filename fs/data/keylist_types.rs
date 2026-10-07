// SPDX-License-Identifier: GPL-2.0

//! The data types of data/keylist_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::CStruct;
use nestify::nest;

nest! {
    #[derive(Clone, Copy, CStruct)]*
    #[repr(C)]*
    pub struct keylist {
        #[c_anon]
        pub keys: pub union keylist_keys {
            pub keys: *mut c::bkey_i,
            pub keys_p: *mut u64,
        },
        #[c_anon]
        pub top: pub union keylist_top {
            pub top: *mut c::bkey_i,
            pub top_p: *mut u64,
        },
    }
}
c_default!(keylist);
