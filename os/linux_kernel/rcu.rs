// SPDX-License-Identifier: GPL-2.0

//! The kernel's RCU, for util/rcu.rs: read-side sections, SRCU domains, and
//! grace periods waited on, polled and queued to.

#[path = "../rcu.rs"]
mod common;
pub use common::*;

use crate::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::locking::MemallocFlags;
use core::ffi::{c_int, c_void, CStr};
use core::marker::PhantomData;
use pin_init::PinInit;

/// Not Send: the section is the thread's. A NORECLAIM scope while it lives.
pub struct ReadGuard {
    _guard:     kernel::sync::rcu::Guard,
    _noreclaim: MemallocFlags,
    _not_send:  PhantomData<*mut ()>,
}

pub fn read_lock() -> ReadGuard {
    ReadGuard {
        _guard:     kernel::sync::rcu::read_lock(),
        _noreclaim: MemallocFlags::noreclaim(),
        _not_send:  PhantomData,
    }
}

/// An SRCU domain, C's srcu_struct: cleaned up when it drops.
#[repr(C)]
pub struct Srcu(crate::util::ffi::Opaque<c::srcu_struct>);

// an srcu_struct is any thread's to read under and wait on
unsafe impl Send for Srcu {}
unsafe impl Sync for Srcu {}

impl Srcu {
    /// # Safety
    /// @key is a static lockdep class.
    pub unsafe fn new(name: &'static CStr, key: *mut c_void) -> impl PinInit<Self, BchError> {
        unsafe {
            ::pin_init::pin_init_from_closure(move |slot: *mut Self| {
                ret_to_result_void(c::rust_init_srcu_struct(slot.cast(), name.as_ptr().cast(), key))
            })
        }
    }

    fn as_ptr(&self) -> *mut c::srcu_struct {
        self.0.as_ptr()
    }

    pub fn read_lock(&self) -> SrcuReadGuard<'_> {
        let idx = unsafe { c::srcu_read_lock(self.as_ptr()) };
        unsafe { SrcuReadGuard::from_raw(self, idx) }
    }

    fn read_unlock(&self, idx: c_int) {
        unsafe { c::srcu_read_unlock(self.as_ptr(), idx) };
    }

    /// Its grace periods, for what waits on or queues to them.
    pub fn flavor(&self) -> Flavor<'_> {
        Flavor(self.as_ptr(), PhantomData)
    }
}

impl Drop for Srcu {
    fn drop(&mut self) {
        // what's queued through a flavor() outlives its borrow
        self.flavor().barrier();
        unsafe {
            c::synchronize_srcu_expedited(self.as_ptr());
            c::cleanup_srcu_struct(self.as_ptr());
        }
    }
}

pub fn synchronize() {
    unsafe { kernel::bindings::synchronize_rcu() };
}

/// How many grace periods back a poll's cookies may be pending at once: RCU's,
/// which SRCU's is no more than - rcu_pending.rs's lists are either's.
pub const NUM_ACTIVE_POLL_OLDSTATE: usize = kernel::bindings::NUM_ACTIVE_RCU_POLL_OLDSTATE as usize;
const _: () = assert!(NUM_ACTIVE_POLL_OLDSTATE >= kernel::bindings::NUM_ACTIVE_SRCU_POLL_OLDSTATE as usize);

/// A grace period, as the poll API's cookie names one.
#[derive(Clone, Copy, Default, PartialEq)]
pub struct GpCookie(usize);

/// Which RCU: the kernel's, or an SRCU domain's, borrowed - C's srcu_struct
/// pointer, null for RCU.
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct Flavor<'a>(*mut c::srcu_struct, PhantomData<&'a Srcu>);

// an srcu_struct is any thread's to wait on and queue on
unsafe impl Send for Flavor<'_> {}
unsafe impl Sync for Flavor<'_> {}

impl Flavor<'static> {
    pub const RCU: Self = Flavor(core::ptr::null_mut(), PhantomData);
}

impl<'a> Flavor<'a> {
    fn ssp(self) -> Option<*mut kernel::bindings::srcu_struct> {
        (!self.0.is_null()).then(|| self.0.cast())
    }

    /// The grace period a reader now would have to be past, without starting
    /// one: get_state_synchronize_rcu().
    pub fn get_state(self) -> GpCookie {
        GpCookie(match self.ssp() {
            Some(ssp) => unsafe { kernel::bindings::get_state_synchronize_srcu(ssp) },
            None      => unsafe { kernel::bindings::get_state_synchronize_rcu() },
        })
    }

    /// get_state(), and that grace period started: start_poll_synchronize_rcu().
    pub fn start_poll(self) -> GpCookie {
        GpCookie(match self.ssp() {
            Some(ssp) => unsafe { kernel::bindings::start_poll_synchronize_srcu(ssp) },
            None      => unsafe { kernel::bindings::start_poll_synchronize_rcu() },
        })
    }

    /// Whether @c's grace period is over: poll_state_synchronize_rcu().
    pub fn poll(self, c: GpCookie) -> bool {
        match self.ssp() {
            Some(ssp) => unsafe { kernel::bindings::poll_state_synchronize_srcu(ssp, c.0) },
            None      => unsafe { kernel::bindings::poll_state_synchronize_rcu(c.0) },
        }
    }

    /// Every callback queued with call() run: rcu_barrier().
    pub fn barrier(self) {
        match self.ssp() {
            Some(ssp) => unsafe { kernel::bindings::srcu_barrier(ssp) },
            None      => unsafe { kernel::bindings::rcu_barrier() },
        }
    }

    /// @func(@head) a grace period on: call_rcu().
    ///
    /// # Safety
    /// @head isn't queued already, and lives until @func runs.
    pub unsafe fn call(self, head: *mut c::rcu_head, func: unsafe extern "C" fn(*mut c::rcu_head)) {
        match self.ssp() {
            Some(ssp) => unsafe { kernel::bindings::call_srcu(ssp, head, Some(func)) },
            None      => unsafe { kernel::bindings::call_rcu(head, Some(func)) },
        }
    }
}
