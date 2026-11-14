// SPDX-License-Identifier: GPL-2.0

//! RCU, for userspace, for util/rcu.rs: liburcu's memb flavor, which C's is
//! too. SRCU is RCU here: a domain has nothing of its own to read under or
//! wait on, and its grace periods are RCU's.

#[path = "../rcu.rs"]
mod common;
pub use common::*;

use crate::c;
use crate::errcode::BchError;
use crate::locking::MemallocFlags;
use core::ffi::{c_int, c_void, CStr};
use core::marker::PhantomData;
use pin_init::PinInit;

/// Not Send: the section is the thread's. A NORECLAIM scope while it lives.
pub struct ReadGuard {
    _noreclaim: MemallocFlags,
    _not_send:  PhantomData<*mut ()>,
}

pub fn read_lock() -> ReadGuard {
    unsafe { c::rcu_read_lock() };

    ReadGuard {
        _noreclaim: MemallocFlags::noreclaim(),
        _not_send:  PhantomData,
    }
}

impl Drop for ReadGuard {
    fn drop(&mut self) {
        unsafe { c::rcu_read_unlock() };
    }
}

/// An SRCU domain: nothing of its own here.
#[repr(C)]
pub struct Srcu([u8; 0]);

impl Srcu {
    /// # Safety
    /// @key is a static lockdep class.
    pub unsafe fn new(_name: &'static CStr, _key: *mut c_void) -> impl PinInit<Self, BchError> {
        unsafe { ::pin_init::pin_init_from_closure(|_slot: *mut Self| Ok(())) }
    }

    pub fn read_lock(&self) -> SrcuReadGuard<'_> {
        unsafe { SrcuReadGuard::from_raw(self, 0) }
    }

    fn read_unlock(&self, _idx: c_int) {}

    /// Its grace periods, for what waits on or queues to them: RCU's.
    pub fn flavor(&self) -> Flavor<'_> {
        Flavor(PhantomData)
    }
}

impl Drop for Srcu {
    fn drop(&mut self) {
        // what's queued through a flavor() outlives its borrow
        self.flavor().barrier();
    }
}

pub fn synchronize() {
    unsafe { c::synchronize_rcu() };
}

/// How many grace periods back a poll's cookies may be pending at once, as
/// the shim's srcu.h had it.
pub const NUM_ACTIVE_POLL_OLDSTATE: usize = 2;

/// A grace period, as the poll API's cookie names one.
#[derive(Clone, Copy, Default)]
pub struct GpCookie(c::urcu_gp_poll_state);

impl PartialEq for GpCookie {
    fn eq(&self, o: &Self) -> bool {
        self.0.grace_period_id == o.0.grace_period_id
    }
}

/// Which RCU: RCU, or an SRCU domain's, borrowed - which is RCU here.
#[derive(Clone, Copy)]
pub struct Flavor<'a>(PhantomData<&'a Srcu>);

impl Flavor<'static> {
    pub const RCU: Self = Flavor(PhantomData);
}

impl<'a> Flavor<'a> {
    /// The grace period a reader now would have to be past:
    /// start_poll_synchronize_rcu(), liburcu having no get_state.
    pub fn get_state(self) -> GpCookie {
        GpCookie(unsafe { c::start_poll_synchronize_rcu() })
    }

    /// get_state(), and that grace period started: start_poll_synchronize_rcu().
    pub fn start_poll(self) -> GpCookie {
        GpCookie(unsafe { c::start_poll_synchronize_rcu() })
    }

    /// Whether @c's grace period is over: poll_state_synchronize_rcu().
    pub fn poll(self, c: GpCookie) -> bool {
        unsafe { c::poll_state_synchronize_rcu(c.0) }
    }

    /// Every callback queued with call() run: rcu_barrier().
    pub fn barrier(self) {
        unsafe { c::rcu_barrier() };
    }

    /// @func(@head) a grace period on: call_rcu().
    ///
    /// # Safety
    /// @head isn't queued already, and lives until @func runs.
    pub unsafe fn call(self, head: *mut c::rcu_head, func: unsafe extern "C" fn(*mut c::rcu_head)) {
        unsafe { c::call_rcu(head, Some(func)) };
    }
}
