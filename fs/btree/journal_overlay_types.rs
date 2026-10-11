// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{DArray, NeBytes, c_default};
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
        #>[bitfield(u32, repr = NeBytes<3>, from = NeBytes::<3>::from_u32, into = NeBytes::<3>::to_u32)]
        pub bits: pub struct journal_key_bits {
            #[bits(8)]
            pub btree_id: c::btree_id,
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

#[repr(C)]
#[derive(CStruct)]
pub struct journal_keys {
    /* must match DArray's layout: nr, size, data, preallocated */
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
    pub pre_sort: DArray<c::journal_key>,

    pub overwrite_lock: c::mutex,
    pub overwrites: DArray<c::journal_key_range_overwritten>,
}
c_default!(journal_keys);
