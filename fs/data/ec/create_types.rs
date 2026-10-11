// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{DArray, bits_to_longs, c_default};
use core::ffi::{c_int, c_uint, c_ulong};
use cstruct_macros::{bitfield, c_enum, c_xmacro, CStruct, c_typedef};
use nestify::nest;

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct ec_dev_stripe_state {
    pub list: c::list_head,
    pub lock: c::mutex,

    pub disk_label: c_uint,

    pub block_stripe: c::dev_stripe_state,
    pub parity_stripe: c::dev_stripe_state,
}
c_default!(ec_dev_stripe_state);

c_enum! {
    #[closed]
    pub enum ec_stripe_ref: u32 {
        STRIPE_REF_io,
        STRIPE_REF_stripe,
        STRIPE_REF_NR,
    }
}

c_xmacro! {
    /*
     * open:	the sector allocator can still hand blocks out of this stripe;
     *		it's the stripe head's h->s.
     * filling:	every block has been handed to a writer, but the writers haven't
     *		finished - buckets are still checked out, and the stripe can
     *		only be completed by the writers that already hold them.
     * in_flight:	every bucket has come back, the data is all in, and creation is
     *		queued on ec.stripe_create_wq. This is the only state guaranteed
     *		to make progress on its own.
     *
     * The filling/in_flight split is what makes it possible to wait on stripe
     * buffer memory safely: a stripe that is filling may never complete (its
     * writers can go away), so blocking on one can deadlock, while an in_flight
     * stripe always drains.
     */
    EC_STRIPE_NEW_STATES(x) {
        (open),
        (filling),
        (in_flight),
    }
}

c_enum! {
    #[closed]
    pub enum ec_stripe_new_state: u32 {
        EC_STRIPE_NEW_STATES(n) => EC_STRIPE_NEW_ ## n,
        EC_STRIPE_NEW_STATE_NR,
    }
}

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct ec_stripe_new_bucket {
    pub hash: c::hlist_node,
    pub dev_bucket: u64,
}
c_default!(ec_stripe_new_bucket);

nest! {
    #[repr(C)]
    #[derive(CStruct)]
    pub struct ec_stripe_new {
        pub c: *mut c::bch_fs,
        pub ctxt: *mut c::moving_context,
        pub lock: c::mutex,
        pub list: c::list_head,
        pub work: c::work_struct,
        pub cl: c::closure,

        pub ref_: [c::atomic_t; c::STRIPE_REF_NR as usize],

        /* seq is only assigned once the refs are gone, so it can't give an age */
        pub start_time: u64,
        pub seq: u64,

        pub err: c_int,

        /*
         * Set by ec_old_stripe_fold() from the read's completion, read by
         * create: its own field so neither side needs a lock to say what
         * happened. @old_stripe_lost_blocks is the blocks that were carried forward
         * and are unreadable, i.e. the data the failure actually cost us.
         */
        pub old_stripe_err: c_int,
        pub old_stripe_lost_blocks: u32,

        pub devs: c::bch_devs_mask,
        pub watermark: c::bch_watermark,
        pub state: c::ec_stripe_new_state,

        #[c_bitfield]
        #>[bitfield(u8)]
        pub bits: pub struct ec_stripe_new_bits {
            have_old_stripe:     bool,

            allocated:           bool,
            mem_allocated:       bool,
            old_stripe_read:     bool,
            old_stripe_read_all: bool,
            __pad:               u3,
        },

        #[c("unsigned long blocks_gotten[BITS_TO_LONGS(BCH_BKEY_PTRS_MAX)]")]
        pub blocks_gotten: [c_ulong; bits_to_longs(c::BCH_BKEY_PTRS_MAX as usize)],
        #[c("unsigned long blocks_allocated[BITS_TO_LONGS(BCH_BKEY_PTRS_MAX)]")]
        pub blocks_allocated: [c_ulong; bits_to_longs(c::BCH_BKEY_PTRS_MAX as usize)],
        #[c("unsigned long blocks_moving[BITS_TO_LONGS(BCH_BKEY_PTRS_MAX)]")]
        pub blocks_moving: [c_ulong; bits_to_longs(c::BCH_BKEY_PTRS_MAX as usize)],
        pub blocks: [c::open_bucket_idx_t; c::BCH_BKEY_PTRS_MAX as usize],
        pub res: c::disk_reservation,

        pub buckets: [c::ec_stripe_new_bucket; c::BCH_BKEY_PTRS_MAX as usize],

        pub new_stripe: c::ec_stripe_buf,
        pub old_stripe: c::ec_stripe_buf,

        pub new_stripe_handle: c::ec_stripe_handle,
        pub old_stripe_handle: c::ec_stripe_handle,

        pub old_block_map: [u8; c::BCH_BKEY_PTRS_MAX as usize],
        pub old_blocks_nr: u8,
    }
}
c_default!(ec_stripe_new);

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct ec_stripe_head {
    pub list: c::list_head,
    pub lock: c::mutex,

    pub disk_label: c_uint,
    pub algo: c_uint,
    pub redundancy: c_uint,
    pub watermark: c::bch_watermark,
    pub insufficient_devs: bool,

    pub rw_devs_change_count: c_ulong,

    pub nr_created: u64,

    pub devs: c::bch_devs_mask,
    pub nr_active_devs: c_uint,

    pub blocksize: c_uint,

    pub dev_stripe: *mut c::ec_dev_stripe_state,

    pub s: *mut c::ec_stripe_new,
}
c_default!(ec_stripe_head);

/*
 * Lazy per-(disk_label, sectors) cache of RW member counts (the can_widen
 * target); shared by callers that walk stripes and need the widening target
 * many times (scan, check). Kept eytzinger-sorted between inserts.
 */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct widen_cache_entry {
    pub disk_label: u8,
    pub sectors: u16,
    pub nr_devs: u16,
}

c_typedef! {
    pub type widen_cache = DArray<c::widen_cache_entry>;
}
