// SPDX-License-Identifier: GPL-2.0

//! Rust counterparts to the primitives in util/locking.h.
//!
//! bcachefs establishes a memory-reclaim scope (PF_MEMALLOC_NOIO) while holding
//! certain locks, so allocations under the lock can't recurse into reclaim IO -
//! a filesystem driving the block layer directly mustn't let reclaim loop back
//! through the device it's allocating for. On the C side the lock type carries
//! the scope (`struct mutex_noio`); on the Rust side the guard is composed
//! explicitly, holding a [`MemallocFlags`] alongside the lock.
//!
//! [`MemallocFlags`] saves the current flags on construction and restores them
//! on drop, mirroring the C memalloc_flags_save/restore pairing - scoped (LIFO)
//! use only, as those primitives require. The flag itself stays on the C side
//! (util/locking.h's rust_memalloc_* shims): the PF_MEMALLOC_* #defines don't
//! reach Rust through bindgen.

use crate::c;

/// RAII guard for a process-context memalloc scope. Restores the previous
/// flags when dropped.
#[must_use = "the memalloc scope ends as soon as the guard is dropped"]
pub struct MemallocFlags {
    saved: u32,
}

impl MemallocFlags {
    /// Enter a PF_MEMALLOC_NOIO scope: allocations may not recurse into reclaim
    /// IO for as long as the guard is held.
    pub fn noio() -> Self {
        // SAFETY: rust_memalloc_noio_save only touches current->flags and
        // returns the previous value, to be replayed by the paired restore.
        Self { saved: unsafe { c::rust_memalloc_noio_save() } }
    }
}

impl Drop for MemallocFlags {
    fn drop(&mut self) {
        // SAFETY: `saved` is the value returned by the paired save above.
        unsafe { c::rust_memalloc_flags_restore(self.saved) }
    }
}
