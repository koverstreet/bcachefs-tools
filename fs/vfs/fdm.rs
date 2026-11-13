// SPDX-License-Identifier: GPL-2.0

//! The faults-disabled-mapping table: the DIO write path's side, which sets a
//! task's mapping while it pins user pages, and the page fault handler's,
//! which reads it. vfs/fdm.h has the design.

use crate::c;
use crate::util::closure::{closure_wait_event, closure_wake_up};
use crate::util::hash::hash_64;
use crate::os::kernel::{current_id, random_u64};
use core::ptr::NonNull;
use core::sync::atomic::{fence, Ordering::{Acquire, Release}};

use super::folio::Mapping;

impl c::fdm_hash {
    /// Empty, its hash seeds random.
    pub fn init() -> impl pin_init::Init<Self> {
        // zeroed is empty: every field's a C type, valid all zeroes
        unsafe {
            pin_init::init_from_closure(|p: *mut Self| {
                p.write_bytes(0, 1);
                (*p).hash_seeds = core::array::from_fn(|_| random_u64());
                Ok(())
            })
        }
    }

    /// The current task's slots, one per hash.
    fn slots_of_current(&self) -> impl Iterator<Item = &c::fdm_slot> {
        let current = current_id();
        (0..c::FDM_NR_HASH as usize)
            .map(move |i| &self.slots[hash_64(current as u64 ^ self.hash_seeds[i], c::FDM_HASH_BITS) as usize])
    }

    fn slot(&self) -> Option<&c::fdm_slot> {
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
     * happen, sleep on the closure waitlist until a slot frees up.
     */
    pub fn set(&self, mapping: Mapping) -> FaultsDisabled<'_> {
        let current = current_id();
        debug_assert!(self.slots_of_current().all(|s| s.task.get() != current),
                      "faults disabled on a second mapping");

        loop {
            closure_wait_event(&self.wait, || self.slots_of_current().any(|s| s.task.get() == 0));

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
pub struct FaultsDisabled<'a>(&'a c::fdm_hash);

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
            closure_wake_up(&self.0.wait);
        }
    }
}
