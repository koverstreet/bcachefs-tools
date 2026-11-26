// SPDX-License-Identifier: GPL-2.0

//! The tasks there are: every task is on a list from when it's made until it
//! finishes, for debugfs to walk. A task's memory is freed a grace period
//! after it's done, so the walk, under RCU, can read what it finds.

use super::{Header, TaskRef};
use crate::data_structures::fast_list::FastList;
use crate::locking::MemallocFlags;
use crate::os::{kernel::msleep, module::{Quiesced, ThisModule}};
use crate::prt;
use crate::errcode::BchError;
use crate::util::alloc::init_at;
use crate::util::log::{log_no_fs, LogLevel};
use crate::util::printbuf::Msg;
use crate::util::rcu::{self, ReadGuard};
use crate::util::Printbuf;
use core::ffi::c_void;
use core::ptr::NonNull;

#[repr(transparent)]
pub struct TaskList(FastList);

/// Tasks of no filesystem's.
pub static TASKS: TaskList = TaskList(::pin_init::zeroed());

impl TaskList {
    /// A list made and torn down by its owner, as a filesystem's async object
    /// lists are.
    pub fn of(list: &FastList) -> &TaskList {
        unsafe { &*core::ptr::from_ref(list).cast::<TaskList>() }
    }

    /// A static's, made in place at module init. Panics if it was made already.
    pub fn init(&'static self, _: &ThisModule) -> Result<(), BchError> {
        assert!(!self.0.is_made(), "the module's task list made twice");
        // SAFETY: zeroed, at module init, before any task is made
        unsafe { init_at(FastList::new(), core::ptr::from_ref(&self.0).cast_mut()) }
    }

    /// A static's, dropped in place and zeroed again: a no-op on one never made.
    pub fn exit(&'static self, _: &Quiesced<'_>) {
        if self.0.is_made() {
            let list = core::ptr::from_ref(&self.0).cast_mut();
            // SAFETY: at module exit, past the barrier: with no filesystem, the
            // last tasks - those freed after a grace period - have run
            unsafe {
                core::ptr::drop_in_place(list);
                core::ptr::write_bytes(list, 0, 1);
            }
        }
    }

    /// The list's a debugging aid: no room isn't an error, the task just
    /// isn't listed - 0, which remove() ignores.
    pub(super) fn add(&self, task: TaskRef) -> u32 {
        self.0.add(task.0.cast()).unwrap_or(0)
    }

    pub(super) fn remove(&self, idx: u32) {
        self.0.remove(idx)
    }

    pub fn items_from<'g>(&'g self, idx: usize, rcu: &'g ReadGuard) -> impl Iterator<Item = (usize, TaskRef)> + 'g {
        self.0.items_from(idx, rcu).map(|(idx, task)| (idx, TaskRef(task.cast())))
    }

    /// Before the list goes: a task comes off it as it finishes, after its
    /// last act - which may be what told its waiter, and so the list's owner,
    /// that it was done. One that hasn't after ten seconds is stuck, and said.
    pub fn drain(&self) {
        let mut waited = 0;
        while self.items_from(0, &rcu::read_lock()).next().is_some() {
            msleep(1);
            waited += 1;
            if waited == 10_000 {
                log_no_fs(LogLevel::WARN, &Msg(|out: &mut Printbuf| {
                    prt!(out, "tasks still running at teardown, waiting on them:\n");
                    let (_noreclaim, rcu) = (MemallocFlags::noreclaim(), rcu::read_lock());
                    for (_, task) in self.items_from(0, &rcu) {
                        task.to_text(out);
                    }
                }));
            }
        }
    }
}

impl TaskRef {
    /// # Safety
    /// @item is what a TaskList lists, read under RCU.
    pub unsafe fn from_listed(item: NonNull<c_void>) -> Self {
        TaskRef(item.cast())
    }
}

impl Header {
    pub(super) fn unlist(&self) {
        unsafe { self.list.as_ref() }.remove(self.listed.replace(0));
    }
}
