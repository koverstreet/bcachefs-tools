// SPDX-License-Identifier: GPL-2.0

//! Timers: struct timer_list, in a struct Rust defines, run from the timer
//! softirq - as an endio is, so it doesn't sleep. Its function is the
//! struct's RawTimer::run(), from run_raw::<Self>, as a WorkAt's; made with
//! new_timer!(), with a lockdep class per call site: timer_setup().

use crate::c;
use core::cell::UnsafeCell;
use core::ffi::{c_ulong, CStr};
use core::marker::PhantomPinned;
use core::pin::Pin;
use core::ptr::NonNull;
use kernel::sync::LockClassKey;

/// Deleted as it drops, a run under way waited out: C has it by address only
/// while it's pending.
#[repr(transparent)]
pub struct TimerList {
    timer: UnsafeCell<c::timer_list>,
    _pin:  PhantomPinned,
}

// the timer code's to synchronize, from any thread
unsafe impl Send for TimerList {}
unsafe impl Sync for TimerList {}

pub type TimerFn = unsafe extern "C" fn(*mut c::timer_list);

/// A TimerList at a fixed place in Self, running Self::run(): new_timer!()
/// with run_raw::<Self>, the trampoline, which run() is inlined into.
pub trait RawTimer {
    /// offset_of!() the TimerList
    const TIMER: usize;

    /// # Safety
    /// @this's timer expired.
    unsafe fn run(this: NonNull<Self>);
}

#[doc(hidden)]
pub unsafe extern "C" fn run_raw<T: RawTimer>(timer: *mut c::timer_list) {
    unsafe { T::run(NonNull::new_unchecked(timer.byte_sub(T::TIMER).cast())) }
}

impl TimerList {
    #[doc(hidden)]
    pub fn new(func: TimerFn, name: &'static CStr, key: Pin<&'static LockClassKey>)
            -> impl ::pin_init::PinInit<Self> {
        unsafe {
            ::pin_init::pin_init_from_closure(move |slot: *mut Self| {
                c::timer_init_key(slot.cast(), Some(func), 0, name.as_ptr().cast(), key.as_ptr());
                Ok(())
            })
        }
    }

    /// Run at @expires, in jiffies - moved there if it was pending:
    /// mod_timer(). Whether it was.
    pub fn modify(&self, expires: c_ulong) -> bool {
        unsafe { c::mod_timer(self.timer.get(), expires as usize) != 0 }
    }

    /// Not pending any more, and not running - waiting out a run that is:
    /// timer_delete_sync(). Whether it was pending.
    pub fn delete_sync(&self) -> bool {
        unsafe { c::timer_delete_sync(self.timer.get()) != 0 }
    }
}

impl Drop for TimerList {
    fn drop(&mut self) {
        self.delete_sync();
    }
}

/// A TimerList's initializer, running C's @func, its lockdep class the call
/// site's, named @name: timer_setup().
#[macro_export]
macro_rules! new_timer {
    ($func:expr, $name:literal $(,)?) => {
        $crate::os::timer::TimerList::new(
            $func,
            const { $crate::util::ffi::c_str(::core::concat!($name, "\0")) },
            ::kernel::static_lock_class!())
    };
}
