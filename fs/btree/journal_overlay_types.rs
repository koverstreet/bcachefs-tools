// SPDX-License-Identifier: GPL-2.0

//! The data types of btree/journal_overlay_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{DArray, c_default};
use cstruct_macros::{bitfield, CStruct};
use nestify::nest;

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct journal_ptr {
    pub csum_good: bool,
    pub csum: c::bch_csum,
    pub dev: u8,
    pub bucket: u32,
    pub bucket_offset: u32,
    pub sector: u64,
}

/*
 * Only used for holding the journal entries we read in btree_journal_read()
 * during cache_registration
 */
#[repr(C)]
#[derive(CStruct)]
pub struct journal_replay {
    #[c("DARRAY_PREALLOCATED(struct journal_ptr, 8) ptrs")]
    pub ptrs: DArray<c::journal_ptr, 8>,

    pub csum_good: bool,
    pub ignore_blacklisted: bool,
    pub ignore_not_dirty: bool,
    /* must be last: */
    pub j: c::jset,
}
c_default!(journal_replay);

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct journal_key_range_overwritten {
    pub start: usize,
    pub end: usize,
}

nest! {
    #[derive(Clone, Copy, CStruct)]*
    #[repr(C)]
    pub struct journal_key {
        #[c_anon]
        #>[repr(C)]
        pub src: pub union journal_key_src {
            #[c_anon]
            #>[repr(C)]
            pub journal: pub struct journal_key_journal_pos {
                pub journal_seq_offset: u32,
                pub journal_offset: u32,
            },
            pub allocated_k: *mut c::bkey_i,
        },
        #[c_bitfield]
        #>[derive(Clone, Copy, CStruct)]-
        #>[bitfield(u32, repr = crate::types::NeBytes::<3>, from = crate::types::NeBytes::<3>::from_u32, into = crate::types::NeBytes::<3>::to_u32)]
        pub btree_id_bits: pub struct journal_key_btree_id_bits {
            #[bits(8)]
            pub btree_id: u32,
            #[bits(8)]
            pub level: u32,
            #[bits(1)]
            pub allocated: bool,
            #[bits(1)]
            pub overwritten: bool,
            #[bits(1)]
            pub rewind: bool,
            #[bits(13)]
            pub __pad: u16,
        },
        pub overwritten_range: u32,
    }
}
c_default!(journal_key);
impl journal_key {
    pub fn btree_id(&self) -> u32 { let b = self.btree_id_bits; b.btree_id() }
    pub fn set_btree_id(&mut self, v: u32) { let mut b = self.btree_id_bits; b.set_btree_id(v); self.btree_id_bits = b; }
    pub fn level(&self) -> u32 { let b = self.btree_id_bits; b.level() }
    pub fn set_level(&mut self, v: u32) { let mut b = self.btree_id_bits; b.set_level(v); self.btree_id_bits = b; }
    pub fn allocated(&self) -> bool { let b = self.btree_id_bits; b.allocated() }
    pub fn set_allocated(&mut self, v: bool) { let mut b = self.btree_id_bits; b.set_allocated(v); self.btree_id_bits = b; }
    pub fn overwritten(&self) -> bool { let b = self.btree_id_bits; b.overwritten() }
    pub fn set_overwritten(&mut self, v: bool) { let mut b = self.btree_id_bits; b.set_overwritten(v); self.btree_id_bits = b; }
    pub fn rewind(&self) -> bool { let b = self.btree_id_bits; b.rewind() }
    pub fn set_rewind(&mut self, v: bool) { let mut b = self.btree_id_bits; b.set_rewind(v); self.btree_id_bits = b; }
}

#[repr(C)]
#[derive(CStruct)]
pub struct journal_keys {
    /* must match layout in darray_types.h */
    pub nr: usize,
    pub size: usize,
    pub data: *mut c::journal_key,
    pub preallocated: [c::journal_key; 0],
    /*
     * Gap buffer: instead of all the empty space in the array being at the
     * end of the buffer - from @nr to @size - the empty space is at @gap.
     * This means that sequential insertions are O(n) instead of O(n^2).
     */
    pub gap: usize,
    pub ref_: c::atomic_t,
    pub initial_ref_held: bool,

    /*
     * Keys inserted before journal_keys_sort() (e.g. from
     * bch2_dev_usage_init during offline device add). These survive
     * the reset in journal_keys_sort and get merged into the main
     * array after sorting.
     */
    #[c("DARRAY(struct journal_key) pre_sort")]
    pub pre_sort: DArray<c::journal_key>,

    pub overwrite_lock: c::mutex,
    #[c("DARRAY(struct journal_key_range_overwritten) overwrites")]
    pub overwrites: DArray<c::journal_key_range_overwritten>,
}
c_default!(journal_keys);
