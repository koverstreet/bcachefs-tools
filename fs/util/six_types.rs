// SPDX-License-Identifier: GPL-2.0

//! The data types of util/six_types.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::types::c_default;
use cstruct_macros::{c_const, c_enum, c_typedef, CStruct};

c_enum! {
    #[closed]
    pub enum six_lock_type: u32 {
        SIX_LOCK_read,
        SIX_LOCK_intent,
        SIX_LOCK_write,
    }
}

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct six_lock_waiter {
    pub trans_start_time: u64,
    pub task: *mut c::task_struct,
    pub lock_want: c::six_lock_type,
    pub lock_acquired: bool,
    /* Index in wait_fifo->data[], set on insert, used for O(1) self-remove. */
    pub slot_idx: u16,
}
c_default!(six_lock_waiter);

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct six_lock_wait_slot {
    pub w: *mut c::six_lock_waiter,
    pub start_time: u64,
}
c_default!(six_lock_wait_slot);

/*
 * RCU-swappable wait list. Not a FIFO — entries sit at fixed indices from
 * insertion to removal, so RCU readers (the cycle detector) never observe an
 * entry moving. Removal writes NULL atomically; insertion finds a free slot
 * (starting from @next_free_hint) and fills it. @nr is the high-water-mark
 * index in use and shrinks when trailing slots go tombstone.
 *
 * Grown by allocating a new struct, copying fields + entries, then
 * rcu_assign_pointer'ing the lock's wait_fifo to the new object. Old
 * heap allocations are kvfree_rcu'd so future lockless readers can
 * outrun the free.
 */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct six_lock_wait_fifo {
    pub size: u16,
    pub nr: u16,
    pub next_free_hint: u16,
    pub data: [c::six_lock_wait_slot; 0],
}

c_const! {
    #[c_int]
    pub const SIX_LOCK_INLINE_WAITERS: u32 = 8;
}

#[repr(C)]
#[derive(CStruct)]
pub struct six_lock {
    pub state: c::atomic_t,
    pub seq: u32,
    #[c("unsigned __percpu *readers")]
    pub readers: *mut core::ffi::c_uint,
    pub intent_lock_recurse: core::ffi::c_uint,
    pub write_lock_recurse: core::ffi::c_uint,
    pub owner: *mut c::task_struct,
    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    pub owner_stack: c::bch_stacktrace,
    pub wait_lock: c::raw_spinlock_t,

    #[c("struct six_lock_wait_fifo __rcu *wait_fifo")]
    pub wait_fifo: *mut c::six_lock_wait_fifo,

    pub inline_fifo: c::six_lock_wait_fifo,
    pub inline_fifo_data: [c::six_lock_wait_slot; c::SIX_LOCK_INLINE_WAITERS as usize],
    #[cfg(CONFIG_DEBUG_LOCK_ALLOC)]
    pub dep_map: c::lockdep_map,
}
c_default!(six_lock);

c_typedef! {
    pub type six_lock_should_sleep_fn = Option<unsafe extern "C" fn(lock: *mut c::six_lock,
                                                                    *mut c::six_lock_waiter) -> core::ffi::c_int>;
}

c_enum! {
    #[flags]
    pub enum six_lock_init_flags: u32 {
        SIX_LOCK_INIT_PCPU = 1 << 0,
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct six_lock_count {
    pub n: [core::ffi::c_uint; 3],
}
