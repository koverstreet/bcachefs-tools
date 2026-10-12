// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use cstruct_macros::{CStruct, bitfield};
use nestify::nest;

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct bbpos {
    pub btree: c::btree_id,
    pub pos: c::bpos,
}

/*
 * Layout is padding-free (bpos is __packed __aligned(4)), so a blbpos can be
 * used directly as a memcmp/rhashtable key.
 */
nest! {
    #[repr(C)]
    #[derive(Clone, Copy, Default, CStruct)]
    pub struct blbpos {
        #[c_bitfield]
        #>[bitfield(u16)]
        pub bits: pub struct blbpos_bits {
            #[bits(16)] pub btree: c::btree_id,
        },
        pub level: u16,
        pub pos: c::bpos,
    }
}
