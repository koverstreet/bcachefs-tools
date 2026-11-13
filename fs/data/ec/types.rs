// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::data::ec::delete::delete_empty_stripes;
use crate::data::ec::io::EcBio;
use crate::data::ec::stripe_new::NewStripes;
use crate::fs::Fs;
use crate::locking::{ByteLock, Locked, Lockdep};
use crate::new_lock;
use crate::util::alloc::{KBox, KVVec};
use crate::util::bio::BioSet;
use crate::data_structures::cuckoo::Cuckoo;
use crate::util::futex::EventCount;
use crate::data_structures::genradix::SharedGenRadix;
use crate::os::workqueue::{ReffedWork, Workqueue, WqFlags};
use core::pin::Pin;
use core::sync::atomic::{AtomicI64, AtomicU64};
use cstruct_macros::{bitfield, placed, MaybeZeroable, Init, TypeInfo, BitfieldMembers};
use nestify::nest;
use pin_init::pin_data;

/// gc's copy of a stripe, which check_allocations counts its blocks' sectors
/// into.
#[repr(C)]
#[derive(MaybeZeroable)]
pub struct gc_stripe {
    pub lock: Locked<ByteLock, GcStripeData>,
}

nest! {
    /// What a gc_stripe's lock protects. Not packed, as a bucket's is:
    /// @ptrs are referenced, and there's one of these per stripe, not per
    /// bucket.
    #[repr(C)]
    #[derive(Clone, Copy, Default, MaybeZeroable, BitfieldMembers)]
    pub struct GcStripeData {
        #[members]
        #>[bitfield(u8)]
        pub bits: pub struct gc_stripe_bits {
            /* does a corresponding key exist in stripes btree? */
            alive: u1,
            __pad: u7,
        },
        pub sectors: u16,
        pub nr_blocks: u8,
        pub nr_redundant: u8,
        pub block_sectors: [u16; c::BCH_BKEY_PTRS_MAX as usize],
        pub ptrs: [c::bch_extent_ptr; c::BCH_BKEY_PTRS_MAX as usize],

        pub r: c::bch_replicas_padded,
    }
}

#[placed(Fs.ec)]
#[pin_data]
#[repr(C)]
#[derive(MaybeZeroable, TypeInfo, Init)]
pub struct bch_fs_ec {
    /// Stripe buffers' memory in use, against ec_stripe_buf_limit: Rust's
    /// alone.
    #[pin]
    #[init(new_lock!(0))]
    pub(crate) stripe_buf_bytes: Locked<c::spinlock_t, usize>,
    /// Who waits for it, woken together as it's freed: the write path's
    /// closures, until it converts, and Rust's waiters.
    pub stripe_buf_wait: c::closure_waitlist,
    pub(crate) stripe_buf_freed: EventCount,

    /// The open stripes, by index, and the buckets of stripes being made:
    /// see trigger.rs. Rust's alone.
    #[pin]
    #[init(::pin_init::init_zeroed())]
    pub(crate) stripes_open: Cuckoo,
    #[pin]
    #[init(::pin_init::init_zeroed())]
    pub(crate) stripes_new_buckets: Cuckoo,

    /// The stripe heads, made on first use - boxed, as their users hold them
    /// without this lock: Rust's alone.
    #[pin]
    #[init(new_lock!(KVVec::new()))]
    pub(crate) stripe_heads: Locked<Lockdep<ByteLock>, KVVec<Pin<KBox<c::ec_stripe_head>>>>,

    /// Each disk label's ec_dev_stripe_state, made on first use - boxed, as
    /// stripe heads point at them: Rust's alone.
    #[pin]
    #[init(new_lock!(KVVec::new()))]
    pub(crate) dev_stripe_states: Locked<Lockdep<ByteLock>, KVVec<Pin<KBox<c::ec_dev_stripe_state>>>>,

    /// The stripes being made, once their heads or repairs hand them on,
    /// until their task is done with them; and room for those held. Boxed,
    /// as their users hold them without this lock: Rust's alone. See
    /// stripe_new.rs.
    #[pin]
    #[init(new_lock!(NewStripes::new()))]
    pub(crate) new_stripes: Locked<c::mutex, NewStripes>,
    /// Woken as a stripe goes in flight, and as one's freed: for the flushes.
    pub(crate) new_stripes_changed: EventCount,
    pub stripe_new_seq: AtomicI64,

    /// What stripes being made's tasks run on: destroyed with the filesystem,
    /// which drains their last runs - see stripe_new.rs.
    #[init(Workqueue::new(c"bcachefs_ec_create", WqFlags::UNBOUND, 0))]
    pub stripe_create_wq: Workqueue,
    /// Where stripe_idx_alloc() looks for a free slot first: stripe heads'
    /// allocations race on it, each under its own lock.
    pub stripe_hint: AtomicU64,

    #[pin]
    pub(crate) stripe_delete_work: ReffedWork<delete_empty_stripes, crate::field!(super.writes)>,

    /// The stripe block IO's bios: destroyed with the filesystem, after its
    /// rounds of IO, which wait for them
    #[pin]
    #[init(BioSet::new(1))]
    pub block_bioset: BioSet<EcBio>,

    /// gc's copies of the stripes: trigger.rs's alone - allocated as the gc
    /// triggers find them, from several threads at once.
    pub(crate) gc_stripes: SharedGenRadix<c::gc_stripe>,
}
