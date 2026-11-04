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
//! flags, then takes R - and releases R, then restores them. Laid out as R,
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

use super::{Guard, Locked, RawLock, RawLockBase, RawRwLock, RawTryLock, ReadGuard};
use crate::cstructs::c;
use crate::types::c_default;
use core::ffi::{c_uint, c_void, CStr};
use cstruct_macros::c_verbatim;

#[cfg(kernel)]
mod pf {
    pub const PF_MEMALLOC_NOIO: u32 = kernel::bindings::PF_MEMALLOC_NOIO;
    // util.h's, from the same holes
    pub const PF_MEMALLOC_NORECLAIM: u32 = kernel::bindings::PF__HOLE__00800000;
    pub const PF_MEMALLOC_NOWARN: u32 = kernel::bindings::PF__HOLE__01000000;
}
#[cfg(not(kernel))]
mod pf {
    pub use crate::cstructs::c::{PF_MEMALLOC_NOIO, PF_MEMALLOC_NORECLAIM, PF_MEMALLOC_NOWARN};
}
pub(crate) use pf::*;

/// current->flags, which only current writes. None in a userspace thread the
/// shim didn't start, which has no task: and no scope, which its allocator
/// doesn't take anyway.
fn current_flags() -> Option<&'static mut c_uint> {
    #[cfg(kernel)]
    let t = kernel::task::Task::current_raw();
    #[cfg(not(kernel))]
    let t = unsafe { c::bch2_current() };

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

/// Lock @R, held in a PF_MEMALLOC_NOIO scope: the flags are saved before
/// it's taken, and restored after it's released.
#[repr(transparent)]
pub struct Noio<R>(R);

unsafe impl<R: RawLockBase> RawLockBase for Noio<R> {
    type GuardState = (MemallocFlags, R::GuardState);
    type InitError = R::InitError;

    unsafe fn init(l: *mut Self, name: &'static CStr, key: *mut c_void) -> Result<(), R::InitError> {
        unsafe { R::init(l.cast(), name, key) }
    }

    unsafe fn destroy(&self) {
        unsafe { self.0.destroy() }
    }
}

unsafe impl<R: RawLock> RawLock for Noio<R> {
    unsafe fn lock(&self) -> Self::GuardState {
        let noio = MemallocFlags::noio();
        (noio, unsafe { self.0.lock() })
    }

    unsafe fn unlock(&self, (noio, g): Self::GuardState) {
        unsafe { self.0.unlock(g) };
        drop(noio);
    }

    unsafe fn assert_held(&self) {
        unsafe { self.0.assert_held() }
    }
}

unsafe impl<R: RawTryLock> RawTryLock for Noio<R> {
    unsafe fn try_lock(&self) -> Option<Self::GuardState> {
        // not taken: dropped, so restored
        let noio = MemallocFlags::noio();
        Some((noio, unsafe { self.0.try_lock() }?))
    }
}

unsafe impl<R: RawRwLock> RawRwLock for Noio<R> {
    unsafe fn lock_shared(&self) -> Self::GuardState {
        let noio = MemallocFlags::noio();
        (noio, unsafe { self.0.lock_shared() })
    }

    unsafe fn unlock_shared(&self, (noio, g): Self::GuardState) {
        unsafe { self.0.unlock_shared(g) };
        drop(noio);
    }

    unsafe fn assert_held_any(&self) {
        unsafe { self.0.assert_held_any() }
    }
}

unsafe impl<R: ::pin_init::Zeroable> ::pin_init::Zeroable for Noio<R> {}

impl<'a, R: RawRwLock<GuardState = ()>, G: Sync, P> ReadGuard<'a, Noio<R>, G, P> {
    pub fn with_write<T>(&mut self, f: impl FnOnce(&mut Guard<'_, Noio<R>, G, P>) -> T) -> T {
        /*
         * Raw of the inner rwsem: our caller holds the lock read via the noio
         * guard, so PF_MEMALLOC_NOIO stays set across this temporary upgrade
         * to write - the raw up/down only move the lock, not the memalloc
         * scope.
         */
        let lock = self.lock();
        unsafe { lock.raw().0.unlock_shared(()) };
        let ret = f(&mut lock.write());
        unsafe { lock.raw().0.lock_shared() };
        ret
    }
}

impl<'a, R: RawLock<GuardState = ()>, G, P> Guard<'a, Noio<R>, G, P> {
    pub fn with_unlocked<T>(&mut self, f: impl FnOnce() -> T) -> T {
        /*
         * Raw drop/retake of just the lock: the guard keeps PF_MEMALLOC_NOIO
         * set the whole time.
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

c_verbatim!(r#"
/*
 * Guard-only by design. The saved memalloc flags live in the guard object, so
 * a raw lock/unlock pair would have nowhere to stash them; scoped use also
 * guarantees the LIFO nesting that memalloc_flags_save/restore require. Use
 * guard(mutex_noio)(&m) or scoped_guard(mutex_noio, &m).
 */
struct mutex_noio {
	struct mutex		lock;
};

/*
 * For rwsems like capacity.mark_lock that are taken over allocating work.
 * Guards mirror the kernel's percpu_read/percpu_write, with _noio.
 *
 * A few hot paths take the lock raw (percpu_down_read on the inner
 * percpu_rw_semaphore) rather than via the guard - that's sound only where the
 * caller is already in a NOIO context (e.g. holding a locked btree_trans);
 * such sites reach through .lock with a comment saying why.
 */
struct percpu_rwsem_noio {
	struct percpu_rw_semaphore lock;
};
"#);
