// SPDX-License-Identifier: GPL-2.0

//! A task (util::task) in a kmalloc()'d box of its own: the spawner's till
//! start(), then the task's done's, which frees it after a grace period -
//! what's in it may be found under RCU. spawn_detached() is one started at
//! once, for a task nothing joins.

use crate::errcode::BchError;
use crate::util::alloc::{kalloc_pinned_gfp, Flags};
use crate::util::rcu::kvfree_rcu;
use crate::util::workqueue::Queue;
use super::{Spawn, Task, TaskRef};
use core::future::Future;
use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::pin::Pin;
use core::ptr::NonNull;
use pin_init::{pin_data, PinInit};

/// Started, the box is given up - nothing holds 'q, or what the future
/// borrows, after: so start() is for one that borrows nothing, and
/// start_unchecked() for the rest.
pub struct TaskBox<'q, F: Future<Output = ()>>(NonNull<Task<'q, F>>);

// SAFETY: a task, Send as its future is
unsafe impl<F: Future<Output = ()> + Send> Send for TaskBox<'_, F> {}

impl<'q, F: Future<Output = ()> + Send> TaskBox<'q, F> {
    pub fn new(at: Spawn<'q, '_>, future: impl PinInit<F>, gfp: Flags) -> Result<Self, BchError> {
        let p = kalloc_pinned_gfp::<Task<'q, F>, BchError>(Task::new(at, future, Self::free), gfp)?;
        Ok(Self(NonNull::new(p).expect("an allocation that succeeded is never null")))
    }

    pub fn get(&mut self) -> Pin<&mut Task<'q, F>> {
        // SAFETY: made in place, never moved; not started, so ours
        unsafe { Pin::new_unchecked(self.0.as_mut()) }
    }

    /// # Safety
    /// Its queue, and what its future borrows, outlive the task.
    pub unsafe fn start_unchecked(self) {
        let p = ManuallyDrop::new(self).0;
        // SAFETY: freed by its done
        unsafe { Pin::new_unchecked(p.as_ref()) }.start();
    }
}

impl<F: Future<Output = ()> + Send + 'static> TaskBox<'static, F> {
    pub fn start(self) {
        // SAFETY: it borrows nothing
        unsafe { self.start_unchecked() }
    }
}

impl<F: Future<Output = ()>> TaskBox<'_, F> {
    /// # Safety
    /// @task is a TaskBox's, not running: its done, or the box's drop.
    unsafe fn free(task: NonNull<Task<'_, F>>) {
        unsafe {
            core::ptr::drop_in_place(task.as_ptr());
            kvfree_rcu(task.as_ptr());
        }
    }
}

impl<F: Future<Output = ()>> Drop for TaskBox<'_, F> {
    fn drop(&mut self) {
        // SAFETY: never started: ours
        unsafe { Self::free(self.0) };
    }
}

/// A TaskBox, started: a task nothing joins.
///
/// # Safety
/// Its queue, and what the future borrows, outlive the task.
pub unsafe fn spawn_detached<F: Future<Output = ()> + Send>(at: Spawn<'_, '_>, future: impl PinInit<F>, gfp: Flags)
    -> Result<(), BchError>
{
    unsafe { TaskBox::new(at, future, gfp)?.start_unchecked() };
    Ok(())
}

// ── A task in its home ───────────────────────────────────────────────────

/// One allocation, @home first, the future borrowing it.
#[repr(C)]
#[pin_data]
struct WithTask<'q, H, F: Future<Output = ()>> {
    #[pin]
    home: H,
    done: unsafe fn(NonNull<H>),
    #[pin]
    task: Task<'q, F>,
}

/// A task in one box with what it's the task of, @H: its owner's, the task
/// borrowing @H, and freed after a grace period - what's in it may be found
/// under RCU. Unstarted, dropping it drops the task; started, its owner drops
/// it only once the task's done.
pub struct BoxWithTask<'q, H> {
    home: NonNull<H>,
    task: TaskRef,
    free: unsafe fn(NonNull<H>),
    /// The task's queue
    _q:   PhantomData<&'q Queue>,
}

// SAFETY: shared as &H, and dropped wherever its owner is
unsafe impl<H: Send + Sync> Send for BoxWithTask<'_, H> {}
unsafe impl<H: Send + Sync> Sync for BoxWithTask<'_, H> {}

impl<'q, H: Send + Sync> BoxWithTask<'q, H> {
    /// @home, and the task of the future @make makes from it, to run @at;
    /// @done its last act, given @home.
    ///
    /// # Safety
    /// What the future borrows outlives the task; @done is the box's owner's
    /// cue that it may drop it, and touches the task no more.
    pub unsafe fn new<'h, M, F>(at: Spawn<'q, '_>, home: impl PinInit<H, BchError>, make: M,
                                done: unsafe fn(NonNull<H>), gfp: Flags) -> Result<Self, BchError>
    where
        H: 'h,
        M: FnOnce(&'h H) -> F,
        F: Future<Output = ()> + Send,
    {
        // SAFETY: @home first: the future borrows it
        let init = unsafe {
            pin_init::pin_init_from_closure(move |slot: *mut WithTask<'q, H, F>| {
                crate::util::alloc::init_at(home, &raw mut (*slot).home)?;
                (&raw mut (*slot).done).write(done);
                let future = make(&*(&raw const (*slot).home));
                let future = pin_init::pin_init_from_closure(move |f: *mut F| {
                    f.write(future);
                    Ok::<(), core::convert::Infallible>(())
                });
                let task = Task::new(at, future, Self::task_done::<F>);
                // on error, its future's dropped; then the home it borrowed
                if let Err(e) = crate::util::alloc::init_at(task, &raw mut (*slot).task) {
                    core::ptr::drop_in_place(&raw mut (*slot).home);
                    return Err(e);
                }
                Ok(())
            })
        };
        let w = kalloc_pinned_gfp::<WithTask<'q, H, F>, BchError>(init, gfp)?;
        // SAFETY: just made
        unsafe {
            Ok(BoxWithTask { home: NonNull::new_unchecked(&raw mut (*w).home),
                             task: (*w).task.task(),
                             free: Self::free::<F>,
                             _q:   PhantomData })
        }
    }

    unsafe fn task_done<F: Future<Output = ()> + Send>(task: NonNull<Task<'q, F>>) {
        let w = unsafe {
            task.as_ptr().byte_sub(core::mem::offset_of!(WithTask<'q, H, F>, task)).cast::<WithTask<'q, H, F>>()
        };
        unsafe { ((*w).done)(NonNull::new_unchecked(&raw mut (*w).home)) };
    }

    /// The task first - an unstarted one's future borrows the home - then
    /// the home.
    ///
    /// # Safety
    /// @home is a WithTask<H, F>'s, its task not running.
    unsafe fn free<F: Future<Output = ()>>(home: NonNull<H>) {
        // SAFETY: the home is first
        unsafe {
            let w = home.as_ptr().cast::<WithTask<'q, H, F>>();
            core::ptr::drop_in_place(&raw mut (*w).task);
            core::ptr::drop_in_place(&raw mut (*w).home);
            kvfree_rcu(w);
        }
    }
}

impl<H> BoxWithTask<'_, H> {
    pub fn task(&self) -> TaskRef {
        self.task
    }

    pub fn as_pin(&self) -> Pin<&H> {
        // SAFETY: made in place, freed only with the box
        unsafe { Pin::new_unchecked(self.home.as_ref()) }
    }
}

impl<H> core::ops::Deref for BoxWithTask<'_, H> {
    type Target = H;

    fn deref(&self) -> &H {
        unsafe { self.home.as_ref() }
    }
}

impl<H> Drop for BoxWithTask<'_, H> {
    fn drop(&mut self) {
        unsafe { (self.free)(self.home) };
    }
}
