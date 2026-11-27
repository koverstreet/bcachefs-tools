// SPDX-License-Identifier: GPL-2.0

//! The tasks there are: every task is on a list from when it's made until it
//! finishes, for debugfs to walk. A task's memory is freed a grace period
//! after it's done, so the walk, under RCU, can read what it finds.

use super::{Header, TaskRef};
use crate::data_structures::fast_list::FastList;
use crate::locking::MemallocFlags;
use crate::os::kernel::msleep;
use crate::util::late::{Late, LateInit, ModuleInit};
use crate::prt;
use crate::errcode::BchError;
use crate::util::log::Log;
use crate::util::printbuf::Msg;
use crate::util::rcu::{self, ReadGuard};
use crate::util::Printbuf;
use core::ffi::c_void;
use core::ptr::NonNull;
use pin_init::{pin_data, pin_init, PinInit};

#[pin_data]
#[repr(transparent)]
#[derive(::pin_init::Zeroable)]
pub struct TaskList {
    #[pin]
    list: FastList<c_void>,
}

/// Tasks of no filesystem's.
pub static TASKS: Late<TaskList, ModuleInit> = ::pin_init::zeroed();

impl LateInit for TaskList {
    fn is_made(&self) -> bool {
        self.list.is_made()
    }
}

impl TaskList {
    pub fn new() -> impl PinInit<Self, BchError> {
        pin_init!(Self { list <- FastList::new() }? BchError)
    }

    /// A list made and torn down by its owner, as a filesystem's async object
    /// lists are.
    pub fn of(list: &FastList<c_void>) -> &TaskList {
        unsafe { &*core::ptr::from_ref(list).cast::<TaskList>() }
    }

    /// The list's a debugging aid: no room isn't an error, the task just
    /// isn't listed - 0, which remove() ignores.
    pub(super) fn add(&self, task: TaskRef) -> u32 {
        self.list.add(task.0.cast()).unwrap_or(0)
    }

    pub(super) fn remove(&self, idx: u32) {
        self.list.remove(idx)
    }

    pub fn items_from<'g>(&'g self, idx: usize, rcu: &'g ReadGuard) -> impl Iterator<Item = (usize, TaskRef)> + 'g {
        self.list.items_from(idx, rcu).map(|(idx, task)| (idx, TaskRef(task.cast())))
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
                Log::NO_FS.warn(&Msg(|out: &mut Printbuf| {
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
