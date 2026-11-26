// SPDX-License-Identifier: GPL-2.0

//! The faults-disabled-mapping table: the DIO write path's side, which sets a
//! task's mapping while it pins user pages, and the page fault handler's,
//! which reads it.
//!
//! Replaces the per-task faults_disabled_mapping field (removed in 7.0)
//! with a fixed-size cuckoo hash in bch_fs. Used to prevent deadlock
//! when the page fault handler is invoked for a mapping that is already
//! being written to by the current task (DIO path).
//!
//! Properties:
//!  - O(1) lookup: exactly three slots to check (3-way cuckoo)
//!  - No locks: each task only writes/removes its own entry
//!  - No resize: table is oversized at init, occupancy stays very low
//!  - Low bit of mapping pointer stolen for dropped_locks signaling
//!  - Waits on the table's EventCount if all slots full (just a read when empty)
//!
//! Hash function approach borrowed from buckets_waiting_for_journal:
//! random seeds with hash_64() give truly independent distributions.
//! With 3 hash functions and 512 slots at ~16 concurrent tasks,
//! probability of all 3 slots occupied is ~(16/512)^3 ≈ 0.003%.

#![allow(non_camel_case_types)]

use crate::c;
use crate::util::{futex::EventCount, hash::hash_64, projections::Volatile, sleep::Sleep};
use crate::os::kernel::{current_id, random_u64};
use core::ffi::c_ulong;
use core::ptr::NonNull;
use core::sync::atomic::{fence, Ordering::{Acquire, Release}};
use cstruct_macros::MaybeZeroable;
use pin_init::Zeroable;

use super::folio::Mapping;

pub const FDM_NR_HASH: u32 = 3;

pub const FDM_HASH_BITS: u32 = 9;

pub const FDM_HASH_SIZE: u32 = 1 << FDM_HASH_BITS;

#[repr(C)]
#[derive(MaybeZeroable)]
pub struct FdmSlot {
    pub task: Volatile<usize>,
    pub mapping: Volatile<c_ulong>, /* address_space * | dropped_locks bit */
}

#[repr(C)]
#[derive(MaybeZeroable)]
pub struct FdmHash {
    pub hash_seeds: [u64; FDM_NR_HASH as usize],
    pub wait: EventCount,
    pub slots: [FdmSlot; FDM_HASH_SIZE as usize],
}

impl FdmHash {
    /// Empty, its hash seeds random.
    pub fn init() -> impl pin_init::Init<Self> {
        pin_init::init!(Self {
            hash_seeds: core::array::from_fn(|_| random_u64()),
            ..Zeroable::init_zeroed()
        })
    }

    /// The current task's slots, one per hash.
    fn slots_of_current(&self) -> impl Iterator<Item = &FdmSlot> {
        let current = current_id();
        (0..FDM_NR_HASH as usize)
            .map(move |i| &self.slots[hash_64(current as u64 ^ self.hash_seeds[i], FDM_HASH_BITS) as usize])
    }

    fn slot(&self) -> Option<&FdmSlot> {
        let current = current_id();
        self.slots_of_current()
            .find(|s| s.task.get() == current)
            /* pair with smp_wmb() in fdm_set */
            .inspect(|_| fence(Acquire))
    }

    /// The mapping this task has page faults disabled on, if any.
    pub fn get(&self) -> Option<Mapping> {
        let mapping = NonNull::new((self.slot()?.mapping.get() & !1) as *mut c::address_space)?;
        Some(unsafe { Mapping::from_raw(mapping.as_ptr()) })
    }

    /// This task's DIO told that a fault dropped its locks.
    pub fn set_dropped_locks(&self) {
        if let Some(s) = self.slot() {
            s.mapping.set(s.mapping.get() | 1);
        }
    }

    /*
     * Insert current task's mapping. Only current ever inserts for itself,
     * so no lock needed. With 3-way hashing at ~3% occupancy, all three
     * slots being occupied is vanishingly rare (~0.003%). If it does
     * happen, sleep until a slot frees up.
     */
    pub fn set(&self, mapping: Mapping) -> FaultsDisabled<'_> {
        let current = current_id();
        debug_assert!(self.slots_of_current().all(|s| s.task.get() != current),
                      "faults disabled on a second mapping");

        loop {
            let Ok(()) = self.wait.wait_until(Sleep::UNINTERRUPTIBLE,
                || self.slots_of_current().any(|s| s.task.get() == 0).then_some(()));

            if let Some(s) = self.slots_of_current().find(|s| s.task.get() == 0) {
                s.mapping.set(mapping.as_ptr().addr() as _);
                /* mapping must be visible before task (task acts as "valid" marker) */
                fence(Release);
                s.task.set(current);
                return FaultsDisabled(self);
            }

            /* Lost race after wakeup — retry */
        }
    }
}

/// This task's page faults disabled on a mapping, till it's dropped: set()'s.
pub struct FaultsDisabled<'a>(&'a FdmHash);

impl FaultsDisabled<'_> {
    /// Whether a fault dropped this task's locks since set().
    pub fn dropped_locks(&self) -> bool {
        self.0.slot().is_some_and(|s| s.mapping.get() & 1 != 0)
    }
}

impl Drop for FaultsDisabled<'_> {
    fn drop(&mut self) {
        let current = current_id();
        if let Some(s) = self.0.slots_of_current().find(|s| s.task.get() == current) {
            s.task.set(0);
            /* Ensure task is cleared before mapping could be reused */
            fence(Release);
            self.0.wait.wake();
        }
    }
}
