// SPDX-License-Identifier: GPL-2.0

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
