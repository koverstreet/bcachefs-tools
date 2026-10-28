// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::fs::Fs;
use crate::alloc::discard::{do_discards_work, DiscardBio, DiscardState};
use crate::util::alloc::KVVec;
use crate::locking::{Lock, SeqLock};
use crate::new_lock;
use crate::types::{CacheAligned, RustOnly, c_default, rust_only};
use crate::util::bio::BioSet;
use crate::util::workqueue::ReffedWork;
use crate::util::percpu::{Counters, PerCpu};
use crate::util::projections::Volatile;
use crate::published;
use core::ffi::{c_uint, c_ulong};
use core::sync::atomic::{AtomicI32, AtomicPtr, AtomicU64};
use cstruct_macros::{bitfield, c_const, c_enum, c_typedef, c_xmacro, placed, CStruct, Fieldwise, Init, TypeInfo};
use pin_init::pin_data;
use nestify::nest;

c_xmacro! {
    BCH_WATERMARKS(x) {
        (stripe),
        (normal),
        (copygc),
        (btree),
        (btree_copygc),
        (reclaim),
        (interior_updates),
    }
}

c_enum! {
    #[closed]
    pub enum bch_watermark: u32 {
        BCH_WATERMARKS(name) => BCH_WATERMARK_ ## name,
        BCH_WATERMARK_NR,
    }
}

c_const! {
    #[c_int]
    pub const BCH_WATERMARK_BITS: u32 = 3;
    pub const BCH_WATERMARK_MASK: u32 = !(!0 << c::BCH_WATERMARK_BITS);

    #[c_int]
    pub const OPEN_BUCKETS_COUNT: u32 = 4096;

    #[c_int]
    pub const WRITE_POINT_HASH_NR: u32 = 32;
    #[c_int]
    pub const WRITE_POINT_MAX: u32 = 32;
}

/*
 * 0 is never a valid open_bucket_idx_t:
 */
c_typedef! {
    pub type open_bucket_idx_t = u16;
}

/// An open bucket's index in c->allocator.open_buckets - never 0, slot 0
/// being the sentinel - so a field C has as an open_bucket_idx_t, 0 for none,
/// is an Option<OpenBucketIdx>, laid out the same. A count stays an
/// open_bucket_idx_t.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct OpenBucketIdx(core::num::NonZero<c::open_bucket_idx_t>);

// SAFETY: None is all zeroes, as an Option of a NonZero is
unsafe impl pin_init::ZeroableOption for OpenBucketIdx {}

// SAFETY: a u16, any of which is an Option of one - 0 None
unsafe impl crate::util::vstructs::Plain for Option<OpenBucketIdx> {}
crate::tear_safe!(Option<OpenBucketIdx>);

impl OpenBucketIdx {
    /// # Safety
    /// @idx isn't 0.
    pub unsafe fn new_unchecked(idx: c::open_bucket_idx_t) -> Self {
        // SAFETY: the caller's
        OpenBucketIdx(unsafe { core::num::NonZero::new_unchecked(idx) })
    }

    pub fn get(self) -> usize {
        self.0.get().into()
    }
}

// The OpenBucketIdx we want is linear (util/linear.rs): an index that holds
// a pin, which can't be copied or dropped - only consumed, into an OpenBucket
// with the filesystem, or into a list that keeps the pin. Then every holder
// of one - a write point's list, the partial list, a btree update, a stripe's
// blocks - is checked for leaks, and OpenBucket::from_idx() needs no unsafe:
// the index is the proof of the pin.
//
//     #[repr(transparent)]
//     #[derive(PartialEq, Eq, Debug, Linear)]
//     pub struct OpenBucketIdx(core::num::NonZero<c::open_bucket_idx_t>);
//
// Tried 2026-10-26: it compiles with small changes, and doesn't link. What it
// needs:
//
// - A second, Copy type for the indices that are links, not pins:
//   open_bucket's freelist and hash, and the allocator's freelist and hash
//   chains. Only open_buckets.v, open_buckets_partial[] and a btree update's
//   open_buckets[] hold pins.
// - Filesystem teardown that consumes them: the lists that hold pins are in
//   C-shared structs inside Fs - the allocator, btree, journal, copygc - and
//   Fs's drop glue reaches every one.
// - No linear value held across an .await: a future's drop glue for being
//   cancelled there drops it.
//
// The last two are past what a link-time check can see. A verifier that
// checks linearity - Aeneas, Verus - could follow ownership through
// teardown and through futures; that's when to bring it back.

nest! {
    /// A slot of c->allocator.open_buckets: a bucket open to be written, or
    /// free. Open from try_alloc_bucket_locked() - @pin 1 - until its last pin
    /// is put; a pin holder reads what's fixed while it's open without a lock.
    ///
    /// What protects what:
    ///
    /// - @pin: atomic. The last put closes it - __bch2_open_bucket_put() -
    ///   unless @ec is set: then it stays open for that stripe, which closes it.
    /// - @lock: held to open it - @valid, @dev, @generation, @bucket,
    ///   @sectors_free and @free_uncounted set - and to close it, @valid and
    ///   @data_type cleared. Taken by what looks at slots it holds no pin on,
    ///   to see @valid, @dev and @bucket together: bch2_dev_has_open_write_point(),
    ///   bch2_open_buckets_to_text(), and bch2_bucket_set_discard_fast(),
    ///   which sets @do_discards_fast under it; the close reads and clears that
    ///   without it.
    /// - @dev, @generation, @bucket: fixed from the open to the close.
    /// - @idx: fixed from when the table's built.
    /// - @sectors_free: set at the open, then its writer's - written under the
    ///   lock of the write point that has it: bch2_alloc_sectors_req()'s
    ///   alignment, bch2_alloc_sectors_append_ptrs_inlined(),
    ///   bch2_alloc_sectors_retire_short(). Fixed once the writer's pin is put.
    /// - @data_type: its holder's - set when it's handed out, and by
    ///   bch2_open_bucket_get() under the write point's lock; cleared at the
    ///   close.
    /// - @freelist, @on_partial_list, @hash: the allocator's, under
    ///   freelist_lock - @hash also read without it, by bch2_bucket_is_open(),
    ///   and @on_partial_list by bch2_dev_has_open_write_point(), under @lock.
    /// - @free_uncounted: set at the open; then under freelist_lock.
    /// - @ec, @ec_idx: set when a stripe being made has a writer claim the
    ///   bucket as one of its blocks - OpenBucket::claim(), under the stripe
    ///   head's lock and the stripe's - before the writer has it; @ec cleared
    ///   when the stripe closes it, EcOpenBucket's drop. Read by its pin
    ///   holders, and by bch2_open_bucket_to_text() under @lock.
    ///
    /// Its index in the table - idx, the way back to the filesystem - is its
    /// place's: placed last, the fields before it ordered so that it fills
    /// what would be tail padding. Raw, not an OpenBucketIdx: slot 0, the
    /// sentinel, has one too.
    #[placed(bch_fs_allocator.open_buckets[], index = idx: c::open_bucket_idx_t)]
    #[pin_data]
    #[repr(C)]
    #[derive(CStruct, Init)]
    pub struct open_bucket {
        pub lock: c::spinlock_t,
        pub pin: AtomicI32,
        /* handle already removed from ca->nr_open_buckets (bucket left free):
         * under freelist_lock */
        pub free_uncounted: Volatile<bool>,
        pub dev: Volatile<u8>,
        pub generation: Volatile<u8>,
        pub sectors_free: Volatile<u32>,
        pub bucket: Volatile<u64>,
        #[c("struct ec_stripe_new *ec")]
        pub ec: AtomicPtr<c::ec_stripe_new>,

        #[c("open_bucket_idx_t freelist")]
        pub freelist: Volatile<Option<OpenBucketIdx>>,
        #[c("open_bucket_idx_t hash")]
        pub hash: Volatile<Option<OpenBucketIdx>>,

        /*
         * When an open bucket has an ec_stripe attached, this is the index of
         * the block in the stripe this open_bucket corresponds to:
         */
        pub ec_idx: Volatile<u8>,
        #[c_bitfield]
        #>[bitfield(u8)]
        pub bits: pub struct open_bucket_bits {
            #[bits(5)] pub data_type: c::bch_data_type,
            pub valid:            bool,
            pub on_partial_list:  bool,
            pub do_discards_fast: bool,
        },
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct open_buckets {
    pub nr: c::open_bucket_idx_t,
    #[c("open_bucket_idx_t v[BCH_BKEY_PTRS_MAX]")]
    pub v: [Option<OpenBucketIdx>; c::BCH_BKEY_PTRS_MAX as usize],
}

/*
 * Per-(write_point, target) WFQ state: next_alloc[i] is the virtual time at
 * which device i should next be served. The smallest hand wins; the per-pick
 * increment is 1/free_space[i], so devs with more free space win more often -
 * the proportional bias is in the increment size.
 *
 * cached_devs records which dev mask the next_alloc values are valid for.
 * On mask change (target switch, rw_devs change, write_point recycled to a
 * different stream) we bump newly-included devs to min(next_alloc[i] of devs
 * that were already in scope), so they join at "current virtual time" rather
 * than at 0 - which would otherwise win every comparison until catching up
 * to the rest of the hands, starving the other devs in the meantime.
 */
#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct dev_stripe_state {
    pub next_alloc: [u64; c::BCH_SB_MEMBERS_MAX as usize],
    pub cached_devs: c::bch_devs_mask,
}
c_default!(dev_stripe_state);

c_xmacro! {
    WRITE_POINT_STATES(x) {
        (stopped),
        (waiting_io),
        (waiting_work),
        (runnable),
        (running),
    }
}

c_enum! {
    #[closed]
    pub enum write_point_state: u32 {
        WRITE_POINT_STATES(n) => WRITE_POINT_ ## n,
        WRITE_POINT_STATE_NR,
    }
}

/* the write point lock's: what a write allocating from it uses */
#[repr(C)]
#[derive(CStruct)]
pub struct WritePointLocked {
    /* calculated based on how many pointers we're actually going to use: */
    pub sectors_free: c_uint,
    pub prev_sectors_free: c_uint,

    pub ptrs: c::open_buckets,
    pub stripe: c::dev_stripe_state,

    pub sectors_allocated: u64,
}

published! {
    /// Which write point it is, and when it was last used: written under its
    /// lock - and the identity under write_points_hash_lock too - and read
    /// without either, to find one or the one to reuse.
    #[derive(CStruct)]
    pub struct WritePointId {
        pub last_used: Volatile<u64>,
        pub write_point: Volatile<c_ulong>,
    }
}

#[pin_data]
#[repr(C)]
#[derive(CStruct, Init)]
pub struct write_point_alloc {
    pub __align: CacheAligned,
    pub node: c::hlist_node,
    pub data_type: c::bch_data_type,
    #[pin]
    #[init(new_lock!(<- ::pin_init::init_zeroed(), ::pin_init::zeroed(), "&wp->lock"))]
    #[c_lock("struct mutex lock")]
    pub lock: Lock<c::mutex, WritePointLocked, WritePointId>,
}

#[pin_data]
#[repr(C)]
#[derive(CStruct, Init)]
pub struct write_point {
    #[c_anon]
    #[pin]
    #[init(write_point_alloc::init())]
    pub alloc: write_point_alloc,
    #[c_anon]
    pub io: write_point_io,
}

#[repr(C)]
#[derive(CStruct)]
pub struct write_point_io {
    pub __align: CacheAligned,
    pub index_update_work: c::work_struct,

    pub writes: c::list_head,
    pub writes_lock: c::spinlock_t,

    pub state: c::write_point_state,
    pub last_state_change: u64,
    pub time: [u64; c::WRITE_POINT_STATE_NR as usize],
    pub last_runtime: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct write_point_specifier {
    pub v: c_ulong,
}

published! {
    /// Where copies have to go, when free space is lopsided enough that it
    /// decides how much fits: recomputed with the reservation caches - from
    /// what it was, for hysteresis - and read without the lock by the
    /// allocator, as a placement preference - see bch2_dev_alloc_required().
    #[derive(CStruct)]
    pub struct Placement {
        /* bit n - 1: placement decides how much fits at n replicas */
        pub placement_constrained: Volatile<c_ulong>,
        /* at n replicas, the devices every such allocation has to use */
        #[c("struct bch_devs_mask placement_required[BCH_REPLICAS_MAX]")]
        pub placement_required: [Volatile<c::DevsMask>; c::BCH_REPLICAS_MAX as usize],
    }
}

/* capacity.pcpu's counters: each CPU's a delta, summed over them */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo, Fieldwise)]
#[fieldwise(Counters::wrapping_add(rhs), Counters::clamp_nonnegative)]
pub struct bch_fs_capacity_counters {
    pub usage: c::bch_fs_usage_base,
    /* Indexed by disk_res_slot(); physical sectors, not data size */
    pub online_reserved: [u64; c::BCH_REPLICAS_MAX as usize],
}

#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_fs_capacity_pcpu {
    #[c_anon]
    pub counters: bch_fs_capacity_counters,
    /*
     * The reservation caches, by disk_res_slot(): slots, each changed by
     * its own CPU and reset by a recompute's
     */
    pub sectors_available: [Volatile<u64>; c::BCH_REPLICAS_MAX as usize],
}

/// See alloc/capacity.rs.
#[pin_data(PinnedDrop)]
#[repr(C)]
#[derive(CStruct, TypeInfo, Init)]
pub struct bch_fs_capacity {
    /* sectors: recalc_capacity()'s, under state_lock, read without it */
    pub capacity: crate::util::projections::Volatile<u64>,
    pub reserved: crate::util::projections::Volatile<u64>,

    /*
     * When capacity _decreases_ (due to a disk being removed), we
     * increment capacity_gen - this invalidates outstanding reservations
     * and forces them to be revalidated
     */
    pub capacity_gen: u32,
    pub bucket_size_max: crate::util::projections::Volatile<c_uint>,

    pub sectors_available: [AtomicU64; c::BCH_REPLICAS_MAX as usize],

    /* the reservation caches' recompute, and the placement it decides */
    #[pin]
    #[init(new_lock!((), ::pin_init::zeroed(), "&c->capacity.sectors_available_lock"))]
    #[c_lock("spinlock_t sectors_available_lock")]
    pub sectors_available_lock: Lock<c::spinlock_t, (), Placement>,

    #[c("struct bch_fs_capacity_pcpu __percpu *pcpu")]
    #[init(PerCpu::init())]
    pub pcpu: PerCpu<c::bch_fs_capacity_pcpu>,

    #[pin]
    #[init(new_lock!((), "&c->capacity.mark_lock"))]
    #[c("struct percpu_rwsem_noio mark_lock")]
    pub mark_lock: crate::locking::PercpuRwsemNoio,
}

#[placed(Fs.allocator)]
#[pin_data]
#[repr(C)]
#[derive(CStruct, TypeInfo, Init)]
pub struct bch_fs_allocator {
    #[c("struct bch_devs_mask rw_devs[BCH_DATA_NR]")]
    pub rw_devs: [c::AtomicDevsMask; c::BCH_DATA_NR as usize],
    /* bumped as a device goes read-write or read-only, under state_lock;
     * read without it */
    pub rw_devs_change_count: crate::util::projections::Volatile<c_ulong>,

    /* fs-wide allocator wake generation; see bch2_alloc_wake_all() */
    pub wake_all_counter: AtomicI32,

    /* the open buckets not in use, and the partial list */
    #[pin]
    #[init(new_lock!(<- ::pin_init::init_zeroed(), ::pin_init::zeroed(), "&c->allocator.freelist_lock"))]
    #[c_lock("spinlock_t freelist_lock")]
    pub freelist_lock: Lock<c::spinlock_t, OpenBucketsFree, OpenBucketsFreeNr>,
    pub freelist_wait: c::closure_waitlist,
    /* jiffies at the last allocator stuck dump; claimed by cmpxchg */
    pub last_stuck: crate::util::bitmap::AtomicULong,

    pub open_buckets_wait: c::closure_waitlist,
    #[pin]
    pub open_buckets: [c::open_bucket; c::OPEN_BUCKETS_COUNT as usize],
    #[c("open_bucket_idx_t open_buckets_hash[OPEN_BUCKETS_COUNT]")]
    pub open_buckets_hash: [Volatile<Option<OpenBucketIdx>>; c::OPEN_BUCKETS_COUNT as usize],

    #[pin]
    #[init(::pin_init::pin_init_array_from_fn(|_| c::write_point::init()))]
    pub write_points: [c::write_point; c::WRITE_POINT_MAX as usize],
    pub write_points_hash: [c::hlist_head; c::WRITE_POINT_HASH_NR as usize],
    /* the hash's writers - its readers are RCU's - and write_points_nr */
    #[pin]
    #[init(new_lock!((), ::pin_init::zeroed(), "&c->allocator.write_points_hash_lock"))]
    #[c_lock("struct mutex write_points_hash_lock")]
    pub write_points_hash_lock: Lock<c::mutex, (), WritePointsNr>,

    #[pin]
    #[init(c::write_point::init())]
    pub btree_write_point: c::write_point,
    #[pin]
    #[init(c::write_point::init())]
    pub reconcile_write_point: c::write_point,
}

/* freelist_lock's: the open buckets not in use, and the partial list */
#[repr(C)]
#[derive(CStruct)]
pub struct OpenBucketsFree {
    #[c("open_bucket_idx_t open_buckets_freelist")]
    pub open_buckets_freelist: Option<OpenBucketIdx>,
    #[c("open_bucket_idx_t open_buckets_partial[OPEN_BUCKETS_COUNT]")]
    pub open_buckets_partial: [Option<OpenBucketIdx>; c::OPEN_BUCKETS_COUNT as usize],
}

published! {
    /// How many there are of each, read without freelist_lock: whether
    /// there's an open bucket to be had, whether the partial list is worth
    /// taking the lock for.
    #[derive(CStruct)]
    pub struct OpenBucketsFreeNr {
        pub open_buckets_nr_free: Volatile<c::open_bucket_idx_t>,
        pub open_buckets_partial_nr: Volatile<c::open_bucket_idx_t>,
    }
}

published! {
    /// The write points in use, of write_points[], read without
    /// write_points_hash_lock.
    #[derive(CStruct)]
    pub struct WritePointsNr {
        pub write_points_nr: Volatile<c_uint>,
    }
}

/// The discard worker's: its work, its bios, the discards in flight - see
/// alloc/discard.rs.
#[placed(Fs.discards)]
#[pin_data]
#[repr(C)]
#[derive(CStruct, TypeInfo, Init)]
pub struct bch_fs_discards {
    #[c("struct work_struct work")]
    #[pin]
    pub(crate) work: ReffedWork<do_discards_work, crate::field!(super.writes)>,
    #[c("struct bio_set bioset")]
    #[pin]
    #[init(BioSet::new(4, false))]
    pub(crate) bioset: BioSet<DiscardBio>,

    /// Buckets with a discard issued, by any worker, not yet marked free.
    #[pin]
    #[init(RustOnly::new(new_lock!(KVVec::new())))]
    pub(crate) in_flight: rust_only!(Lock<c::mutex, KVVec<u64>>),

    /// The worker's progress, for sysfs and the allocator's stuck report.
    #[pin]
    #[init(RustOnly::new(SeqLock::new(DiscardState::default())))]
    pub(crate) s: rust_only!(SeqLock<DiscardState>),
}
