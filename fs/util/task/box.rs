// SPDX-License-Identifier: GPL-2.0

//! A task (util::task) in one allocation with what it's the task of - its
//! home, which its future borrows - its memory freed after a grace period:
//! what's in it may be found under RCU. A TaskSlot is the box before
//! its task: the home made, and the future, which may consume what can't be
//! dropped, like a guard to be ended, made only once there's memory for it.
//!
//! A Shared task's future is made by an async closure handed &H: it may
//! borrow the home, but only for as long as the closure's call, which no
//! lifetime it can name outlives - so it can't keep the borrow past the task -
//! and others may meanwhile, through the box; with no done of its owner's,
//! the box frees itself when its task is done. An Exclusive task's is what a
//! closure returns, handed Pin<&mut H>: a future with a name, if it's an
//! #[in_place] fn's, so a pool can be sized for its box - exclusive_size().
//! Started, the box is the task's alone, freed when it's done: spawn_detached()'s,
//! a Join's. A Join is a task its spawner awaits, for what it returns, through
//! a CompletionReturn.

use crate::c;
use crate::errcode::BchError;
use crate::util::alloc::{init_at, Alloc, Kmalloc};
use crate::util::rcu::{Flavor, HasRcuHead, RcuHead};
use crate::os::workqueue::Queue;
use super::{waiter, CompletionReturn, DoneWith, Spawn, Task, TaskRef};
use core::alloc::Layout;
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

/// An Exclusive task's future: what @M returns, given the home as its own -
/// a type with a name, if M calls an #[in_place] fn, so a pool can be sized
/// for it.
type FutureOfMut<M, H> = <M as FnOnce<(Pin<&'static mut H>,)>>::Output;

/// A TaskSlot whose task shares its home: the future has &H, and so may the
/// box's owner, who keeps the box while the task runs.
pub struct Shared;

/// A TaskSlot whose task has its home to itself: the future has Pin<&mut H>,
/// and started, the box is the task's alone - freed when it's done.
pub struct Exclusive;

/// One allocation, from @A: the task of a future borrowing @home, then the
/// home, and what frees it - A, and the size it gave. The task's head is the
/// free's, so the task goes first: tiny RCU's kvfree_rcu() wants it in the
/// first page.
#[repr(C)]
#[derive(RcuHead)]
struct WithTask<'q, H, F: Future<Output = ()>, A: Alloc + Copy> {
    #[rcu_head]
    task:  MaybeUninit<Task<'q, F>>,
    home:  H,
    done:  Option<unsafe fn(NonNull<H>)>,
    alloc: A,
    given: usize,
}

impl<H, F: Future<Output = ()>, A: Alloc + Copy> WithTask<'_, H, F, A> {
    /// # Safety
    /// @home is a WithTask<H, F>'s.
    unsafe fn of_home(home: NonNull<H>) -> NonNull<Self> {
        unsafe { home.byte_sub(core::mem::offset_of!(Self, home)).cast() }
    }
}

/// A TaskBox's memory, its home made and its task not yet: allocated apart
/// from making the future, a failed allocation consumes none of what the
/// future would. @K is how the task has its home, Shared or Exclusive. Not
/// bounded by @M's AsyncFnOnce: a bound naming its future type would be what
/// rustc infers the closure's signature from, and that's for one lifetime,
/// not every one.
pub struct TaskSlot<'q, H, M, A: Alloc + Copy = Kmalloc, K = Shared> {
    home: NonNull<H>,
    /// The home dropped, and the memory freed
    free: unsafe fn(NonNull<H>),
    _p:   PhantomData<(&'q Queue, fn() -> M, A, K)>,
}

// Its home, which nothing else has yet, as a Box's
unsafe impl<H: Send, M, A: Alloc + Copy + Send, K> Send for TaskSlot<'_, H, M, A, K> {}

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
        Self::alloc_in(Kmalloc, home)
    }
}

impl<'q, H, M, A: Alloc + Copy> TaskSlot<'q, H, M, A>
where
    H: Send + Sync + 'static,
    M: for<'h> AsyncFnOnce<(&'h H,), Output = ()>,
    for<'h> <M as AsyncFnOnce<(&'h H,)>>::CallOnceFuture: Send,
{
    /// alloc(), from @alloc.
    pub fn alloc_in<E>(alloc: A, home: impl PinInit<H, E>) -> Result<Self, BchError>
    where
        BchError: From<E>,
    {
        let home = unsafe { alloc_mem::<H, FutureOf<M, H>, A, E, _>(alloc, Layout::new::<()>(), |_| home)? };
        Ok(Self { home, free: free_slot::<H, FutureOf<M, H>, A>, _p: PhantomData })
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
            let task = init_task::<H, _, A>(home, at, move || make(home.as_ref()), done)?;
            Ok(TaskBox { home, task, free: free::<H, FutureOf<M, H>, A>, _q: PhantomData })
        }
    }
}

impl<'q, H, M, A: Alloc + Copy> TaskSlot<'q, H, M, A, Exclusive>
where
    H: Send + 'static,
    M: FnOnce<(Pin<&'static mut H>,)>,
    FutureOfMut<M, H>: Future<Output = ()> + Send,
{
    /// An Exclusive task's TaskSlot, from @alloc.
    pub fn exclusive_in<E>(alloc: A, home: impl PinInit<H, E>) -> Result<Self, BchError>
    where
        BchError: From<E>,
    {
        Self::exclusive_tail_in(alloc, Layout::new::<()>(), |_| home)
    }

    /// exclusive_in(), with @tail after the box, in the same allocation - a
    /// bio's bvecs - the home made by @home from where it is.
    pub fn exclusive_tail_in<E, I>(alloc: A, tail: Layout, home: impl FnOnce(NonNull<u8>) -> I)
        -> Result<Self, BchError>
    where
        I: PinInit<H, E>,
        BchError: From<E>,
    {
        let home = unsafe { alloc_mem::<H, FutureOfMut<M, H>, A, E, I>(alloc, tail, home)? };
        Ok(Self { home, free: free_slot::<H, FutureOfMut<M, H>, A>, _p: PhantomData })
    }

    /// The task of the future @make makes from the home, started to run on
    /// @q: the box is the task's from here, freed when it's done.
    ///
    /// # Safety
    /// @q, and what @make holds and its future borrows beyond the home,
    /// outlive the task; the home, handed @make as 'static, is borrowed by
    /// nothing but the future it makes.
    pub unsafe fn start(self, q: &'q Queue, make: M) {
        unsafe { self.make(q, make).start() }
    }

    /// start(), its first poll run here.
    ///
    /// # Safety
    /// As start()'s.
    pub unsafe fn start_inline(self, q: &'q Queue, make: M) {
        unsafe { self.make(q, make).start_inline() }
    }

    unsafe fn make(self, q: &'q Queue, make: M) -> TaskRef {
        let home = ManuallyDrop::new(self).home;
        unsafe {
            init_task::<H, _, A>(home, Spawn::Queue(q), move || make(Pin::new_unchecked(&mut *home.as_ptr())), None)
        }.expect("a queue's task: only a thread's fails to start")
    }
}

/// The size of an Exclusive box of @H and @F's task, from @A, with @tail:
/// what a pool's elements are, for one.
pub fn exclusive_size<H, F: Future<Output = ()>, A: Alloc + Copy>(tail: Layout) -> usize {
    box_layout::<H, F, A>(tail).0.size()
}

/// A box with @tail after it, and where the tail is.
fn box_layout<'q, H, F: Future<Output = ()>, A: Alloc + Copy>(tail: Layout) -> (Layout, usize) {
    let (layout, at) = Layout::new::<WithTask<'q, H, F, A>>().extend(tail).expect("a box and its tail");
    (layout.pad_to_align(), at)
}

impl<H, M, A: Alloc + Copy, K> TaskSlot<'_, H, M, A, K> {
    /// The home, before there's a task to borrow it.
    pub fn home(&mut self) -> Pin<&mut H> {
        unsafe { Pin::new_unchecked(self.home.as_mut()) }
    }
}

impl<H, M, A: Alloc + Copy, K> Drop for TaskSlot<'_, H, M, A, K> {
    fn drop(&mut self) {
        unsafe { (self.free)(self.home) }
    }
}

/// The memory of a box from @alloc, and @tail after it - A and the size it
/// gave written in for its free - and the home made in it, by what @home
/// makes of where the tail is. Not zeroed: each of its fields is written, and
/// a pool's element is as big as a bio with BIO_MAX_VECS bvecs.
///
/// # Safety
/// F is the future the box's task will be.
unsafe fn alloc_mem<'q, H, F: Future<Output = ()>, A: Alloc + Copy, E, I>(
    alloc: A, tail: Layout, home: impl FnOnce(NonNull<u8>) -> I,
) -> Result<NonNull<H>, BchError>
where
    I: PinInit<H, E>,
    BchError: From<E>,
{
    let (layout, at) = box_layout::<H, F, A>(tail);
    let mem = alloc.alloc(layout)?;
    let w = mem.cast::<WithTask<'q, H, F, A>>().as_ptr();
    unsafe {
        (&raw mut (*w).alloc).write(alloc);
        (&raw mut (*w).given).write(mem.len());
        if let Err(e) = init_at(home(mem.cast::<u8>().add(at)), &raw mut (*w).home) {
            alloc.free(mem.cast(), Layout::from_size_align_unchecked(mem.len(), layout.align()));
            return Err(e.into());
        }
        Ok(NonNull::new_unchecked(&raw mut (*w).home))
    }
}

/// The task of @make's future in @home's box, to run @at: the box freed if it
/// can't be made.
///
/// # Safety
/// @home is alloc_mem::<H, F, A>()'s, its task not made, and @make's future
/// borrows nothing of it but the home.
unsafe fn init_task<'q, H, F: Future<Output = ()> + Send, A: Alloc + Copy>(
    home: NonNull<H>, at: Spawn<'q, '_>, make: impl FnOnce() -> F, done: Option<unsafe fn(NonNull<H>)>,
) -> Result<TaskRef, BchError> {
    unsafe {
        let w = WithTask::<'q, H, F, A>::of_home(home).as_ptr();
        (&raw mut (*w).done).write(done);
        // made where it lives: a future is its whole state machine, and a
        // move of one is a copy of all of it - what's moved here is what
        // @make holds, its args
        let future = ::pin_init::pin_init_from_closure(move |slot: *mut F| {
            slot.write(make());
            Ok::<_, core::convert::Infallible>(())
        });
        let task = Task::new(at, future, task_done::<H, F, A>);
        if let Err(e) = init_at(task, (&raw mut (*w).task).cast()) {
            free_slot::<H, F, A>(home);
            return Err(e);
        }
        Ok((*w).task.assume_init_ref().task())
    }
}

/// A box's task's done: its owner's, or the box freed.
///
/// # Safety
/// The executor's, as the task's done function: @task is finished.
unsafe fn task_done<'q, H, F: Future<Output = ()>, A: Alloc + Copy>(task: NonNull<Task<'q, F>>, (): ()) {
    unsafe {
        let w = task.as_ptr().byte_sub(core::mem::offset_of!(WithTask<'q, H, F, A>, task))
            .cast::<WithTask<'q, H, F, A>>();
        let home = NonNull::new_unchecked(&raw mut (*w).home);
        match (*w).done {
            Some(done) => done(home),
            None => free::<H, F, A>(home),
        }
    }
}

/// A TaskSlot's free: the home made, the task never. The home is dropped now,
/// and the memory given back to A a grace period on: what's in it may be
/// found under RCU - by kvfree_rcu(), batched, if kvfree() frees it.
///
/// # Safety
/// @home is a WithTask<H, F, A>'s, its task not made or dropped.
unsafe fn free_slot<H, F: Future<Output = ()>, A: Alloc + Copy>(home: NonNull<H>) {
    unsafe {
        let w = WithTask::<'_, H, F, A>::of_home(home);
        core::ptr::drop_in_place(home.as_ptr());
        let head = w.byte_add(<WithTask<'_, H, F, A> as HasRcuHead>::HEAD).as_ptr().cast::<c::rcu_head>();
        #[cfg(kernel)]
        if A::KVFREE {
            // tiny RCU passes the head's offset as its callback
            const { assert!(<WithTask<'_, H, F, A> as HasRcuHead>::HEAD < 4096) };
            return crate::util::alloc::kvfree_rcu_head(w.as_ptr().cast(), head.cast());
        }
        Flavor::RCU.call(head, free_mem::<H, F, A>);
    }
}

unsafe extern "C" fn free_mem<H, F: Future<Output = ()>, A: Alloc + Copy>(head: *mut c::rcu_head) {
    unsafe {
        let w = head.byte_sub(<WithTask<'_, H, F, A> as HasRcuHead>::HEAD).cast::<WithTask<'_, H, F, A>>();
        let (alloc, given) = ((*w).alloc, (*w).given);
        alloc.free(NonNull::new_unchecked(w).cast(),
                   Layout::from_size_align_unchecked(given, align_of::<WithTask<'_, H, F, A>>()));
    }
}

/// The task first - an unstarted one's future borrows the home - then the
/// home.
///
/// # Safety
/// @home is a WithTask<H, F, A>'s, its task made and not running.
unsafe fn free<H, F: Future<Output = ()>, A: Alloc + Copy>(home: NonNull<H>) {
    unsafe {
        let w = WithTask::<'_, H, F, A>::of_home(home);
        core::ptr::drop_in_place((*w.as_ptr()).task.as_mut_ptr());
        free_slot::<H, F, A>(home);
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

/// A task nothing joins, in a box of its own, started on @q: @make's future,
/// made in the box - the caller's frame holds only what @make captures, not
/// the state machine.
///
/// # Safety
/// @q, and what the future borrows, outlive the task.
pub unsafe fn spawn_detached<F: Future<Output = ()> + Send>(q: &Queue, make: impl FnOnce() -> F)
    -> Result<(), BchError>
{
    unsafe { TaskSlot::exclusive_in(Kmalloc, ())?.start(q, move |_| make()) };
    Ok(())
}

/// spawn_detached(), for a task that borrows nothing, on a queue that lasts.
pub fn spawn_static<F: Future<Output = ()> + Send + 'static>(q: &'static Queue, make: impl FnOnce() -> F)
    -> Result<(), BchError>
{
    unsafe { spawn_detached(q, make) }
}

/// A task its spawner awaits, for what it returns: its CompletionReturn's
/// DoneWith, in the spawner's frame. The task completes it as its last act,
/// touching the spawner no more. spawn() is a plain call, so the task's
/// future goes straight into its box - a spawner that's a future holds the
/// Join and the CompletionReturn, not the task's state. The box is had before
/// the Done's armed, so failing to allocate it leaves nothing to wait on.
pub struct Join<T>(Option<DoneWith<T>>);

impl<T: Send> Join<T> {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Join(None)
    }

    /// @make's future run as a task on @q, for @waiter to wait on - the
    /// CompletionReturn, which outlives nothing the future borrows. Made in
    /// the box, as spawn_detached()'s.
    pub fn spawn<'a, F>(&'a mut self, q: &Queue, make: impl FnOnce() -> F + Send + 'a, waiter: waiter::Async)
        -> Result<CompletionReturn<'a, T>, BchError>
    where
        F: Future<Output = T> + Send + 'a,
    {
        let slot = TaskSlot::exclusive_in(Kmalloc, ())?;
        let (wait, ret) = CompletionReturn::new(&mut self.0, waiter);
        unsafe { slot.start(q, move |_| async move { ret.complete(make().await) }) };
        Ok(wait)
    }
}
