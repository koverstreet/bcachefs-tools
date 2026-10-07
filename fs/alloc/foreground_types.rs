// SPDX-License-Identifier: GPL-2.0

//! The data types of alloc/foreground_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::{DArray, c_opaque, c_default};
use cstruct_macros::{bitfield, c_enum, c_verbatim, CStruct};

c_verbatim!(r#"
struct bkey;

struct bch_dev;

struct bch_fs;

struct bch_devs_List;
"#);

c_opaque!(bch_devs_List);

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct dev_alloc_list {
    pub nr: core::ffi::c_uint,
    pub data: [u8; c::BCH_SB_MEMBERS_MAX as usize],
}
c_default!(dev_alloc_list);

#[bitfield(u8)]
pub struct alloc_trace_entry_new_stripe_alloc_bits {
    #[bits(1)]
    pub new_stripe_alloc: bool,
    #[bits(1)]
    pub will_retry_all_devices: bool,
    #[bits(1)]
    pub will_retry_target_devices: bool,
    #[bits(1)]
    pub will_retry_set_devices: bool,
    #[bits(1)]
    pub copygc_can_make_progress: bool,
    #[bits(1)]
    pub have_cl: bool,
    #[bits(2)]
    pub __pad: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
#[c_typedef]
pub struct alloc_trace_entry {
    pub dev: u8,
    #[c_bitfield]
    pub new_stripe_alloc_bits: alloc_trace_entry_new_stripe_alloc_bits,
    pub err: i16,
    pub wake_counter_snapshot: u32,
    pub free_buckets: u64,
}
impl alloc_trace_entry {
    pub fn new_stripe_alloc(&self) -> bool { let b = self.new_stripe_alloc_bits; b.new_stripe_alloc() }
    pub fn set_new_stripe_alloc(&mut self, v: bool) { let mut b = self.new_stripe_alloc_bits; b.set_new_stripe_alloc(v); self.new_stripe_alloc_bits = b; }
    pub fn will_retry_all_devices(&self) -> bool { let b = self.new_stripe_alloc_bits; b.will_retry_all_devices() }
    pub fn set_will_retry_all_devices(&mut self, v: bool) { let mut b = self.new_stripe_alloc_bits; b.set_will_retry_all_devices(v); self.new_stripe_alloc_bits = b; }
    pub fn will_retry_target_devices(&self) -> bool { let b = self.new_stripe_alloc_bits; b.will_retry_target_devices() }
    pub fn set_will_retry_target_devices(&mut self, v: bool) { let mut b = self.new_stripe_alloc_bits; b.set_will_retry_target_devices(v); self.new_stripe_alloc_bits = b; }
    pub fn will_retry_set_devices(&self) -> bool { let b = self.new_stripe_alloc_bits; b.will_retry_set_devices() }
    pub fn set_will_retry_set_devices(&mut self, v: bool) { let mut b = self.new_stripe_alloc_bits; b.set_will_retry_set_devices(v); self.new_stripe_alloc_bits = b; }
    pub fn copygc_can_make_progress(&self) -> bool { let b = self.new_stripe_alloc_bits; b.copygc_can_make_progress() }
    pub fn set_copygc_can_make_progress(&mut self, v: bool) { let mut b = self.new_stripe_alloc_bits; b.set_copygc_can_make_progress(v); self.new_stripe_alloc_bits = b; }
    pub fn have_cl(&self) -> bool { let b = self.new_stripe_alloc_bits; b.have_cl() }
    pub fn set_have_cl(&mut self, v: bool) { let mut b = self.new_stripe_alloc_bits; b.set_have_cl(v); self.new_stripe_alloc_bits = b; }
}

#[bitfield(u8)]
pub struct alloc_request_ec_bits {
    #[bits(1)]
    pub ec: bool,
    #[bits(1)]
    pub new_stripe_alloc: bool,
    #[bits(1)]
    pub will_retry_all_devices: bool,
    #[bits(1)]
    pub will_retry_target_devices: bool,
    #[bits(1)]
    pub will_retry_set_devices: bool,
    #[bits(1)]
    pub copygc_can_make_progress: bool,
    #[bits(1)]
    pub trace_alloc_failed: bool,
    /* failure domains are a hard requirement, not a preference (erasure coding): */
    #[bits(1)]
    pub failure_domains_required: bool,
}

/* alloc_request's counters: C's struct { ... } counters */
#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct alloc_request_counters {
    pub buckets_seen: u64,
    pub skipped_open: u64,
    pub skipped_need_journal_commit: u64,
    pub need_journal_commit: u64,
    pub skipped_nocow: u64,
    pub skipped_nouse: u64,
    pub skipped_mi_btree_bitmap: u64,
}

/* alloc_request's btree_bitmap: C's enum is anonymous, in the field */
c_enum! {
    #[closed]
    pub enum alloc_btree_bitmap: u32 {
        BTREE_BITMAP_NO,
        BTREE_BITMAP_YES,
        BTREE_BITMAP_ANY,
    }
}

#[repr(C)]
#[derive(CStruct)]
pub struct alloc_request {
    pub cl: *mut c::closure,
    pub wake_all_counter_snapshot: u32,
    pub nr_replicas: u8,
    pub ec_replicas: u8,
    pub ec_max_data_blocks: u8, /* 0 = no cap */
    pub target: core::ffi::c_uint,
    #[c_bitfield]
    pub ec_bits: alloc_request_ec_bits,
    pub watermark: c::bch_watermark,
    pub flags: c::bch_write_flags,
    pub data_type: c::bch_data_type,
    pub devs_have: *mut c::bch_devs_list,
    pub wp: *mut c::write_point,

    /* These fields are used primarily by open_bucket_add_buckets */
    pub ptrs: c::open_buckets,
    pub nr_effective: core::ffi::c_uint, /* sum of @ptrs durability */
    pub devs_may_alloc: c::bch_devs_mask,

    /*
     * Devices already holding a replica of what we're allocating for -
     * devs_have plus buckets allocated so far - for spreading replicas
     * across failure domains, see bch2_dev_domain_key():
     */
    pub devs_chosen: c::bch_devs_mask,

    /* devices this allocation should use first, see bch2_dev_alloc_required() */
    pub devs_required: *const c::bch_devs_mask,

    /* bch2_bucket_alloc_set_trans(): */
    pub devs_sorted: c::dev_alloc_list,
    pub domain_keys: [u64; c::BCH_SB_MEMBERS_MAX as usize],
    pub usage: c::bch_dev_usage,

    /* bch2_bucket_alloc_trans(): */
    pub ca: *mut c::bch_dev,

    /*
     * Allocate the free bucket nearest this device position (a 32.32
     * fixed point fraction of the device, see dev_frac_to_offset()),
     * instead of allocating from the device cursor; 0 = no target.
     *
     * A fraction rather than a sector offset so it means the same thing
     * on devices of different sizes: erasure coding uses it to allocate
     * a stripe's blocks at equivalent positions on each device, and the
     * device is chosen after the target is set:
     */
    pub target_frac: u64,

    pub btree_bitmap: c::alloc_btree_bitmap,

    #[c_inline]
    pub counters: alloc_request_counters,

    pub scratch_nr_replicas: core::ffi::c_uint,
    pub scratch_nr_effective: core::ffi::c_uint,
    pub scratch_flags: c::bch_write_flags,
    pub scratch_have_cache: bool,
    pub scratch_data_type: c::bch_data_type,
    pub scratch_ptrs: c::open_buckets,
    pub scratch_devs_may_alloc: c::bch_devs_mask,

    /* Allocation attempt trace — dumped on allocator stuck */
    #[c("DARRAY_PREALLOCATED(alloc_trace_entry, 16) trace")]
    pub trace: DArray<c::alloc_trace_entry, 16>,
}
c_default!(alloc_request);
impl alloc_request {
    pub fn ec(&self) -> bool { let b = self.ec_bits; b.ec() }
    pub fn set_ec(&mut self, v: bool) { let mut b = self.ec_bits; b.set_ec(v); self.ec_bits = b; }
    pub fn new_stripe_alloc(&self) -> bool { let b = self.ec_bits; b.new_stripe_alloc() }
    pub fn set_new_stripe_alloc(&mut self, v: bool) { let mut b = self.ec_bits; b.set_new_stripe_alloc(v); self.ec_bits = b; }
    pub fn will_retry_all_devices(&self) -> bool { let b = self.ec_bits; b.will_retry_all_devices() }
    pub fn set_will_retry_all_devices(&mut self, v: bool) { let mut b = self.ec_bits; b.set_will_retry_all_devices(v); self.ec_bits = b; }
    pub fn will_retry_target_devices(&self) -> bool { let b = self.ec_bits; b.will_retry_target_devices() }
    pub fn set_will_retry_target_devices(&mut self, v: bool) { let mut b = self.ec_bits; b.set_will_retry_target_devices(v); self.ec_bits = b; }
    pub fn will_retry_set_devices(&self) -> bool { let b = self.ec_bits; b.will_retry_set_devices() }
    pub fn set_will_retry_set_devices(&mut self, v: bool) { let mut b = self.ec_bits; b.set_will_retry_set_devices(v); self.ec_bits = b; }
    pub fn copygc_can_make_progress(&self) -> bool { let b = self.ec_bits; b.copygc_can_make_progress() }
    pub fn set_copygc_can_make_progress(&mut self, v: bool) { let mut b = self.ec_bits; b.set_copygc_can_make_progress(v); self.ec_bits = b; }
    pub fn trace_alloc_failed(&self) -> bool { let b = self.ec_bits; b.trace_alloc_failed() }
    pub fn set_trace_alloc_failed(&mut self, v: bool) { let mut b = self.ec_bits; b.set_trace_alloc_failed(v); self.ec_bits = b; }
    pub fn failure_domains_required(&self) -> bool { let b = self.ec_bits; b.failure_domains_required() }
    pub fn set_failure_domains_required(&mut self, v: bool) { let mut b = self.ec_bits; b.set_failure_domains_required(v); self.ec_bits = b; }
}

c_verbatim!(r#"
enum bch_write_flags;
"#);
