// SPDX-License-Identifier: GPL-2.0

//! A task (util::task) in a kmalloc()'d MaybeRcuAlloc with what it's the task
//! of - its home, which its future borrows - its memory freed after a grace
//! period: what's in it may be found under RCU. A TaskSlot is the box before
//! its task: the home made, and the future, which may consume what can't be
//! dropped, like a guard to be ended, made only once there's memory for it.
//!
//! The future is made by an async closure handed the home: it may borrow the
//! home, but only for as long as the closure's call, which no lifetime it can
//! name outlives - so it can't keep the borrow past the task. A box with no
//! done of its owner's frees itself when its task is done; spawn_detached()
//! is one started at once.

use crate::errcode::BchError;
use crate::util::alloc::Kmalloc;
use crate::util::maybe_rcu_alloc::MaybeRcuAlloc;
use crate::util::rcu::RcuHead;
use crate::util::workqueue::Queue;
use super::{Spawn, Task, TaskRef};
use core::future::Future;
use core::marker::PhantomData;
use core::mem::{ManuallyDrop, MaybeUninit};
use core::ops::AsyncFnOnce;
use core::pin::Pin;
use core::ptr::NonNull;
use pin_init::PinInit;

/// The future @M makes, for a home of @H: its borrow of the home erased, as
/// the box keeps it.
type FutureOf<M, H> = <M as AsyncFnOnce<(&'static H,)>>::CallOnceFuture;

/// One allocation: the task of a future borrowing @home, then the home. The
/// task's head is the free's, so the task goes first: tiny RCU's kvfree_rcu()
/// wants it in the first page.
#[repr(C)]
#[derive(RcuHead)]
struct WithTask<'q, H, F: Future<Output = ()>> {
    #[rcu_head]
    task: MaybeUninit<Task<'q, F>>,
    home: H,
    done: Option<unsafe fn(NonNull<H>)>,
}

impl<H, F: Future<Output = ()>> WithTask<'_, H, F> {
    /// # Safety
    /// @home is a WithTask<H, F>'s.
    unsafe fn of_home(home: NonNull<H>) -> NonNull<Self> {
        unsafe { home.byte_sub(core::mem::offset_of!(Self, home)).cast() }
    }
}

/// A TaskBox's memory, its home made and its task not yet: allocated apart
/// from making the future, a failed allocation consumes none of what the
/// future would. Not bounded by @M's AsyncFnOnce: a bound naming its future
/// type would be what rustc infers the closure's signature from, and that's
/// for one lifetime, not every one.
pub struct TaskSlot<'q, H, M> {
    home: NonNull<H>,
    /// The home dropped, and the memory freed
    free: unsafe fn(NonNull<H>),
    _p:   PhantomData<(&'q Queue, fn() -> M)>,
}

impl<'q, H, M> TaskSlot<'q, H, M>
where
    H: Send + Sync + 'static,
    M: for<'h> AsyncFnOnce<(&'h H,), Output = ()>,
    for<'h> <M as AsyncFnOnce<(&'h H,)>>::CallOnceFuture: Send,
{
    pub fn alloc<E>(home: impl PinInit<H, E>) -> Result<Self, BchError>
    where
        BchError: From<E>,
    {
        let init = unsafe {
            pin_init::pin_init_from_closure(move |w: *mut WithTask<'q, H, FutureOf<M, H>>| {
                crate::util::alloc::init_at(home, &raw mut (*w).home).map_err(BchError::from)
            })
        };
        let w = MaybeRcuAlloc::<WithTask<'q, H, FutureOf<M, H>>, Kmalloc>::new(init)?.into_raw();
        Ok(Self {
            home: unsafe { NonNull::new_unchecked(&raw mut (*w.as_ptr()).home) },
            free: free_slot::<H, FutureOf<M, H>>,
            _p:   PhantomData,
        })
    }

    /// The home, before there's a task to borrow it.
    pub fn home(&mut self) -> Pin<&mut H> {
        unsafe { Pin::new_unchecked(self.home.as_mut()) }
    }

    /// The task to be, for a Done its owner arms before it starts: where it
    /// will be.
    pub fn task(&self) -> TaskRef {
        let w = unsafe { WithTask::<'q, H, FutureOf<M, H>>::of_home(self.home) }.as_ptr();
        TaskRef(unsafe { NonNull::new_unchecked(&raw mut (*w).task).cast() })
    }

    /// The task of the future @make makes from the home, to run @at. @done is
    /// the box's owner's cue that the task is done and it may drop the box;
    /// with none, the box frees itself. What @make holds outlives the box,
    /// unstarted: started, start_unchecked() says how long the task runs.
    pub fn init(self, at: Spawn<'q, '_>, make: M, done: Option<unsafe fn(NonNull<H>)>)
        -> Result<TaskBox<'q, H>, BchError>
    where
        M: 'q,
    {
        let home = ManuallyDrop::new(self).home;
        unsafe {
            let w = WithTask::<'q, H, FutureOf<M, H>>::of_home(home).as_ptr();
            (&raw mut (*w).done).write(done);
            let future = make(&*(&raw const (*w).home));
            let task = Task::new(at, future, Self::task_done);
            if let Err(e) = crate::util::alloc::init_at(task, (&raw mut (*w).task).cast()) {
                free_slot::<H, FutureOf<M, H>>(home);
                return Err(e);
            }
            Ok(TaskBox { home,
                         task: (*w).task.assume_init_ref().task(),
                         free: free::<H, FutureOf<M, H>>,
                         _q:   PhantomData })
        }
    }

    unsafe fn task_done(task: NonNull<Task<'q, FutureOf<M, H>>>, (): ()) {
        unsafe {
            let w = task.as_ptr().byte_sub(core::mem::offset_of!(WithTask<'q, H, FutureOf<M, H>>, task))
                .cast::<WithTask<'q, H, FutureOf<M, H>>>();
            let home = NonNull::new_unchecked(&raw mut (*w).home);
            match (*w).done {
                Some(done) => done(home),
                None => free::<H, FutureOf<M, H>>(home),
            }
        }
    }
}

impl<H, M> Drop for TaskSlot<'_, H, M> {
    fn drop(&mut self) {
        unsafe { (self.free)(self.home) }
    }
}

/// A TaskSlot's free: the home made, the task never.
///
/// # Safety
/// @home is a WithTask<H, F>'s, its task not made.
unsafe fn free_slot<H, F: Future<Output = ()>>(home: NonNull<H>) {
    drop(unsafe { MaybeRcuAlloc::<WithTask<'_, H, F>, Kmalloc>::from_raw(WithTask::of_home(home)) })
}

/// The task first - an unstarted one's future borrows the home - then the
/// home.
///
/// # Safety
/// @home is a WithTask<H, F>'s, its task made and not running.
unsafe fn free<H, F: Future<Output = ()>>(home: NonNull<H>) {
    unsafe {
        let w = WithTask::<'_, H, F>::of_home(home);
        core::ptr::drop_in_place((*w.as_ptr()).task.as_mut_ptr());
        drop(MaybeRcuAlloc::<WithTask<'_, H, F>, Kmalloc>::from_raw(w));
    }
}

/// A task in one box with what it's the task of, @H. Unstarted, dropping it
/// drops the task. Started, a box with a done of its owner's is dropped by
/// the owner once the task's done; one without frees itself.
pub struct TaskBox<'q, H> {
    home: NonNull<H>,
    task: TaskRef,
    free: unsafe fn(NonNull<H>),
    /// The task's queue
    _q:   PhantomData<&'q Queue>,
}

// SAFETY: shared as &H, and dropped wherever its owner is
unsafe impl<H: Send + Sync> Send for TaskBox<'_, H> {}
unsafe impl<H: Send + Sync> Sync for TaskBox<'_, H> {}

impl<H> TaskBox<'_, H> {
    pub fn task(&self) -> TaskRef {
        self.task
    }

    /// A box that frees itself, started.
    ///
    /// # Safety
    /// Its queue, and what its future borrows beyond its home, outlive the
    /// task.
    pub unsafe fn start_unchecked(self) {
        unsafe { ManuallyDrop::new(self).task.start() };
    }

    /// start_unchecked(), its first poll run here.
    ///
    /// # Safety
    /// As start_unchecked()'s.
    pub unsafe fn start_inline_unchecked(self) {
        unsafe { ManuallyDrop::new(self).task.start_inline() };
    }
}

impl<H> core::ops::Deref for TaskBox<'_, H> {
    type Target = H;

    fn deref(&self) -> &H {
        unsafe { self.home.as_ref() }
    }
}

impl<H> Drop for TaskBox<'_, H> {
    fn drop(&mut self) {
        unsafe { (self.free)(self.home) };
    }
}

/// A task nothing joins, in a box of its own, started.
///
/// # Safety
/// Its queue, and what the future borrows, outlive the task.
pub unsafe fn spawn_detached<F: Future<Output = ()> + Send>(at: Spawn<'_, '_>, future: F) -> Result<(), BchError> {
    let task = TaskSlot::alloc(())?.init(at, async move |_: &()| future.await, None)?;
    unsafe { task.start_unchecked() };
    Ok(())
}

/// spawn_detached(), for a task that borrows nothing, on a queue that lasts.
pub fn spawn_static<F: Future<Output = ()> + Send + 'static>(at: Spawn<'static, '_>, future: F)
    -> Result<(), BchError>
{
    unsafe { spawn_detached(at, future) }
}
