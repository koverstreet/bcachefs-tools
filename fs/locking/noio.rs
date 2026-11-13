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
//! FLAGS> with NOIO; another scope is another FLAGS. Laid out as R,
//! so Noio<c::mutex> is C's struct mutex_noio and Noio<c::percpu_rw_semaphore>
//! its struct percpu_rwsem_noio, which C's guards take the same way
//! (locking.h).
//!
//! MemallocFlags is the scope alone - saved on construction, restored on
//! drop, as memalloc_flags_save() and _restore() pair: scoped (LIFO) use
//! only. NOIO's, or NORECLAIM's or NOWARN's: those two are ours, in sched.h's
//! PF__HOLE__ bits, so the page allocator doesn't know them - util::alloc
//! applies them. What can't sleep - spinlocks, RCU, preemption or interrupts
//! off - holds a NORECLAIM scope with its guard.

#![allow(non_camel_case_types)]

use super::{Exclusive, ExclusiveGuard, Locked, RawLock, RawLockBase, RawMovableLock, RawReadLock, RawRwLock, RawTryLock, Shared,
            SharedGuard};
use crate::cstructs::c;
use crate::types::c_default;
use crate::os::sched;
use core::ffi::{c_uint, c_void, CStr};

pub(crate) use crate::c::{PF_MEMALLOC_NOIO, PF_MEMALLOC_NORECLAIM, PF_MEMALLOC_NOWARN};

/// current->flags, which only current writes. None in a userspace thread the
/// shim didn't start, which has no task: and no scope, which its allocator
/// doesn't take anyway.
fn current_flags() -> Option<&'static mut c_uint> {
    let t = sched::current();

    unsafe { t.as_mut() }.map(|t| &mut t.flags)
}

/// current->flags: the scopes the task is in.
#[cfg(kernel)]
pub(crate) fn memalloc_flags() -> u32 {
    current_flags().map_or(0, |f| *f)
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
/// recurse into reclaim IO. NORECLAIM and NOWARN, which only our allocation
/// entry points apply (util::alloc::current_gfp_context()): as GFP_NOWAIT,
/// and __GFP_NOWARN.
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

    /// The scope a C caller's @gfp says, for Rust that takes none, in the
    /// flags we use: NORECLAIM without direct reclaim, else NOIO without
    /// __GFP_IO; NOWARN. The rest of @gfp has no scope, and goes.
    /// In userspace, whose allocators take no scope, none.
    pub fn for_gfp(gfp: crate::c::gfp_t) -> Self {
        #[cfg(kernel)]
        let flags = {
            use crate::c::{GFP_KERNEL, GFP_NOIO, GFP_NOWAIT, __GFP_NOWARN};

            // __GFP_DIRECT_RECLAIM and __GFP_IO, which bindgen doesn't name
            let mut flags = 0;
            if gfp & (GFP_NOIO & !GFP_NOWAIT) == 0 {
                flags |= PF_MEMALLOC_NORECLAIM;
            } else if gfp & (GFP_KERNEL & !GFP_NOIO) == 0 {
                flags |= PF_MEMALLOC_NOIO;
            }
            if gfp & __GFP_NOWARN != 0 {
                flags |= PF_MEMALLOC_NOWARN;
            }
            flags
        };
        #[cfg(not(kernel))]
        let flags = { let _ = gfp; 0 };
        Self::new(flags)
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

unsafe impl<R: RawMovableLock, const F: u32> RawMovableLock for Memalloc<R, F> {}

unsafe impl<R: RawLockBase, const F: u32> RawLockBase for Memalloc<R, F> {
    type GuardState = (MemallocFlags, R::GuardState);
    type InitError = R::InitError;

    unsafe fn init(l: *mut Self, name: &'static CStr, key: *mut c_void) -> Result<(), R::InitError> {
        unsafe { R::init(l.cast(), name, key) }
    }

    unsafe fn destroy(&self) {
        unsafe { self.0.destroy() }
    }
}

unsafe impl<R: RawLock, const F: u32> RawLock for Memalloc<R, F> {
    unsafe fn lock(&self) -> Self::GuardState {
        let scope = MemallocFlags::new(F);
        (scope, unsafe { self.0.lock() })
    }

    unsafe fn unlock(&self, (scope, g): Self::GuardState) {
        unsafe { self.0.unlock(g) };
        drop(scope);
    }

    unsafe fn assert_held(&self) {
        unsafe { self.0.assert_held() }
    }
}

unsafe impl<R: RawTryLock, const F: u32> RawTryLock for Memalloc<R, F> {
    unsafe fn try_lock(&self) -> Option<Self::GuardState> {
        // not taken: dropped, so restored
        let scope = MemallocFlags::new(F);
        Some((scope, unsafe { self.0.try_lock() }?))
    }
}

unsafe impl<R: RawReadLock, const F: u32> RawReadLock for Memalloc<R, F> {
    unsafe fn lock_read(&self) -> Self::GuardState {
        let scope = MemallocFlags::new(F);
        (scope, unsafe { self.0.lock_read() })
    }

    unsafe fn unlock_read(&self, (scope, g): Self::GuardState) {
        unsafe { self.0.unlock_read(g) };
        drop(scope);
    }
}

unsafe impl<R: RawRwLock, const F: u32> RawRwLock for Memalloc<R, F> {
    unsafe fn assert_held_any(&self) {
        unsafe { self.0.assert_held_any() }
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
        let ret = f(&mut lock.write().placed());
        unsafe { lock.raw().0.lock_read() };
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
        unsafe { lock.raw().0.lock() };
        ret
    }
}

/// C's struct mutex_noio and percpu_rwsem_noio, declared for C below.
pub type mutex_noio = Noio<c::mutex>;
pub type percpu_rwsem_noio = Noio<c::percpu_rw_semaphore>;

c_default!(mutex_noio);
c_default!(percpu_rwsem_noio);

/// Locks over @G held in a NOIO scope: C's mutex_noio and percpu_rwsem_noio,
/// and what they protect.
pub type MutexNoio<G = (), P = ()> = Locked<mutex_noio, G, P>;
pub type PercpuRwsemNoio<G = (), P = ()> = Locked<percpu_rwsem_noio, G, P>;

