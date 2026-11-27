// SPDX-License-Identifier: GPL-2.0

//! NOIO: a memory-reclaim scope, and the locks that hold one.
//!
//! bcachefs holds a PF_MEMALLOC_NOIO scope with certain locks, so what's
//! allocated under them can't recurse into reclaim IO - a filesystem driving
//! the block layer directly mustn't let reclaim loop back through the device
//! it's allocating for. sb_lock above all: pairing each such lock with a
//! separate guard(memalloc_flags)(PF_MEMALLOC_NOIO) is easy to forget, and
//! was, on many sb_lock sites - so the scope is the lock's: holding it _is_
//! the NOIO context, and you can't take it without.
//!
//! Noio<R> is any lock R, taken that way: a RawLock that saves the memalloc
//! flags, then takes R - and releases R, then restores them - Memalloc<R,
//! FLAGS> with NOIO; another scope is another FLAGS.
//!
//! MemallocFlags is the scope alone - saved on construction, restored on
//! drop, as memalloc_flags_save() and _restore() pair: scoped (LIFO) use
//! only. NOIO's, or NORECLAIM's or NOWARN's: those two are ours, in sched.h's
//! PF__HOLE__ bits, so the page allocator doesn't know them - util::alloc
//! applies them. What can't sleep - spinlocks, RCU, preemption or interrupts
//! off - holds a NORECLAIM scope with its guard.

#![allow(non_camel_case_types)]

use core::ffi::CStr;
use crate::locking::{Exclusive, ExclusiveGuard, Locked, Mutex, PercpuRwSem, RawLock, RawLockBase,
    RawReadLock, RawRwLock, RawTryLock, Shared, SharedGuard};
use crate::os::{lockdep::LockClass, sched};
use crate::util::alloc::{flags, Flags};

pub(crate) use crate::c::{PF_MEMALLOC_NOIO, PF_MEMALLOC_NORECLAIM, PF_MEMALLOC_NOWARN};

/// current->flags, which only current writes. None in a userspace thread the
/// shim didn't start, which has no task: and no scope, which its allocator
/// doesn't take anyway.
fn current_flags() -> Option<&'static mut u32> {
    let t = sched::current();

    unsafe { t.as_mut() }.map(|t| &mut t.flags)
}

/// Set @flags in current->flags: those of them that were, for
/// memalloc_flags_restore().
pub(crate) fn memalloc_flags_save(flags: u32) -> u32 {
    let Some(f) = current_flags() else { return flags };
    let was = *f & flags;
    *f |= flags;
    was
}

/// @flags back as memalloc_flags_save() found them: @was set, the rest not.
pub(crate) fn memalloc_flags_restore(flags: u32, was: u32) {
    if let Some(f) = current_flags() {
        *f = *f & !flags | was;
    }
}

/// A PF_MEMALLOC_* scope, until it's dropped. NOIO: allocations may not
/// recurse into reclaim IO. NORECLAIM and NOWARN, which only our allocators
/// apply (os::alloc's current_gfp_context_raw()): no direct reclaim, and
/// __GFP_NOWARN.
#[must_use = "the memalloc scope ends as soon as the guard is dropped"]
pub struct MemallocFlags {
    flags: u32,
    /// Those of @flags already set, which stay set
    was:   u32,
}

impl MemallocFlags {
    fn new(flags: u32) -> Self {
        Self { flags, was: memalloc_flags_save(flags) }
    }

    pub fn noio() -> Self {
        Self::new(PF_MEMALLOC_NOIO)
    }

    pub fn noreclaim() -> Self {
        Self::new(PF_MEMALLOC_NORECLAIM)
    }

    pub fn nowarn() -> Self {
        Self::new(PF_MEMALLOC_NOWARN)
    }

    /// The scope a caller's @gfp says, for Rust that takes none: NORECLAIM for
    /// NOWAIT or ATOMIC, else NOIO for NOIO; NOWARN. The rest of @gfp has no
    /// scope, and goes.
    pub fn for_gfp(gfp: Flags) -> Self {
        let mut pf = 0;
        if gfp.intersects(flags::NOWAIT | flags::ATOMIC) {
            pf |= PF_MEMALLOC_NORECLAIM;
        } else if gfp.contains(flags::NOIO) {
            pf |= PF_MEMALLOC_NOIO;
        }
        if gfp.contains(flags::NOWARN) {
            pf |= PF_MEMALLOC_NOWARN;
        }
        Self::new(pf)
    }
}

impl Drop for MemallocFlags {
    fn drop(&mut self) {
        memalloc_flags_restore(self.flags, self.was)
    }
}

/// Lock @R, held in a PF_MEMALLOC_* scope of @FLAGS: the flags are saved
/// before it's taken, and restored after it's released.
#[repr(transparent)]
pub struct Memalloc<R, const FLAGS: u32>(R);

/// Lock @R, held in a PF_MEMALLOC_NOIO scope.
pub type Noio<R> = Memalloc<R, PF_MEMALLOC_NOIO>;

impl<R: RawLockBase, const F: u32> RawLockBase for Memalloc<R, F> {
    type GuardState = (MemallocFlags, R::GuardState);
    type InitError = R::InitError;

    unsafe fn init(l: *mut Self, name: &'static CStr, key: LockClass) -> Result<(), R::InitError> {
        unsafe { R::init(l.cast(), name, key) }
    }

    unsafe fn destroy(&self) {
        unsafe { self.0.destroy() }
    }
}

impl<R: RawLock, const F: u32> RawLock for Memalloc<R, F> {
    fn lock(&self) -> Self::GuardState {
        let scope = MemallocFlags::new(F);
        (scope, self.0.lock())
    }

    unsafe fn unlock(&self, (scope, g): Self::GuardState) {
        unsafe { self.0.unlock(g) };
        drop(scope);
    }

    fn assert_held(&self) {
        self.0.assert_held()
    }
}

impl<R: RawTryLock, const F: u32> RawTryLock for Memalloc<R, F> {
    fn try_lock(&self) -> Option<Self::GuardState> {
        // not taken: dropped, so restored
        let scope = MemallocFlags::new(F);
        Some((scope, self.0.try_lock()?))
    }
}

impl<R: RawReadLock, const F: u32> RawReadLock for Memalloc<R, F> {
    fn lock_read(&self) -> Self::GuardState {
        let scope = MemallocFlags::new(F);
        (scope, self.0.lock_read())
    }

    unsafe fn unlock_read(&self, (scope, g): Self::GuardState) {
        unsafe { self.0.unlock_read(g) };
        drop(scope);
    }
}

impl<R: RawRwLock, const F: u32> RawRwLock for Memalloc<R, F> {
    fn assert_held_any(&self) {
        self.0.assert_held_any()
    }
}

unsafe impl<R: ::pin_init::Zeroable, const F: u32> ::pin_init::Zeroable for Memalloc<R, F> {}

impl<'a, R: RawRwLock<GuardState = ()>, const F: u32, G: Sync, P, A> SharedGuard<'a, Memalloc<R, F>, G, P, Shared, A> {
    pub fn with_write<T>(&mut self, f: impl FnOnce(&mut ExclusiveGuard<'_, Memalloc<R, F>, G, P, Exclusive, A>) -> T)
        -> T
    {
        /*
         * Raw of the inner rwsem: our caller holds the lock read via the
         * guard, so its memalloc scope stays set across this temporary
         * upgrade to write - the raw up/down only move the lock, not the
         * scope.
         */
        let lock = self.lock();
        unsafe { lock.raw().0.unlock_read(()) };
        let ret = f(&mut self.write_placed());
        lock.raw().0.lock_read();
        ret
    }
}

impl<'a, R: RawLock<GuardState = ()>, const F: u32, G, P, A> ExclusiveGuard<'a, Memalloc<R, F>, G, P, Exclusive, A> {
    pub fn with_unlocked<T>(&mut self, f: impl FnOnce() -> T) -> T {
        /*
         * Raw drop/retake of just the lock: the guard keeps its memalloc
         * scope set the whole time.
         */
        let lock = self.lock();
        unsafe { lock.raw().0.unlock(()) };
        let ret = f();
        lock.raw().0.lock();
        ret
    }
}

/// Locks over @G held in a NOIO scope, and what they protect.
pub type MutexNoio<G = (), P = ()> = Locked<Noio<Mutex>, G, P>;
pub type PercpuRwsemNoio<G = (), P = ()> = Locked<Noio<PercpuRwSem>, G, P>;

