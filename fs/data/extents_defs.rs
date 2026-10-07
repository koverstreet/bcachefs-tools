// SPDX-License-Identifier: GPL-2.0

//! The data types of data/extents_defs.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::{c_enum, c_verbatim, CStruct};
use typeinfo_macros::TypeInfo;

c_verbatim!(r#"
struct bch_fs;

struct btree_trans;
"#);

#[repr(C, align(8))]
#[derive(Clone, Copy, CStruct, TypeInfo)]
pub union bch_extent_crc {
    pub type_: u8,
    pub crc32: c::bch_extent_crc32,
    pub crc64: c::bch_extent_crc64,
    pub crc128: c::bch_extent_crc128,
}
c_default!(bch_extent_crc);

/* bkey_ptrs: generically over any key type that has ptrs */
#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct bkey_ptrs_c {
    pub start: *const c::bch_extent_entry,
    pub end: *const c::bch_extent_entry,
}
c_default!(bkey_ptrs_c);

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct bkey_ptrs {
    pub start: *mut c::bch_extent_entry,
    pub end: *mut c::bch_extent_entry,
}
c_default!(bkey_ptrs);

/*
 * Everything one walk of a key's pointer list can say about its replication.
 *
 * Gathered together because the walk is the expensive part - and for the exact
 * version, a stripe read per erasure coded pointer - while callers routinely
 * want several of these at once. bch2_sum_sector_overwrites() asks four
 * separate single-value helpers for them, on the same two keys, three of the
 * calls inside a loop.
 *
 * The durability counts are weighted by each device's mi.durability and skip
 * BCH_SB_MEMBER_INVALID placeholders - those are added by
 * bch2_bkey_set_needs_reconcile() on a degraded write, to stand for a replica
 * that isn't there. The raw counts below are unweighted.
 *
 * u8 except the sector count: BCH_MEMBER_DURABILITY is a two bit field and
 * BCH_REPLICAS_MAX is 4, so none of these can come near 255.
 *
 * Not handled here: KEY_TYPE_reservation, which several of the older
 * single-value helpers report as v->nr_replicas. Callers that need that still
 * special-case it themselves.
 */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct bkey_durability {
    pub online: u8,
    pub total: u8,
    pub acct: u8,
    pub min_durability: u8,

    pub nr_ptrs: u8, /* real device pointers */
    pub nr_overwritable: u8, /* uncompressed - an overwrite reclaims these */
    pub sectors_compressed: core::ffi::c_uint,

    /*
     * Copies this key occupies, for disk space accounting.
     *
     * Deliberately not weighted by mi.durability, unlike the counts above:
     * durability is OPT_RUNTIME, and accounting is persistent, so a
     * durability change would retroactively invalidate space already
     * accounted for. This has to be a function of what is physically on
     * disk.
     *
     * Differs from nr_ptrs only for a reservation, which occupies the space
     * it reserved while having no pointers at all.
     */
    pub nr_replicas: u8,

    /*
     * Copies for the purpose of "does this write increase replication" -
     * erasure coding counts, because a stripe genuinely provides it.
     *
     * Distinct from nr_replicas on purpose: parity is accounted separately
     * as BCH_DATA_parity at the stripe, so counting redundancy in a
     * per-extent space figure would charge the same parity to every extent
     * sharing the stripe. Space and replication are different questions.
     */
    pub replicas: u8,
}

/* Generic extent code: */
c_enum! {
    #[closed]
    pub enum bch_extent_overlap: u32 {
        BCH_EXTENT_OVERLAP_ALL = 0,
        BCH_EXTENT_OVERLAP_BACK = 1,
        BCH_EXTENT_OVERLAP_FRONT = 2,
        BCH_EXTENT_OVERLAP_MIDDLE = 3,
    }
}
