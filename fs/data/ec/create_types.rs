// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::data::ec::stripe_new::StripeNewBox;
use crate::locking::Lock;
use crate::types::{bits_to_longs, c_opaque};
use crate::util::bitmap::BitArray;
use core::ffi::{c_uint, c_ulong};
use cstruct_macros::{c_enum, c_xmacro, CStruct};
use pin_init::pin_data;

/// A disk label's striping of new stripes' buckets across its devices - in
/// bch_fs_ec.dev_stripe_states, by label, made on first use: shared by the
/// label's stripe heads, and repairs. Freed only with the filesystem.
#[pin_data]
#[repr(C)]
#[derive(CStruct)]
pub struct ec_dev_stripe_state {
    #[c_lock("struct mutex lock")]
    #[pin]
    pub lock: Lock<c::mutex, DevStripes>,

    pub disk_label: c_uint,
}

/// What an ec_dev_stripe_state's lock protects: how the buckets of new
/// stripes' data blocks, and of their parity, have been spread across the
/// devices, for the allocator to go on spreading them.
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct DevStripes {
    pub block_stripe: c::dev_stripe_state,
    pub parity_stripe: c::dev_stripe_state,
}

c_xmacro! {
    /*
     * open:	the sector allocator can still hand blocks out of this stripe;
     *		it's the stripe head's h->s.
     * filling:	every block has been handed to a writer, but the writers haven't
     *		finished - buckets are still checked out, and the stripe can
     *		only be completed by the writers that already hold them.
     * in_flight:	every bucket has come back, the data is all in, and its task
     *		is creating it. This is the only state guaranteed to make
     *		progress on its own.
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

/// A set of a stripe's blocks, by index.
pub type StripeBlockMask = BitArray<{ bits_to_longs(c::BCH_BKEY_PTRS_MAX as usize) }>;

// data/ec/stripe_new.rs's StripeNew, to C: what ob->ec points to
c_opaque!(ec_stripe_new);

/// The sector allocator's source of erasure coded buckets, one per disk label,
/// algorithm, redundancy and watermark - its key, fixed when it's made - and
/// the stripe it's filling, @s. In bch_fs_ec.stripe_heads, made on first use,
/// freed with the filesystem.
#[pin_data]
#[repr(C)]
#[derive(CStruct)]
pub struct ec_stripe_head {
    #[c_lock("struct mutex lock")]
    #[pin]
    pub lock: Lock<c::mutex, HeadState>,

    pub disk_label: c_uint,
    pub algo: c_uint,
    pub redundancy: c_uint,
    pub watermark: c::bch_watermark,
}

/// What a stripe head's lock protects: the devices it makes stripes on, as of
/// the allocator's rw_devs_change_count, and the stripe being filled - the
/// head's, till it hands it on.
#[repr(C)]
#[derive(CStruct)]
pub struct HeadState {
    pub insufficient_devs: bool,

    pub rw_devs_change_count: c_ulong,

    pub nr_created: u64,

    pub devs: c::bch_devs_mask,
    pub nr_active_devs: c_uint,

    pub blocksize: c_uint,

    #[c("struct ec_stripe_new *s")]
    pub s: Option<StripeNewBox>,
}

impl Default for HeadState {
    fn default() -> Self {
        HeadState {
            insufficient_devs:    false,
            rw_devs_change_count: 0,
            nr_created:           0,
            devs:                 Default::default(),
            nr_active_devs:       0,
            blocksize:            0,
            s:                    None,
        }
    }
}
