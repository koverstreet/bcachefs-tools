// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::types::c_default;
use cstruct_macros::CStruct;

/// keys..top, whole keys back to back: C reads each pointer as a bkey_i * or
/// a u64 *, Rust as the u64 *.
#[derive(CStruct)]
#[repr(C)]
pub struct keylist {
    #[c_anon("union { struct bkey_i *keys; u64 *keys_p; }")]
    pub(crate) keys_p: *mut u64,
    #[c_anon("union { struct bkey_i *top; u64 *top_p; }")]
    pub(crate) top_p: *mut u64,
}
c_default!(keylist);
