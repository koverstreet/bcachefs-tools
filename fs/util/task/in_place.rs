// SPDX-License-Identifier: GPL-2.0

//! A task in the memory of what it's the task of, which its output may own:
//! so it's dropped in place when it finishes, and only then is its output
//! ended (TaskEnd) - which may free the memory it was in.

use crate::util::alloc::init_at;
use crate::util::workqueue::Queue;
use super::{Spawn, Task, TaskRef};
use core::cell::UnsafeCell;
use core::future::Future;
use core::mem::MaybeUninit;
use core::ptr::NonNull;
use pin_init::{Init, PinInit};

/// What a task in place ends with: its output, once the task's memory is
/// done with.
pub trait TaskEnd {
    fn end(self);
}

#[repr(transparent)]
#[derive(::pin_init::Zeroable)]
pub struct InPlace<'q, F: Future>(UnsafeCell<MaybeUninit<Task<'q, F>>>);

// The task once it's started, as Task is: only the executor touches it
unsafe impl<F: Future + Send> Send for InPlace<'_, F> where F::Output: Send {}
unsafe impl<F: Future + Send> Sync for InPlace<'_, F> where F::Output: Send {}

impl<'q, F: Future> InPlace<'q, F> {
    /// No task in it yet: nothing written.
    pub fn new() -> impl Init<Self> {
        unsafe { pin_init::init_from_closure(|_| Ok(())) }
    }
}

impl<'q, F: Future + Send> InPlace<'q, F>
where
    F::Output: TaskEnd,
{
    /// The task of @future, started in @this: its first poll run here.
    ///
    /// # Safety
    /// No task is in @this, its memory lasts till the task's end, and what
    /// @future borrows outlives the task.
    pub unsafe fn start_inline(this: NonNull<Self>, q: &'q Queue, future: impl PinInit<F>) {
        unsafe { Self::init(this, q, future).start_inline() }
    }

    /// start_inline(), its first poll on @q.
    ///
    /// # Safety
    /// As start_inline()'s.
    pub unsafe fn start(this: NonNull<Self>, q: &'q Queue, future: impl PinInit<F>) {
        unsafe { Self::init(this, q, future).start() }
    }

    unsafe fn init(this: NonNull<Self>, q: &'q Queue, future: impl PinInit<F>) -> TaskRef {
        let task = this.cast::<Task<'q, F>>().as_ptr();
        unsafe {
            init_at(Task::new(Spawn::Queue(q), future, Self::done), task)
                .expect("a queue's task: nothing in it fails");
            (*task).task()
        }
    }

    /// # Safety
    /// The executor's, as the task's done function: @task is finished.
    unsafe fn done(task: NonNull<Task<'q, F>>, out: F::Output) {
        unsafe { core::ptr::drop_in_place(task.as_ptr()) };
        out.end();
    }
}
