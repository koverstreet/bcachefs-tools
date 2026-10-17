// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use cstruct_macros::{whole, CStruct, Plain, TypeInfo};

/*
 * Damage tracking: one key per damaged inode at (0, inum, snapshot),
 * recording which errors damaged it. Written in the same transaction as
 * the repair that did the damage, so the record can't be lost to a crash
 * between repair and bookkeeping.
 *
 * The value is the same records the errors superblock section keeps,
 * sorted by error id: bch_sb_field_error_entry_v2 packs the id, a
 * saturating occurrence count and the times of first and last
 * occurrence (BCH_SB_ERROR_ENTRY_V2_ID/NR/FIRST/LAST). One vocabulary
 * for "what happened": the sb section counts per-filesystem, damage
 * keys count per-inode.
 */
#[whole(tail = entries: [c::bch_sb_field_error_entry_v2], bkey)]
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, Plain, TypeInfo)]
#[c_name(bch_damage)]
pub struct Damage {
    pub v: c::bch_val,
    pub entries: [c::bch_sb_field_error_entry_v2; 0],
}
