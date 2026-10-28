// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::fs::Fs;
use crate::util::Printbuf;
use crate::types::{CacheAligned, DArray, c_default};
use crate::util::percpu::{Counters, PerCpu};
use core::ffi::{c_uint, c_ulong};
use core::sync::atomic::{AtomicI32, AtomicI64};
use cstruct_macros::{bitfield, c_const, c_enum, c_extern, c_typedef, c_xmacro, CStruct, Fieldwise, TypeInfo};
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
    #[repr(C)]
    #[derive(CStruct)]
    pub struct open_bucket {
        pub lock: c::spinlock_t,
        pub pin: AtomicI32,
        #[c("open_bucket_idx_t freelist")]
        pub freelist: Option<OpenBucketIdx>,
        #[c("open_bucket_idx_t hash")]
        pub hash: Option<OpenBucketIdx>,

        /*
         * When an open bucket has an ec_stripe attached, this is the index of
         * the block in the stripe this open_bucket corresponds to:
         */
        pub ec_idx: u8,
        #[c_bitfield]
        #>[bitfield(u8)]
        pub bits: pub struct open_bucket_bits {
            #[bits(5)] pub data_type: c::bch_data_type,
            valid:            bool,
            on_partial_list:  bool,
            do_discards_fast: bool,
        },
        /* handle already removed from ca->nr_open_buckets (bucket left free) */
        pub free_uncounted: bool,

        pub dev: u8,
        pub generation: u8,
        /*
         * its own, in c->allocator.open_buckets: the way back to the
         * filesystem. Raw, not an OpenBucketIdx: slot 0, the sentinel, has none.
         */
        pub idx: c::open_bucket_idx_t,
        pub sectors_free: u32,
        pub bucket: u64,
        pub ec: *mut c::ec_stripe_new,
    }
}
c_default!(open_bucket);

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

nest! {
    #[derive(CStruct)]*
    #[repr(C)]*
    pub struct write_point {
        #[c_anon]
        pub alloc: pub struct write_point_alloc {
            pub __align: CacheAligned,
            pub node: c::hlist_node,
            pub lock: c::mutex,
            pub last_used: u64,
            pub write_point: c_ulong,
            pub data_type: c::bch_data_type,

            /* calculated based on how many pointers we're actually going to use: */
            pub sectors_free: c_uint,
            pub prev_sectors_free: c_uint,

            pub ptrs: c::open_buckets,
            pub stripe: c::dev_stripe_state,

            pub sectors_allocated: u64,
        },

        #[c_anon]
        pub io: pub struct write_point_io {
            pub __align: CacheAligned,
            pub index_update_work: c::work_struct,

            pub writes: c::list_head,
            pub writes_lock: c::spinlock_t,

            pub state: c::write_point_state,
            pub last_state_change: u64,
            pub time: [u64; c::WRITE_POINT_STATE_NR as usize],
            pub last_runtime: u64,
        },
    }
}
c_default!(write_point);

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct write_point_specifier {
    pub v: c_ulong,
}

/* Indexed by disk_res_slot(); physical sectors, not data size */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo, Fieldwise)]
#[fieldwise(Counters::wrapping_add(rhs), Counters::clamp_nonnegative)]
pub struct bch_fs_capacity_pcpu {
    pub usage: c::bch_fs_usage_base,
    pub sectors_available: [u64; c::BCH_REPLICAS_MAX as usize],
    pub online_reserved: [u64; c::BCH_REPLICAS_MAX as usize],
}

#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_fs_capacity {
    pub capacity: u64, /* sectors */
    pub reserved: u64, /* sectors */

    /*
     * When capacity _decreases_ (due to a disk being removed), we
     * increment capacity_gen - this invalidates outstanding reservations
     * and forces them to be revalidated
     */
    pub capacity_gen: u32,
    pub bucket_size_max: c_uint,

    pub sectors_available: [AtomicI64; c::BCH_REPLICAS_MAX as usize],
    pub sectors_available_lock: c::spinlock_t,

    /*
     * Bit n - 1 set when free space is lopsided enough that where copies
     * go decides how much fits at n replicas, and the devices every such
     * allocation has to use - see bch2_dev_alloc_required(). Recomputed
     * with the reservation caches; read without locking, it's a placement
     * preference.
     *
     * placement_allowance: physical sectors we can allocate before that
     * could change, and so the most the reservation caches hand out
     * between recomputes.
     */
    pub placement_constrained: c_ulong,
    pub placement_required: [c::bch_devs_mask; c::BCH_REPLICAS_MAX as usize],
    pub placement_allowance: u64,

    #[c("struct bch_fs_capacity_pcpu __percpu *pcpu")]
    pub pcpu: PerCpu<c::bch_fs_capacity_pcpu>,

    pub mark_lock: c::percpu_rwsem_noio,
}
c_default!(bch_fs_capacity);

#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_fs_allocator {
    #[c("struct bch_devs_mask rw_devs[BCH_DATA_NR]")]
    pub rw_devs: [c::AtomicDevsMask; c::BCH_DATA_NR as usize],
    pub rw_devs_change_count: c_ulong,

    /* fs-wide allocator wake generation; see bch2_alloc_wake_all() */
    pub wake_all_counter: AtomicI32,

    pub freelist_lock: c::spinlock_t,
    pub freelist_wait: c::closure_waitlist,
    pub last_stuck: c_ulong,

    #[c("open_bucket_idx_t open_buckets_freelist")]
    pub open_buckets_freelist: Option<OpenBucketIdx>,
    pub open_buckets_nr_free: c::open_bucket_idx_t,
    pub open_buckets_wait: c::closure_waitlist,
    pub open_buckets: [c::open_bucket; c::OPEN_BUCKETS_COUNT as usize],
    #[c("open_bucket_idx_t open_buckets_hash[OPEN_BUCKETS_COUNT]")]
    pub open_buckets_hash: [Option<OpenBucketIdx>; c::OPEN_BUCKETS_COUNT as usize],

    #[c("open_bucket_idx_t open_buckets_partial[OPEN_BUCKETS_COUNT]")]
    pub open_buckets_partial: [Option<OpenBucketIdx>; c::OPEN_BUCKETS_COUNT as usize],
    pub open_buckets_partial_nr: c::open_bucket_idx_t,

    pub write_points: [c::write_point; c::WRITE_POINT_MAX as usize],
    pub write_points_hash: [c::hlist_head; c::WRITE_POINT_HASH_NR as usize],
    pub write_points_hash_lock: c::mutex,
    pub write_points_nr: c_uint,

    pub btree_write_point: c::write_point,
    pub reconcile_write_point: c::write_point,
}
c_default!(bch_fs_allocator);

impl bch_fs_allocator {
    /// How many times @rw_devs has changed - bumped as a device goes
    /// read-write or read-only - read once, as C's READ_ONCE().
    pub fn rw_devs_change_count(&self) -> c_ulong {
        // SAFETY: read once, volatile
        unsafe { core::ptr::read_volatile(&self.rw_devs_change_count) }
    }
}

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
#[c_typedef]
pub struct discard_in_flight {
    pub dev_bucket: u64,
    /*
     * Stashed at add time so completion doesn't have to re-derive ca via
     * c->devs[idx], which dev_remove may have cleared while our io_ref
     * still pins the dev object.
     */
    pub ca: *mut c::bch_dev,
    pub complete: bool,
    pub marking_free: bool,
}
c_default!(discard_in_flight);

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct discard_release {
    pub buffer: u64,
    pub pending_need_flush: u64,
    pub pending_need_rewind_advance: u64,
    pub pending_total: u64,
    pub free: u64,
    pub reserve: u64,
    pub buffer_clamped: u64,
    pub release: i64,
    pub new_rewind_seq: u64,
    pub flush_journal: bool,
    pub flush_wb: bool,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct discard_state {
    pub seen: u64,
    pub not_rw: u64,
    pub eexist: u64,
    pub eagain: u64,
    pub open: u64,
    pub need_journal_commit: u64,
    pub need_rewind_advance: u64,
    pub bad_data_type: u64,
    pub discarded: u64,
    pub committed: u64,
    pub pos: c::bpos,
    pub r: c::discard_release,
}

#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_fs_discards {
    pub work: c::work_struct,
    /// Going read-only: discards for the allocator without a write ref.
    pub going_ro_work: c::work_struct,
    pub going_ro_stopped: bool,
    /// One discard pass at a time.
    pub worker_lock: c::mutex,
    pub bioset: c::bio_set,

    pub lock: c::spinlock_t,
    pub in_flight: DArray<c::discard_in_flight>,
    pub ready: u32,
    pub ref_: u32,
    pub refs: [u8; c::BCH_SB_MEMBERS_MAX as usize],
    pub wait: c::closure_waitlist,

    pub s: c::discard_state,
}
c_default!(bch_fs_discards);

c_extern! {
    pub fn bch2_discards_to_text(out: *mut c::printbuf, c: *mut c::bch_fs, s: *mut c::discard_state);
    pub fn bch2_do_discards_async(c: *mut c::bch_fs);
    pub fn bch2_do_invalidates(c: *mut c::bch_fs);
    pub fn bch2_fast_discards_to_text(out: *mut c::printbuf, ca: *mut c::bch_dev);
}

impl c::bch_dev {
    /// Its discards in flight - the fast path's.
    pub fn fast_discards_to_text(&self, out: &mut Printbuf) {
        unsafe { bch2_fast_discards_to_text(out.as_raw(), core::ptr::from_ref(self).cast_mut()) }
    }
}

impl Fs {
    pub fn discards_to_text(&self, out: &mut Printbuf, s: &c::discard_state) {
        unsafe { bch2_discards_to_text(out.as_raw(), self.as_ptr(), core::ptr::from_ref(s).cast_mut()) }
    }

    /// Kick the discard worker - it takes its own write ref; going
    /// read-only, the going-RO discard work instead.
    pub fn do_discards_async(&self) {
        unsafe { bch2_do_discards_async(self.as_ptr()) }
    }

    /// Kick each device's invalidate worker - which takes its own refs.
    pub fn do_invalidates(&self) {
        unsafe { bch2_do_invalidates(self.as_ptr()) }
    }
}
