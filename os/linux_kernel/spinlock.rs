// SPDX-License-Identifier: GPL-2.0

//! The kernel's spinlocks, as RawLocks: SpinLockIrq, a spinlock_t taken
//! irqsave; SpinLock, a spinlock_t taken with spin_lock(); and RawSpinLock, a
//! raw_spinlock_t. Initializing in a lockdep class and spin_lock_irqsave() are
//! macros: shim.c's rust_* functions - but on x86 and arm64 the locks are
//! inline, the queued spinlock's fast paths, where that's all they are.

use crate::c;
use crate::locking::{MemallocFlags, RawLock, RawLockBase, RawNestedLock};
use crate::os::lockdep::{self, LockClass};
use crate::os::percpu::{local_irq_restore, local_irq_save, preempt_enable};
use crate::util::ffi::Opaque;
use core::convert::Infallible;
use core::ffi::CStr;
use crate::util::ffi::c_ulong;
use kernel::bindings as b;

pub type SpinLockIrq = Opaque<b::spinlock_t>;
pub type RawSpinLock = Opaque<b::raw_spinlock_t>;

// ── The queued spinlock, inline ──────────────────────────────────────────

/*
 * raw_spin_lock() and _unlock() as the kernel's inlines have them, where
 * that's all they are: a queued spinlock, not debugged, traced or paravirt.
 * spin_lock() too, but on PREEMPT_RT, where a spinlock_t sleeps.
 */
const RAW_INLINE: bool = cfg!(any(target_arch = "x86_64", target_arch = "aarch64")) &&
    !cfg!(any(CONFIG_DEBUG_SPINLOCK, CONFIG_DEBUG_LOCK_ALLOC, CONFIG_PARAVIRT_SPINLOCKS,
              CONFIG_QUEUED_SPINLOCKS_TRACE_CONTENDED_RELEASE));
const SPIN_INLINE: bool = RAW_INLINE && !cfg!(CONFIG_PREEMPT_RT);

/* where they're inline, each lock is only its arch_spinlock_t */
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const _: () = assert!(!RAW_INLINE || size_of::<b::raw_spinlock_t>() == size_of::<c::qspinlock>());
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
const _: () = assert!(!SPIN_INLINE || size_of::<b::spinlock_t>() == size_of::<c::qspinlock>());

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
mod qspin {
    pub(super) unsafe fn lock(_: *mut u8) {
        unreachable!()
    }

    pub(super) unsafe fn unlock(_: *mut u8) {
        unreachable!()
    }

    pub(super) unsafe fn do_unlock(_: *mut u8) {
        unreachable!()
    }
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
mod qspin {
    use crate::c;
    use crate::os::percpu::{preempt_disable, preempt_enable};
    use core::sync::atomic::{AtomicI32, AtomicU8, Ordering::{Acquire, Relaxed, Release}};

    unsafe extern "C" {
        fn queued_spin_lock_slowpath(lock: *mut c::qspinlock, val: u32);
    }

    /// __raw_spin_lock(): preemption off, then queued_spin_lock().
    #[inline(always)]
    pub(super) unsafe fn lock(l: *mut c::qspinlock) {
        preempt_disable();
        let val = unsafe { AtomicI32::from_ptr(&raw mut (*l).__bindgen_anon_1.val.counter) };
        if let Err(v) = val.compare_exchange(0, c::_Q_LOCKED_VAL as i32, Acquire, Relaxed) {
            unsafe { queued_spin_lock_slowpath(l, v as u32) }
        }
    }

    /// __raw_spin_unlock().
    #[inline(always)]
    pub(super) unsafe fn unlock(l: *mut c::qspinlock) {
        unsafe { do_unlock(l) };
        preempt_enable();
    }

    /// queued_spin_unlock().
    #[inline(always)]
    pub(super) unsafe fn do_unlock(l: *mut c::qspinlock) {
        unsafe { AtomicU8::from_ptr(&raw mut (*l).__bindgen_anon_1.__bindgen_anon_1.locked) }.store(0, Release)
    }
}

// taken and released from any thread
unsafe impl Send for SpinLockIrq {}
unsafe impl Sync for SpinLockIrq {}
unsafe impl Send for RawSpinLock {}
unsafe impl Sync for RawSpinLock {}

// ── SpinLockIrq: spinlock_t, taken irqsave ───────────────────────────────

// Taken with interrupts off - spin_lock_irqsave() - for what interrupt
// context takes too, an endio; held, it mustn't sleep.
impl RawLockBase for SpinLockIrq {
    type GuardState = (c_ulong, MemallocFlags);
    type InitError = Infallible;

    unsafe fn init(l: *mut Self, name: &'static CStr, key: LockClass) -> Result<(), Infallible> {
        unsafe { c::rust_spin_lock_init(l.cast(), name.as_ptr().cast(), key.as_ptr().cast()) };
        Ok(())
    }
}

impl RawLock for SpinLockIrq {
    fn lock(&self) -> (c_ulong, MemallocFlags) {
        let flags = if SPIN_INLINE {
            let flags = local_irq_save();
            unsafe { qspin::lock(self.as_ptr().cast()) };
            flags
        } else {
            unsafe { c::rust_spin_lock_irqsave(self.as_ptr().cast()) }
        };
        (flags, MemallocFlags::noreclaim())
    }

    unsafe fn unlock(&self, (flags, _noreclaim): (c_ulong, MemallocFlags)) {
        if SPIN_INLINE {
            // interrupts back on before preemption: a reschedule it finds
            // wanted can't run with them off
            unsafe { qspin::do_unlock(self.as_ptr().cast()) };
            local_irq_restore(flags);
            preempt_enable();
        } else {
            unsafe { c::rust_spin_unlock_irqrestore(self.as_ptr().cast(), flags) }
        }
    }

    fn assert_held(&self) {
        lockdep::spin_lock_assert_held(self)
    }
}

impl RawNestedLock for SpinLockIrq {
    fn lock_nested(&self, subclass: u32) -> (c_ulong, MemallocFlags) {
        let flags = unsafe { c::rust_spin_lock_irqsave_nested(self.as_ptr().cast(), subclass) };
        (flags, MemallocFlags::noreclaim())
    }
}

// ── SpinLock: spinlock_t, taken with spin_lock() ─────────────────────────

/// A spinlock_t taken with spin_lock(), interrupts as they are, for what no
/// interrupt takes - where SpinLockIrq is taken irqsave. Not RawSpinLock: on
/// PREEMPT_RT this one sleeps, so it can be taken in an RCU read section with
/// a GFP_NOWAIT allocation under it, where RT forbids allocating under a
/// raw_spinlock_t.
#[repr(transparent)]
pub struct SpinLock(Opaque<b::spinlock_t>);

unsafe impl Send for SpinLock {}
unsafe impl Sync for SpinLock {}

impl RawLockBase for SpinLock {
    type GuardState = MemallocFlags;
    type InitError = Infallible;

    unsafe fn init(l: *mut Self, name: &'static CStr, key: LockClass) -> Result<(), Infallible> {
        unsafe { c::rust_spin_lock_init(l.cast(), name.as_ptr().cast(), key.as_ptr().cast()) };
        Ok(())
    }
}

impl RawLock for SpinLock {
    fn lock(&self) -> MemallocFlags {
        if SPIN_INLINE {
            unsafe { qspin::lock(self.0.as_ptr().cast()) };
        } else {
            unsafe { b::spin_lock(self.0.as_ptr()) };
        }
        MemallocFlags::noreclaim()
    }

    unsafe fn unlock(&self, _noreclaim: MemallocFlags) {
        if SPIN_INLINE {
            unsafe { qspin::unlock(self.0.as_ptr().cast()) };
        } else {
            unsafe { b::spin_unlock(self.0.as_ptr()) };
        }
    }

    fn assert_held(&self) {
        lockdep::spin_lock_assert_held(&self.0)
    }
}

// ── RawSpinLock: raw_spinlock_t ──────────────────────────────────────────

// Taken with interrupts as they are, for what no interrupt takes - a lock's
// wait list. A spinning lock on PREEMPT_RT too, where spinlock_t sleeps.
impl RawLockBase for RawSpinLock {
    type GuardState = MemallocFlags;
    type InitError = Infallible;

    unsafe fn init(l: *mut Self, name: &'static CStr, key: LockClass) -> Result<(), Infallible> {
        unsafe { c::rust_raw_spin_lock_init(l.cast(), name.as_ptr().cast(), key.as_ptr().cast()) };
        Ok(())
    }
}

impl RawLock for RawSpinLock {
    fn lock(&self) -> MemallocFlags {
        if RAW_INLINE {
            unsafe { qspin::lock(self.as_ptr().cast()) };
        } else {
            unsafe { c::rust_raw_spin_lock(self.as_ptr().cast()) };
        }
        MemallocFlags::noreclaim()
    }

    unsafe fn unlock(&self, _noreclaim: MemallocFlags) {
        if RAW_INLINE {
            unsafe { qspin::unlock(self.as_ptr().cast()) };
        } else {
            unsafe { c::rust_raw_spin_unlock(self.as_ptr().cast()) }
        }
    }

    fn assert_held(&self) {
        lockdep::raw_spin_lock_assert_held(self)
    }
}
