// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::locking::{ByteLock, Lock};
use crate::types::{Atomic64, c_default, rust_only};
use crate::util::genradix::SharedGenRadix;
use core::ffi::c_long;
use cstruct_macros::{bitfield, CStruct, TypeInfo};
use nestify::nest;

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct stripe {
    pub heap_idx: usize,
    pub sectors: u16,
    pub algorithm: u8,
    pub nr_blocks: u8,
    pub nr_redundant: u8,
    pub blocks_nonempty: u8,
    pub disk_label: u8,
}

/// gc's copy of a stripe, which check_allocations counts its blocks' sectors
/// into.
#[repr(C)]
#[derive(CStruct)]
pub struct gc_stripe {
    #[c_lock("u8 lock")]
    pub lock: Lock<ByteLock, GcStripeData>,
}

nest! {
    /// What a gc_stripe's lock protects. Not packed, as a bucket's is:
    /// @ptrs are referenced, and there's one of these per stripe, not per
    /// bucket.
    #[repr(C)]
    #[derive(Clone, Copy, Default, CStruct)]
    pub struct GcStripeData {
        #[c_bitfield]
        #>[bitfield(u8, ty = u32)]
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

/* A claim on a stripe index, hashed in bch_fs_ec.stripes_new: */
#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct ec_stripe_handle {
    pub hash: c::hlist_node,
    pub idx: u64,
}
c_default!(ec_stripe_handle);

#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_fs_ec {
    pub stripe_buf_bytes: c_long,
    pub stripe_buf_lock: c::spinlock_t,
    pub stripe_buf_wait: c::closure_waitlist,

    pub stripes_new: [c::hlist_head; 32],
    pub stripes_new_buckets: [c::hlist_head; 64],
    pub stripes_new_lock: c::spinlock_t,

    pub stripe_head_list: c::list_head,
    pub stripe_head_lock: c::mutex,

    pub dev_stripe_state_list: c::list_head,
    pub dev_stripe_state_lock: c::mutex,

    pub stripe_new_list: c::list_head,
    pub stripe_new_lock: c::mutex,
    pub stripe_new_wait: c::wait_queue_head_t,
    #[c("atomic64_t stripe_new_seq")]
    pub stripe_new_seq: Atomic64,

    pub stripe_create_wq: *mut c::workqueue_struct,
    pub stripe_hint: u64,

    pub stripe_delete_work: c::work_struct,

    pub block_bioset: c::bio_set,

    /// gc's copies of the stripes: trigger.rs's alone - allocated as the gc
    /// triggers find them, from several threads at once.
    pub(crate) gc_stripes: rust_only!(SharedGenRadix<c::gc_stripe>),
}
c_default!(bch_fs_ec);
