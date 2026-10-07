// SPDX-License-Identifier: GPL-2.0

//! The data types of alloc/types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{DArray, c_default};
use cstruct_macros::{bitfield, c_const, c_enum, c_typedef, c_xmacro, CStruct};
use nestify::nest;
use typeinfo_macros::TypeInfo;

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

macro_rules! __bch_watermark_0 {
    ([$($acc:tt)*] $(($name:tt)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[closed]
            pub enum bch_watermark: u32 {
                $($acc)*
                $([<BCH_WATERMARK_ $name>],)*
                BCH_WATERMARK_NR,
            }
        }
    } };
}
BCH_WATERMARKS!(__bch_watermark_0 []);

c_const! {
    #[c_int]
    pub const BCH_WATERMARK_BITS: u32 = 3;
}

c_const! {
    pub const BCH_WATERMARK_MASK: u32 = !(!0 << c::BCH_WATERMARK_BITS);
}

c_const! {
    #[c_int]
    pub const OPEN_BUCKETS_COUNT: u32 = 4096;
}

c_const! {
    #[c_int]
    pub const WRITE_POINT_HASH_NR: u32 = 32;
}

c_const! {
    #[c_int]
    pub const WRITE_POINT_MAX: u32 = 32;
}

/*
 * 0 is never a valid open_bucket_idx_t:
 */
c_typedef! {
    pub type open_bucket_idx_t = u16;
}

#[bitfield(u8)]
pub struct open_bucket_data_type_bits {
    #[bits(5)]
    pub data_type: u32,
    #[bits(1)]
    pub valid: bool,
    #[bits(1)]
    pub on_partial_list: bool,
    #[bits(1)]
    pub do_discards_fast: bool,
}

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct open_bucket {
    pub lock: c::spinlock_t,
    pub pin: c::atomic_t,
    pub freelist: c::open_bucket_idx_t,
    pub hash: c::open_bucket_idx_t,

    /*
     * When an open bucket has an ec_stripe attached, this is the index of
     * the block in the stripe this open_bucket corresponds to:
     */
    pub ec_idx: u8,
    #[c_bitfield]
    pub data_type_bits: open_bucket_data_type_bits,
    /* handle already removed from ca->nr_open_buckets (bucket left free) */
    pub free_uncounted: bool,

    pub dev: u8,
    pub generation: u8,
    pub sectors_free: u32,
    pub bucket: u64,
    pub ec: *mut c::ec_stripe_new,
}
c_default!(open_bucket);
impl open_bucket {
    pub fn data_type(&self) -> u32 { let b = self.data_type_bits; b.data_type() }
    pub fn set_data_type(&mut self, v: u32) { let mut b = self.data_type_bits; b.set_data_type(v); self.data_type_bits = b; }
    pub fn valid(&self) -> bool { let b = self.data_type_bits; b.valid() }
    pub fn set_valid(&mut self, v: bool) { let mut b = self.data_type_bits; b.set_valid(v); self.data_type_bits = b; }
    pub fn on_partial_list(&self) -> bool { let b = self.data_type_bits; b.on_partial_list() }
    pub fn set_on_partial_list(&mut self, v: bool) { let mut b = self.data_type_bits; b.set_on_partial_list(v); self.data_type_bits = b; }
    pub fn do_discards_fast(&self) -> bool { let b = self.data_type_bits; b.do_discards_fast() }
    pub fn set_do_discards_fast(&mut self, v: bool) { let mut b = self.data_type_bits; b.set_do_discards_fast(v); self.data_type_bits = b; }
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct open_buckets {
    pub nr: c::open_bucket_idx_t,
    pub v: [c::open_bucket_idx_t; c::BCH_BKEY_PTRS_MAX as usize],
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

macro_rules! __write_point_state_0 {
    ([$($acc:tt)*] $(($n:tt)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[closed]
            pub enum write_point_state: u32 {
                $($acc)*
                $([<WRITE_POINT_ $n>],)*
                WRITE_POINT_STATE_NR,
            }
        }
    } };
}
WRITE_POINT_STATES!(__write_point_state_0 []);

nest! {
    #[derive(Clone, Copy, CStruct)]*
    #[repr(C)]*
    pub struct write_point {
        #[c_anon]
        #>[c_align("SMP_CACHE_BYTES")]
        pub alloc: pub struct write_point_alloc {
            #[c_anon("")] pub __align: [crate::types::CacheAligned; 0],
            pub node: c::hlist_node,
            pub lock: c::mutex,
            pub last_used: u64,
            pub write_point: core::ffi::c_ulong,
            pub data_type: c::bch_data_type,

            /* calculated based on how many pointers we're actually going to use: */
            pub sectors_free: core::ffi::c_uint,
            pub prev_sectors_free: core::ffi::c_uint,

            pub ptrs: c::open_buckets,
            pub stripe: c::dev_stripe_state,

            pub sectors_allocated: u64,
        },

        #[c_anon]
        #>[c_align("SMP_CACHE_BYTES")]
        pub io: pub struct write_point_io {
            #[c_anon("")] pub __align: [crate::types::CacheAligned; 0],
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
    pub v: core::ffi::c_ulong,
}

/* Indexed by disk_res_slot(); physical sectors, not data size */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_fs_capacity_pcpu {
    pub usage: c::bch_fs_usage_base,
    pub sectors_available: [u64; c::BCH_REPLICAS_MAX as usize],
    pub online_reserved: [u64; c::BCH_REPLICAS_MAX as usize],
}

#[repr(C)]
#[derive(Clone, Copy, CStruct, TypeInfo)]
pub struct bch_fs_capacity {
    pub capacity: u64, /* sectors */
    pub reserved: u64, /* sectors */

    /*
     * When capacity _decreases_ (due to a disk being removed), we
     * increment capacity_gen - this invalidates outstanding reservations
     * and forces them to be revalidated
     */
    pub capacity_gen: u32,
    pub bucket_size_max: core::ffi::c_uint,

    pub sectors_available: [c::atomic64_t; c::BCH_REPLICAS_MAX as usize],
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
    pub placement_constrained: core::ffi::c_ulong,
    pub placement_required: [c::bch_devs_mask; c::BCH_REPLICAS_MAX as usize],
    pub placement_allowance: u64,

    #[c("struct bch_fs_capacity_pcpu __percpu *pcpu")]
    pub pcpu: *mut c::bch_fs_capacity_pcpu,

    pub mark_lock: c::percpu_rwsem_noio,
}
c_default!(bch_fs_capacity);

#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_fs_allocator {
    pub rw_devs: [c::bch_devs_mask; c::BCH_DATA_NR as usize],
    pub rw_devs_change_count: core::ffi::c_ulong,

    /* fs-wide allocator wake generation; see bch2_alloc_wake_all() */
    pub wake_all_counter: c::atomic_t,

    pub freelist_lock: c::spinlock_t,
    pub freelist_wait: c::closure_waitlist,
    pub last_stuck: core::ffi::c_ulong,

    pub open_buckets_freelist: c::open_bucket_idx_t,
    pub open_buckets_nr_free: c::open_bucket_idx_t,
    pub open_buckets_wait: c::closure_waitlist,
    pub open_buckets: [c::open_bucket; c::OPEN_BUCKETS_COUNT as usize],
    pub open_buckets_hash: [c::open_bucket_idx_t; c::OPEN_BUCKETS_COUNT as usize],

    pub open_buckets_partial: [c::open_bucket_idx_t; c::OPEN_BUCKETS_COUNT as usize],
    pub open_buckets_partial_nr: c::open_bucket_idx_t,

    pub write_points: [c::write_point; c::WRITE_POINT_MAX as usize],
    pub write_points_hash: [c::hlist_head; c::WRITE_POINT_HASH_NR as usize],
    pub write_points_hash_lock: c::mutex,
    pub write_points_nr: core::ffi::c_uint,

    pub btree_write_point: c::write_point,
    pub reconcile_write_point: c::write_point,
}
c_default!(bch_fs_allocator);

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
    pub bioset: c::bio_set,

    pub lock: c::spinlock_t,
    #[c("DARRAY(discard_in_flight) in_flight")]
    pub in_flight: DArray<c::discard_in_flight>,
    pub ready: u32,
    pub ref_: u32,
    pub refs: [u8; c::BCH_SB_MEMBERS_MAX as usize],
    pub wait: c::closure_waitlist,

    pub s: c::discard_state,
}
c_default!(bch_fs_discards);
