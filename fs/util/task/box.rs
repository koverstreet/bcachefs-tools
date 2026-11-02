// SPDX-License-Identifier: GPL-2.0

//! A task (util::task) in a kmalloc()'d MaybeRcuAlloc of its own: the
//! spawner's till start(), then the task's done's, which takes it back - its
//! memory freed after a grace period, as what's in it may be found under RCU.
//! spawn_detached() is one started at once, for a task nothing joins.

use crate::errcode::BchError;
use crate::util::alloc::Flags;
use crate::util::maybe_rcu_alloc::{Kmalloc, MaybeRcuAlloc};
use crate::util::rcu::RcuHead;
use crate::util::workqueue::Queue;
use super::{Spawn, Task, TaskRef};
use core::future::Future;
use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::pin::Pin;
use core::ptr::NonNull;
use pin_init::PinInit;

/// Started, the box is given up - nothing holds 'q, or what the future
/// borrows, after: so start() is for one that borrows nothing, and
/// start_unchecked() for the rest.
pub struct TaskBox<'q, F: Future<Output = ()>>(MaybeRcuAlloc<Task<'q, F>, Kmalloc>);

impl<'q, F: Future<Output = ()> + Send> TaskBox<'q, F> {
    pub fn new(at: Spawn<'q, '_>, future: impl PinInit<F>, gfp: Flags) -> Result<Self, BchError> {
        MaybeRcuAlloc::new(Task::new(at, future, Self::free), gfp).map(Self)
    }

    pub fn get(&mut self) -> Pin<&mut Task<'q, F>> {
        self.0.as_pin_mut()
    }

    /// # Safety
    /// Its queue, and what its future borrows, outlive the task.
    pub unsafe fn start_unchecked(self) {
        let p = self.0.into_raw();
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
    /// Its done: the allocation start_unchecked() gave up, taken back.
    ///
    /// # Safety
    /// @task is a started TaskBox's, not running.
    unsafe fn free(task: NonNull<Task<'_, F>>) {
        drop(unsafe { MaybeRcuAlloc::<Task<'_, F>, Kmalloc>::from_raw(task) })
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

/// One allocation, the future borrowing @home. The task's head is the free's,
/// so the task goes first: tiny RCU's kvfree_rcu() wants it in the first page.
#[repr(C)]
#[derive(RcuHead)]
struct WithTask<'q, H, F: Future<Output = ()>> {
    #[rcu_head]
    task: ManuallyDrop<Task<'q, F>>,
    home: ManuallyDrop<H>,
    done: unsafe fn(NonNull<H>),
}

impl<H, F: Future<Output = ()>> Drop for WithTask<'_, H, F> {
    /// The task first: an unstarted one's future still borrows the home.
    fn drop(&mut self) {
        unsafe {
            ManuallyDrop::drop(&mut self.task);
            ManuallyDrop::drop(&mut self.home);
        }
    }
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
                let home_p = (&raw mut (*slot).home).cast::<H>();
                crate::util::alloc::init_at(home, home_p)?;
                (&raw mut (*slot).done).write(done);
                let future = make(&*home_p);
                let future = pin_init::pin_init_from_closure(move |f: *mut F| {
                    f.write(future);
                    Ok::<(), core::convert::Infallible>(())
                });
                let task = Task::new(at, future, Self::task_done::<F>);
                // on error, its future's dropped; then the home it borrowed
                if let Err(e) = crate::util::alloc::init_at(task, (&raw mut (*slot).task).cast()) {
                    core::ptr::drop_in_place(home_p);
                    return Err(e);
                }
                Ok(())
            })
        };
        let w = MaybeRcuAlloc::<WithTask<'q, H, F>, Kmalloc>::new(init, gfp)?.into_raw().as_ptr();
        unsafe {
            Ok(BoxWithTask { home: NonNull::new_unchecked((&raw mut (*w).home).cast()),
                             task: (*w).task.task(),
                             free: Self::free::<F>,
                             _q:   PhantomData })
        }
    }

    unsafe fn task_done<F: Future<Output = ()> + Send>(task: NonNull<Task<'q, F>>) {
        let w = unsafe {
            task.as_ptr().byte_sub(core::mem::offset_of!(WithTask<'q, H, F>, task)).cast::<WithTask<'q, H, F>>()
        };
        unsafe { ((*w).done)(NonNull::new_unchecked((&raw mut (*w).home).cast())) };
    }

    /// # Safety
    /// @home is a WithTask<H, F>'s, its task not running.
    unsafe fn free<F: Future<Output = ()>>(home: NonNull<H>) {
        unsafe {
            let w = home.as_ptr().byte_sub(core::mem::offset_of!(WithTask<'q, H, F>, home)).cast::<WithTask<'q, H, F>>();
            drop(MaybeRcuAlloc::<WithTask<'q, H, F>, Kmalloc>::from_raw(NonNull::new_unchecked(w)));
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
