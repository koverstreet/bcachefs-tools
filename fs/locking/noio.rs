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
//! only. The flag itself stays on the C side (locking.c): the
//! PF_MEMALLOC_* #defines don't reach Rust.

#![allow(non_camel_case_types)]

use super::{Lock, RawLock, RawRwLock};
use crate::cstructs::c;
use crate::types::c_default;
use core::ffi::{c_uint, c_void, CStr};
use cstruct_macros::{c_extern, c_verbatim};

c_extern! {
    pub fn rust_memalloc_noio_save() -> c_uint;
    pub fn rust_memalloc_flags_restore(flags: c_uint);
}

/// A PF_MEMALLOC_NOIO scope: allocations may not recurse into reclaim IO
/// until it's dropped, which restores the flags it saved.
#[must_use = "the memalloc scope ends as soon as the guard is dropped"]
pub struct MemallocFlags {
    saved: u32,
}

impl MemallocFlags {
    pub fn noio() -> Self {
        Self { saved: unsafe { rust_memalloc_noio_save() } }
    }
}

impl Drop for MemallocFlags {
    fn drop(&mut self) {
        unsafe { rust_memalloc_flags_restore(self.saved) }
    }
}

/// Lock @R, held in a PF_MEMALLOC_NOIO scope: the flags are saved before
/// it's taken, and restored after it's released.
#[repr(transparent)]
pub struct Noio<R>(R);

unsafe impl<R: RawLock> RawLock for Noio<R> {
    type GuardState = (MemallocFlags, R::GuardState);
    type InitError = R::InitError;

    unsafe fn init(l: *mut Self, name: &'static CStr, key: *mut c_void) -> Result<(), R::InitError> {
        unsafe { R::init(l.cast(), name, key) }
    }

    unsafe fn lock(l: *mut Self) -> Self::GuardState {
        let noio = MemallocFlags::noio();
        (noio, unsafe { R::lock(l.cast()) })
    }

    unsafe fn unlock(l: *mut Self, (noio, g): Self::GuardState) {
        unsafe { R::unlock(l.cast(), g) };
        drop(noio);
    }

    unsafe fn assert_held(l: *mut Self) {
        unsafe { R::assert_held(l.cast()) }
    }

    unsafe fn destroy(l: *mut Self) {
        unsafe { R::destroy(l.cast()) }
    }
}

unsafe impl<R: RawRwLock> RawRwLock for Noio<R> {
    unsafe fn lock_shared(l: *mut Self) -> Self::GuardState {
        let noio = MemallocFlags::noio();
        (noio, unsafe { R::lock_shared(l.cast()) })
    }

    unsafe fn unlock_shared(l: *mut Self, (noio, g): Self::GuardState) {
        unsafe { R::unlock_shared(l.cast(), g) };
        drop(noio);
    }

    unsafe fn assert_held_any(l: *mut Self) {
        unsafe { R::assert_held_any(l.cast()) }
    }
}

unsafe impl<R: ::pin_init::Zeroable> ::pin_init::Zeroable for Noio<R> {}

/// C's struct mutex_noio and percpu_rwsem_noio, declared for C below.
pub type mutex_noio = Noio<c::mutex>;
pub type percpu_rwsem_noio = Noio<c::percpu_rw_semaphore>;

c_default!(mutex_noio);
c_default!(percpu_rwsem_noio);

/// Locks over @G held in a NOIO scope: C's mutex_noio and percpu_rwsem_noio,
/// and what they protect.
pub type MutexNoio<G = (), P = ()> = Lock<mutex_noio, G, P>;
pub type PercpuRwsemNoio<G = (), P = ()> = Lock<percpu_rwsem_noio, G, P>;

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
