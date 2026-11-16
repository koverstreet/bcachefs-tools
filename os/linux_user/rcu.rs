// SPDX-License-Identifier: GPL-2.0

//! RCU, for userspace, for util/rcu.rs: liburcu's memb flavor, which C's is
//! too, and SRCU domains on it: srcu.rs.

#[path = "../rcu.rs"]
mod common;
pub use common::*;

use crate::c;
use crate::locking::MemallocFlags;
use core::marker::PhantomData;

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

pub use super::srcu::Srcu;

pub fn synchronize() {
    unsafe { c::synchronize_rcu() };
}

/// How many grace periods back a poll's cookies may be pending at once: the
/// running one's and the next's - liburcu's start_poll_synchronize_rcu()
/// names one or the other, and a domain's get_state() the same.
pub const NUM_ACTIVE_POLL_OLDSTATE: usize = 2;

/// A grace period, as the poll API's cookie names one: RCU's grace_period_id,
/// or a domain's gp_seq.
#[derive(Clone, Copy, Default, PartialEq)]
pub struct GpCookie(pub(super) u64);

/// Which RCU: RCU, liburcu's memb flavor, or an SRCU domain's, borrowed.
#[derive(Clone, Copy)]
pub struct Flavor<'a>(Option<&'a Srcu>);

impl Flavor<'static> {
    pub const RCU: Self = Flavor(None);
}

impl<'a> Flavor<'a> {
    pub(super) fn srcu(srcu: &'a Srcu) -> Self {
        Flavor(Some(srcu))
    }

    /// The grace period a reader now would have to be past, without starting
    /// one - RCU's started, liburcu having no get_state.
    pub fn get_state(self) -> GpCookie {
        match self.0 {
            Some(srcu) => srcu.get_state(),
            None       => self.start_poll(),
        }
    }

    /// get_state(), and that grace period started: start_poll_synchronize_rcu().
    pub fn start_poll(self) -> GpCookie {
        match self.0 {
            Some(srcu) => srcu.start_poll(),
            None       => GpCookie(unsafe { c::start_poll_synchronize_rcu() }.grace_period_id as u64),
        }
    }

    /// Whether @c's grace period is over: poll_state_synchronize_rcu().
    pub fn poll(self, c: GpCookie) -> bool {
        match self.0 {
            Some(srcu) => srcu.poll(c),
            None       => unsafe {
                c::poll_state_synchronize_rcu(c::urcu_gp_poll_state { grace_period_id: c.0 as _ })
            },
        }
    }

    /// Every callback queued with call() run: rcu_barrier().
    pub fn barrier(self) {
        match self.0 {
            Some(srcu) => srcu.barrier(),
            None       => unsafe { c::rcu_barrier() },
        }
    }

    /// @func(@head) a grace period on: call_rcu(), for util/rcu.rs's call().
    ///
    /// # Safety
    /// @head isn't queued already, and lives until @func runs.
    pub(crate) unsafe fn call_raw(self, head: *mut c::rcu_head, func: unsafe extern "C" fn(*mut c::rcu_head)) {
        match self.0 {
            Some(srcu) => unsafe { srcu.call(head, func) },
            None       => unsafe { c::call_rcu(head, Some(func)) },
        }
    }
}
