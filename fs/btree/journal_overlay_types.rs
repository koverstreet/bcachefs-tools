// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::locking::Lock;
use crate::types::{DArray, c_default};
use crate::util::gap_buffer::GapBuffer;
use crate::util::rcu::{RcuArray, RcuElement};
use crate::util::vstructs::Head;
use crate::{new_lock, published};
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, AtomicU8, AtomicUsize};
use cstruct_macros::{bitfield, CStruct, Init};
use nestify::nest;
use pin_init::pin_data;

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
    pub j: Head<c::jset>,
}
c_default!(journal_replay);

nest! {
    #[derive(CStruct)]*
    #[repr(C)]
    pub struct journal_key {
        #[c_anon]
        #>[repr(C)]
        #>[derive(Clone, Copy)]
        pub src: pub union journal_key_src {
            #[c_anon]
            #>[repr(C)]
            #>[derive(Clone, Copy)]
            pub journal: pub struct journal_key_journal_pos {
                pub journal_seq_offset: u32,
                pub journal_offset: u32,
            },
            pub allocated_k: *mut c::bkey_i,
        },
        #[c_bitfield]
        #>[derive(CStruct)]-
        #>[bitfield(u16)]
        pub id: pub struct journal_key_id {
            #[bits(8)] pub btree_id: c::btree_id,
            level:       u8,
        },
        #[c_bitfield]
        #>[derive(CStruct)]-
        #>[bitfield(u8)]
        pub flags: pub struct journal_key_flags {
            allocated:   bool,
            rewind:      bool,
            __pad:       u6,
        },
        /*
         * Set, and overwritten_range, under overwrite_lock, while the keys
         * are read without it: in a byte of their own
         */
        pub overwritten: AtomicU8,
        pub overwritten_range: AtomicU32,
    }
}
c_default!(journal_key);

/// A run of overwritten keys, [start, end) by index, that a reader skips in
/// one step: btree/journal_overlay.rs.
#[repr(C)]
#[derive(Default)]
pub struct OverwriteRange {
    pub start: AtomicUsize,
    pub end:   AtomicUsize,
}

// SAFETY: atomics, valid zeroed, owning nothing
unsafe impl RcuElement for OverwriteRange {}

/* overwrite_lock's: how many of the ranges are in use, the first a sentinel */
#[repr(C)]
#[derive(CStruct)]
pub struct OverwriteRanges {
    pub overwrites_nr: usize,
}

published! {
    /// The ranges, read without overwrite_lock under RCU: a key's
    /// overwritten_range indexes them.
    #[derive(CStruct)]
    pub struct OverwriteRangesLive {
        #[c_anon("void *overwrites")]
        pub ranges: RcuArray<OverwriteRange>,
    }
}

#[pin_data]
#[repr(C)]
#[derive(CStruct, Init)]
pub struct journal_keys {
    /*
     * The keys, a gap buffer: util/gap_buffer.rs, whose fields C reads as
     * its own. Written only where nothing else reads them: keys_mut()
     */
    #[c_anon("size_t nr; size_t size; struct journal_key *data; struct journal_key preallocated[0]; size_t gap")]
    pub keys: UnsafeCell<GapBuffer<c::journal_key>>,
    #[init(AtomicI32::new(1))]
    pub ref_: AtomicI32,
    #[init(true)]
    pub initial_ref_held: bool,
    /* Bumped by each insert: an iterator that saw another searches again */
    pub seq: AtomicU64,

    /*
     * Keys inserted before journal_keys_sort() (e.g. from
     * bch2_dev_usage_init during offline device add). These survive
     * the reset in journal_keys_sort and get merged into the main
     * array after sorting. Written as the keys are.
     */
    #[c_anon("DARRAY(struct journal_key) pre_sort")]
    pub pre_sort: UnsafeCell<DArray<c::journal_key>>,

    #[pin]
    #[init(new_lock!(<- ::pin_init::init_zeroed(), ::pin_init::zeroed(), "&keys->overwrite_lock"))]
    #[c_lock("struct mutex overwrite_lock")]
    pub overwrites: Lock<c::mutex, OverwriteRanges, OverwriteRangesLive>,
}
