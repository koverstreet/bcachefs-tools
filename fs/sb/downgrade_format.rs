// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::Le16;
use zerocopy::byteorder::little_endian as le;
use crate::util::vstructs::Head;
use cstruct_macros::{counted, whole, MaybeZeroable, Plain, TypeInfo};

/*
 * Packed, recovery_passes at offset 2: zerocopy's le::U64, aligned to 1,
 * not Le64, which Rust would pad for.
 */
#[counted(nr_errors)]
#[repr(C, align(2))]
#[derive(Clone, Copy, Default, MaybeZeroable, Plain, TypeInfo)]
pub struct bch_sb_field_downgrade_entry {
    pub version: Le16<c::MetadataVersion>,
    pub recovery_passes: [le::U64; 2],
    pub nr_errors: le::U16,
    pub errors: [le::U16; 0],
}

/* Each entry is 2 byte aligned and the section's length a multiple of 8, so up
 * to 6 bytes past the last are padding */
#[whole(tail = start: [u8], entries = bch_sb_field_downgrade_entry)]
#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable, Plain, TypeInfo)]
pub struct bch_sb_field_downgrade {
    pub start: [Head<c::bch_sb_field_downgrade_entry>; 0],
}
