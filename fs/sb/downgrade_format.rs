// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use zerocopy::byteorder::little_endian as le;
use crate::util::vstructs::{vstruct, Head};
use cstruct_macros::{counted, unsized_vstruct, CStruct, Plain, TypeInfo};

/*
 * Packed, recovery_passes at offset 2: zerocopy's le:: types, aligned to 1,
 * not Le16/Le64, which Rust would pad for.
 */
#[counted(nr_errors)]
#[repr(C, align(2))]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
#[c_packed]
pub struct bch_sb_field_downgrade_entry {
    pub version: le::U16,
    pub recovery_passes: [le::U64; 2],
    pub nr_errors: le::U16,
    #[c("__le16 errors[] __counted_by(nr_errors)")]
    pub errors: [le::U16; 0],
}

#[unsized_vstruct(tail = [u64])]
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
pub struct bch_sb_field_downgrade {
    pub field: Head<c::bch_sb_field>,
    pub entries: [Head<c::bch_sb_field_downgrade_entry>; 0],
}
vstruct!(unsized bch_sb_field_downgrade, field.u64s, start = 0);
