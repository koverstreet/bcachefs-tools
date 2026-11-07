// SPDX-License-Identifier: GPL-2.0

//! With CONFIG_BCACHEFS_TESTS a table has Faults, given by the torture test
//! (debug/tests/cuckoo.rs), for what load alone rarely reaches: windows
//! inside moves and between a lookup's slots, failed allocations, a packed
//! table - and Seen, counts of how often each rare path ran. Without, Faults
//! is zero sized and every hook a constant.

#[cfg(CONFIG_BCACHEFS_TESTS)]
use core::sync::atomic::AtomicU64;

/// What the torture test injects into one table.
#[cfg(CONFIG_BCACHEFS_TESTS)]
#[derive(Default)]
pub struct Faults {
    /// Spins inside each move, between its copy, its count and its emptying
    /// - and a yield, the writer off CPU mid-chain, holding the writers' lock.
    pub move_window: u32,
    /// Spins in a lookup before each slot it reads.
    pub read_window: u32,
    /// One in this many buffer allocations fails (0: none).
    pub alloc_fail:  u32,
    /// The table grows only when it's full: inserts walk long chains, and
    /// rehash to find room.
    pub dense:       bool,
    pub seen:        Seen,
}

/// How often each rare path ran: that a test reached them.
#[cfg(CONFIG_BCACHEFS_TESTS)]
#[derive(Default)]
pub struct Seen {
    /// Lookups that missed and retried: a move was counted during them.
    pub retry:  AtomicU64,
    /// Lookups that missed and looked again with the buffer's own seeds.
    pub raced:  AtomicU64,
    pub rehash: AtomicU64,
    /// Rehashes whose seeds didn't fit at a size, trying a bigger table.
    pub grown:  AtomicU64,
}

#[cfg(CONFIG_BCACHEFS_TESTS)]
fn spins(n: u32) {
    (0..n).for_each(|_| core::hint::spin_loop());
}

#[cfg(CONFIG_BCACHEFS_TESTS)]
impl Faults {
    pub(super) fn move_window(&self) {
        if self.move_window != 0 {
            spins(self.move_window);
            crate::util::kernel::cond_resched();
        }
    }

    pub(super) fn read_window(&self) { spins(self.read_window) }

    pub(super) fn alloc_fail(&self) -> bool {
        self.alloc_fail != 0 && crate::util::kernel::random_u64_below(self.alloc_fail.into()) == 0
    }

    pub(super) fn dense(&self) -> bool { self.dense }
}

#[cfg(not(CONFIG_BCACHEFS_TESTS))]
#[derive(Default)]
pub struct Faults;

#[cfg(not(CONFIG_BCACHEFS_TESTS))]
impl Faults {
    pub(super) fn move_window(&self) {}
    pub(super) fn read_window(&self) {}
    pub(super) fn alloc_fail(&self) -> bool { false }
    pub(super) fn dense(&self) -> bool { false }
}
