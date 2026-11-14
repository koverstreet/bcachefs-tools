// SPDX-License-Identifier: GPL-2.0

//! Allocation flags, both ports' alike: how an allocation is special - may
//! it wait, start IO, dip into reserves - and how it comes back. None is a
//! plain allocation. Only what we use: the kernel port maps them to its gfp_t,
//! and userspace reads them itself.

use core::ops::{BitAnd, BitOr, BitOrAssign, Not};

/// Flags to allocate memory with, from the [`flags`] module: combined with
/// `|`, `&` and `!`.
#[repr(transparent)]
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Flags(u32);

impl Flags {
    pub const fn empty() -> Self {
        Flags(0)
    }

    /// @f set too: `|`, in a const.
    pub const fn union(self, f: Flags) -> Self {
        Flags(self.0 | f.0)
    }

    /// Whether every flag in @f is set.
    pub const fn contains(self, f: Flags) -> bool {
        self.0 & f.0 == f.0
    }

    /// Whether any flag in @f is set.
    pub const fn intersects(self, f: Flags) -> bool {
        self.0 & f.0 != 0
    }
}

impl BitOr for Flags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Flags(self.0 | rhs.0)
    }
}

impl BitOrAssign for Flags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl BitAnd for Flags {
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self {
        Flags(self.0 & rhs.0)
    }
}

impl Not for Flags {
    type Output = Self;

    fn not(self) -> Self {
        Flags(!self.0)
    }
}

pub mod flags {
    use super::Flags;

    // No flags is a plain allocation: it may wait for memory to be freed, in
    // reclaim that may start IO and call into filesystems - this one too. What
    // follows is what's special.

    /// Never waits - fails instead, quietly - and wakes background reclaim.
    pub const NOWAIT:      Flags = Flags(1 << 0);
    /// Never waits, and may dip into the emergency reserves: for what can't
    /// wait and mustn't fail, an interrupt's.
    pub const ATOMIC:      Flags = Flags(1 << 1);
    /// Waits, but reclaim starts no IO and calls into no filesystem: for what
    /// IO completion may be waiting on.
    pub const NOIO:        Flags = Flags(1 << 2);

    pub const ZERO:        Flags = Flags(1 << 3);
    /// Fails without a warning.
    pub const NOWARN:      Flags = Flags(1 << 4);
    /// Fails rather than try hard: no retrying reclaim, no OOM killer.
    pub const NORETRY:     Flags = Flags(1 << 5);
    /// Never into the emergency reserves - not even from a task that's
    /// freeing memory itself.
    pub const NO_RESERVES: Flags = Flags(1 << 6);
    /// Charged to the task's memory cgroup.
    pub const ACCOUNT:     Flags = Flags(1 << 7);
    /// What a shrinker frees: kept with memory reclaim can free, away from
    /// what it can't.
    pub const RECLAIMABLE: Flags = Flags(1 << 8);
    /// Not zeroed where the allocator would zero it anyway, for memory
    /// tagging: it's about to be overwritten.
    pub const SKIP_ZERO:   Flags = Flags(1 << 9);
    /// Never fails: waits for memory as long as it takes - so never where
    /// reclaim can't run.
    pub const NOFAIL:      Flags = Flags(1 << 10);
}
