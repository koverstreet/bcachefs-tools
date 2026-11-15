// SPDX-License-Identifier: GPL-2.0

//! Lockdep: a lock's map and the hooks on it, for locks whose state is ours -
//! six, ByteLock - so no C lock tells lockdep about them; and the C locks'
//! asserts.

use crate::c;
use crate::locking::{Mutex, PercpuRwSem, RawSpinLock, RwSem, SpinLockIrq};
use crate::util::ffi::c_char;
use core::ffi::{c_ulong, c_void};
use core::pin::Pin;
use core::ptr;

pub use kernel::sync::LockClassKey;

/// A lock's lockdep class, by its key: a static's.
pub type LockClass = Pin<&'static LockClassKey>;

/// The lockdep class key of the call site: a static of its own.
#[macro_export]
macro_rules! lock_class_key {
    () => { ::kernel::static_lock_class!() };
}

/// A lock's struct lockdep_map. Without lockdep it's nothing, as C's is then
/// too - an empty struct - so a struct C shares declares one either way. (C's
/// is empty without CONFIG_LOCKDEP; everything that turns that on turns on
/// CONFIG_DEBUG_LOCK_ALLOC, which the hooks are under.)
#[repr(C)]
pub struct LockdepMap(
    #[cfg(CONFIG_DEBUG_LOCK_ALLOC)] crate::util::ffi::Opaque<crate::cstructs::c::lockdep_map>,
    // C's empty struct: improper_ctypes won't have a struct with no fields
    #[cfg(not(CONFIG_DEBUG_LOCK_ALLOC))] [u8; 0],
);

#[cfg(CONFIG_DEBUG_LOCK_ALLOC)]
impl LockdepMap {
    fn raw(&self) -> *mut c_void {
        self.0.as_ptr().cast()
    }

    /// # Safety
    /// Before its lock is first taken.
    pub unsafe fn init(this: *mut Self, name: *const c_char, key: LockClass) {
        unsafe { c::rust_lockdep_init_map(this.cast(), name, key.as_ptr().cast()) }
    }

    pub fn acquire(&self, trylock: bool, read: bool, ip: c_ulong) {
        unsafe { c::rust_lock_acquire(self.raw(), trylock.into(), read.into(), ip) }
    }

    pub fn release(&self, ip: c_ulong) {
        unsafe { c::rust_lock_release(self.raw(), ip) }
    }

    pub fn contended(&self, ip: c_ulong) {
        unsafe { c::rust_lock_contended(self.raw(), ip) }
    }

    pub fn acquired(&self, ip: c_ulong) {
        unsafe { c::rust_lock_acquired(self.raw(), ip) }
    }

    /// For a lock lockdep doesn't track: btree node locks.
    pub fn set_notrack_class(&self) {
        unsafe { c::rust_lockdep_set_notrack_class(self.raw().cast()) }
    }
}

#[cfg(not(CONFIG_DEBUG_LOCK_ALLOC))]
impl LockdepMap {
    /// # Safety
    /// None: there's nothing to initialize.
    pub unsafe fn init(_this: *mut Self, _name: *const c_char, _key: LockClass) {}
    pub fn acquire(&self, _trylock: bool, _read: bool, _ip: c_ulong) {}
    pub fn release(&self, _ip: c_ulong) {}
    pub fn contended(&self, _ip: c_ulong) {}
    pub fn acquired(&self, _ip: c_ulong) {}
    pub fn set_notrack_class(&self) {}
}

/// That no lock lockdep knows of is in the @len bytes at @p, about to be
/// freed or reused.
pub fn debug_check_no_locks_freed(p: *const c_void, len: usize) {
    #[cfg(CONFIG_DEBUG_LOCK_ALLOC)]
    unsafe { c::rust_debug_check_no_locks_freed(p, len as c_ulong) };
    #[cfg(not(CONFIG_DEBUG_LOCK_ALLOC))]
    let _ = (p, len);
}

/// @take - which takes locks - run as if in reclaim: so lockdep knows reclaim
/// may wait on them, and complains of any held across an allocation that may
/// reclaim. Without lockdep there's no one to tell: @take isn't run.
pub fn reclaim_lock_order(take: impl FnOnce()) {
    #[cfg(CONFIG_LOCKDEP)]
    {
        unsafe extern "C" {
            fn fs_reclaim_acquire(gfp_mask: c::gfp_t);
            fn fs_reclaim_release(gfp_mask: c::gfp_t);
        }

        let gfp = c::GFP_KERNEL;
        unsafe { fs_reclaim_acquire(gfp) };
        take();
        unsafe { fs_reclaim_release(gfp) };
    }
    #[cfg(not(CONFIG_LOCKDEP))]
    let _ = take;
}

/// The contention tracepoints, for a lock whose state is ours: of @lock's
/// address, which they print and never follow.
pub fn trace_contention_begin<L>(lock: &L, flags: core::ffi::c_uint) {
    unsafe { c::rust_trace_contention_begin(ptr::from_ref(lock).cast(), flags) }
}

pub fn trace_contention_end<L>(lock: &L, ret: core::ffi::c_int) {
    unsafe { c::rust_trace_contention_end(ptr::from_ref(lock).cast(), ret) }
}

pub fn mutex_assert_held(l: &Mutex) {
    unsafe { c::rust_mutex_assert_held(ptr::from_ref(l).cast_mut().cast()) }
}

pub fn spin_lock_assert_held(l: &SpinLockIrq) {
    unsafe { c::rust_spin_lock_assert_held(ptr::from_ref(l).cast_mut().cast()) }
}

pub fn raw_spin_lock_assert_held(l: &RawSpinLock) {
    unsafe { c::rust_raw_spin_lock_assert_held(ptr::from_ref(l).cast_mut().cast()) }
}

pub fn rwsem_assert_held(l: &RwSem) {
    unsafe { c::rust_rwsem_assert_held(ptr::from_ref(l).cast_mut().cast()) }
}

pub fn rwsem_assert_held_write(l: &RwSem) {
    unsafe { c::rust_rwsem_assert_held_write(ptr::from_ref(l).cast_mut().cast()) }
}

pub fn percpu_rwsem_assert_held(l: &PercpuRwSem) {
    unsafe { c::rust_percpu_rwsem_assert_held(ptr::from_ref(l).cast_mut().cast()) }
}

pub fn percpu_rwsem_assert_held_write(l: &PercpuRwSem) {
    unsafe { c::rust_percpu_rwsem_assert_held_write(ptr::from_ref(l).cast_mut().cast()) }
}
