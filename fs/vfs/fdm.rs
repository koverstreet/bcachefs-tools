// SPDX-License-Identifier: GPL-2.0

//! The faults-disabled-mapping table, as the page fault handler reads it:
//! vfs/fdm.h has the DIO path's side, and the design.

use crate::c;
use crate::util::hash::hash_64;
use crate::util::kernel::current_id;
use core::ptr::NonNull;
use core::sync::atomic::{fence, Ordering::Acquire};

use super::folio::Mapping;

impl c::fdm_hash {
    fn slot(&self) -> Option<&c::fdm_slot> {
        let current = current_id();
        (0..c::FDM_NR_HASH as usize)
            .map(|i| &self.slots[hash_64(current as u64 ^ self.hash_seeds[i], c::FDM_HASH_BITS) as usize])
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
}
