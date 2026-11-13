// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::data::ec::stripe_new::StripeNewBox;
use crate::locking::{ByteLock, Locked, Lockdep};
use crate::types::{bits_to_longs, c_opaque};
use crate::data_structures::bitmap::BitArray;
use core::ffi::{c_uint, c_ulong};
use cstruct_macros::{bch_enum, CStruct};
use pin_init::pin_data;

/// A disk label's striping of new stripes' buckets across its devices - in
/// bch_fs_ec.dev_stripe_states, by label, made on first use: shared by the
/// label's stripe heads, and repairs. Freed only with the filesystem.
#[pin_data]
#[repr(C)]
#[derive(CStruct)]
pub struct ec_dev_stripe_state {
    #[pin]
    pub lock: Locked<Lockdep<ByteLock>, DevStripes>,

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
#[bch_enum(closed)]
#[prefix(EC_STRIPE_NEW_)]
#[nr]
#[repr(u32)]
pub enum ec_stripe_new_state {
    open,
    filling,
    in_flight,
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
    #[pin]
    pub lock: Locked<Lockdep<ByteLock>, HeadState>,

    pub disk_label: c_uint,
    pub algo: c_uint,
    pub redundancy: c_uint,
    pub watermark: c::bch_watermark,
}

/// The devices a stripe head makes stripes on - its disk label's, as
/// disk_label_ec_devs() has them - and what follows from them, all as of one
/// count of the allocator's RW device changes: set together, by
/// stripe_head_devs_update(), whenever that's moved on.
#[repr(C)]
#[derive(Default, CStruct)]
pub struct HeadDevs {
    /// The allocator's rw_devs_change_count they're as of
    pub change_count: c_ulong,
    pub mask:         c::bch_devs_mask,
    pub nr_active:    c_uint,
    pub blocksize:    c_uint,
    /// Too few for the head's redundancy: it makes no stripes
    pub insufficient: bool,
}

/// What a stripe head's lock protects: the devices it makes stripes on, and
/// the stripe being filled - the head's, till it hands it on.
#[repr(C)]
#[derive(Default, CStruct)]
pub struct HeadState {
    pub devs: HeadDevs,

    pub nr_created: u64,

    pub s: Option<StripeNewBox>,
}
