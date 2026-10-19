// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::fs::Fs;
use crate::types::{DArray, c_default};
use crate::util::Printbuf;
use core::ffi::c_uint;
use cstruct_macros::{bitfield, c_enum, c_extern, CStruct};
use nestify::nest;

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct dev_alloc_list {
    pub nr: c_uint,
    pub data: [u8; c::BCH_SB_MEMBERS_MAX as usize],
}
c_default!(dev_alloc_list);

nest! {
    #[repr(C)]
    #[derive(Clone, Copy, Default, CStruct)]
    #[c_typedef]
    pub struct alloc_trace_entry {
        pub dev: u8,
        #[c_bitfield]
        #>[bitfield(u8)]
        pub bits: pub struct alloc_trace_entry_bits {
            new_stripe_alloc:          bool,
            will_retry_all_devices:    bool,
            will_retry_target_devices: bool,
            will_retry_set_devices:    bool,
            copygc_can_make_progress:  bool,
            have_cl:                   bool,
            __pad:                     u2,
        },
        pub err: i16,
        pub wake_counter_snapshot: u32,
        pub free_buckets: u64,
    }
}

/*
 * alloc_request.btree_bitmap: whether the bucket must be in a region the
 * device's btree bitmap marks (YES), must not be (NO), or may be either (ANY)
 */
c_enum! {
    #[closed]
    pub enum alloc_btree_bitmap: u32 {
        BTREE_BITMAP_NO,
        BTREE_BITMAP_YES,
        BTREE_BITMAP_ANY,
    }
}

nest! {
    #[derive(CStruct)]*
    #[repr(C)]*
    pub struct alloc_request {
        pub cl: *mut c::closure,
        pub wake_all_counter_snapshot: u32,
        pub nr_replicas: u8,
        pub ec_replicas: u8,
        pub ec_max_data_blocks: u8, /* 0 = no cap */
        pub target: c_uint,
        #[c_bitfield]
        #>[bitfield(u8)]
        #>[derive(CStruct)]-
        #>[repr(C)]-
        pub bits: pub struct alloc_request_bits {
            ec:                        bool,
            new_stripe_alloc:          bool,
            will_retry_all_devices:    bool,
            will_retry_target_devices: bool,
            will_retry_set_devices:    bool,
            copygc_can_make_progress:  bool,
            trace_alloc_failed:        bool,
            /* failure domains are a hard requirement, not a preference (erasure coding): */
            failure_domains_required:  bool,
        },
        pub watermark: c::bch_watermark,
        pub flags: c::bch_write_flags,
        pub data_type: c::bch_data_type,
        pub devs_have: *mut c::bch_devs_list,
        pub wp: *mut c::write_point,

        /* These fields are used primarily by open_bucket_add_buckets */
        pub ptrs: c::open_buckets,
        pub nr_effective: c_uint, /* sum of @ptrs durability */
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
        #>[derive(Clone, Copy)]
        pub counters: pub struct alloc_request_counters {
            pub buckets_seen: u64,
            pub skipped_open: u64,
            pub skipped_need_journal_commit: u64,
            pub need_journal_commit: u64,
            pub skipped_nocow: u64,
            pub skipped_nouse: u64,
            pub skipped_mi_btree_bitmap: u64,
        },

        pub scratch_nr_replicas: c_uint,
        pub scratch_nr_effective: c_uint,
        pub scratch_flags: c::bch_write_flags,
        pub scratch_have_cache: bool,
        pub scratch_data_type: c::bch_data_type,
        pub scratch_ptrs: c::open_buckets,
        pub scratch_devs_may_alloc: c::bch_devs_mask,

        /* Allocation attempt trace — dumped on allocator stuck */
        pub trace: DArray<c::alloc_trace_entry, 16>,
    }
}
c_default!(alloc_request);

c_extern! {
    pub fn bch2_alloc_wake_all(c: *mut c::bch_fs);
    pub fn bch2_open_buckets_to_text(out: *mut c::printbuf, c: *mut c::bch_fs, ca: *mut c::bch_dev);
    pub fn bch2_open_buckets_partial_to_text(out: *mut c::printbuf, c: *mut c::bch_fs);
    pub fn bch2_write_points_to_text(out: *mut c::printbuf, c: *mut c::bch_fs);
    pub fn bch2_fs_open_buckets_to_text(out: *mut c::printbuf, c: *mut c::bch_fs);
    pub fn bch2_fs_alloc_debug_to_text(out: *mut c::printbuf, c: *mut c::bch_fs);
    pub fn bch2_dev_alloc_debug_to_text(out: *mut c::printbuf, ca: *mut c::bch_dev);
}

impl c::bch_dev {
    pub fn alloc_debug_to_text(&self, out: &mut Printbuf) {
        unsafe { bch2_dev_alloc_debug_to_text(out.as_raw(), core::ptr::from_ref(self).cast_mut()) }
    }
}

impl Fs {
    /// Wake everything waiting on the allocator.
    pub fn alloc_wake_all(&self) {
        unsafe { bch2_alloc_wake_all(self.as_ptr()) }
    }

    /// The open buckets - on @ca, or all of them.
    pub fn open_buckets_to_text(&self, out: &mut Printbuf, ca: Option<&c::bch_dev>) {
        unsafe { bch2_open_buckets_to_text(out.as_raw(), self.as_ptr(), crate::sb::members::dev_ptr(ca)) }
    }

    pub fn open_buckets_partial_to_text(&self, out: &mut Printbuf) {
        unsafe { bch2_open_buckets_partial_to_text(out.as_raw(), self.as_ptr()) }
    }

    pub fn fs_open_buckets_to_text(&self, out: &mut Printbuf) {
        unsafe { bch2_fs_open_buckets_to_text(out.as_raw(), self.as_ptr()) }
    }

    pub fn alloc_debug_to_text(&self, out: &mut Printbuf) {
        unsafe { bch2_fs_alloc_debug_to_text(out.as_raw(), self.as_ptr()) }
    }
}
