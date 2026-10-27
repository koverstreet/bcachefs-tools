// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use cstruct_macros::{CStruct, Plain, bitfield};
use nestify::nest;

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, Plain)]
pub struct bbpos {
    pub btree: c::btree_id,
    pub pos: c::bpos,
}

// Torn, a mix of two positions - not necessarily between them: harmless in
// what reads one racily, a walk's progress, shown
crate::tear_safe!(bbpos);

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
