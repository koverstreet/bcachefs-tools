// SPDX-License-Identifier: GPL-2.0

//! The tasks there are: every task is on a list from when it's made until it
//! finishes, for debugfs to walk. A task's memory is freed a grace period
//! after it's done, so the walk, under RCU, can read what it finds.

use super::TaskRef;
use crate::c;
use crate::util::alloc::AllocError;
use crate::util::rcu::ReadGuard;

#[repr(transparent)]
pub struct TaskList(c::fast_list);

pub static TASKS: TaskList = TaskList(::pin_init::zeroed());

impl TaskList {
    /// # Safety
    /// At module init, before any task is made.
    pub unsafe fn init(&self) -> Result<(), AllocError> {
        unsafe { self.0.init() }
    }

    /// # Safety
    /// At module exit, after the last task is done.
    pub unsafe fn exit(&self) {
        unsafe { self.0.exit() }
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
}
