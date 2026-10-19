// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use crate::util::vstructs::Head;
use cstruct_macros::{CStruct, TypeInfo};

#[repr(C)]
#[derive(Clone, Copy, CStruct, TypeInfo)]
pub union bch_replicas_padded {
    #[c("u8 bytes[struct_size_t(struct bch_replicas_entry_v1, devs, BCH_BKEY_PTRS_MAX)]")]
    pub bytes: [u8; core::mem::size_of::<Head<c::bch_replicas_entry_v1>>()
                    + c::BCH_BKEY_PTRS_MAX as usize * core::mem::size_of::<u8>()],
    pub e: Head<c::bch_replicas_entry_v1>,
}
c_default!(bch_replicas_padded);
