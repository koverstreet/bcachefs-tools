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
//! Started, the box is the task's alone, freed when it's done: spawn()'s,
//! a Join's. A Join is a task its spawner awaits, for what it returns, through
//! a CompletionReturn. Started in a Scope (util::gather), what it returns is
//! put to the scope once its future and home are dropped: so they may borrow
//! what the scope's body lends, and the start is safe.

use crate::errcode::BchError;
use crate::util::alloc::{init_at, init_with, Alloc, Kmalloc};
use crate::util::gather::{Fold, GatherRef, Scope, Width};
use crate::util::rcu::{DropAnytime, DropAtomic, Flavor, HasRcuHead, RcuHead, RcuOwned};
use crate::os::workqueue::Queue;
use super::{waiter, CompletionReturn, DoneWith, Spawn, Task, TaskList, TaskRef, Waiter};
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

/// An Exclusive task's future: what @M returns, given the home as its own for
/// 'q - a type with a name, if M calls an #[in_place] fn, so a pool can be
/// sized for it.
type FutureOfMut<'q, M, H> = <M as FnOnce<(Pin<&'q mut H>,)>>::Output;

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
struct WithTask<'q, H, F: Future, A: Alloc + Copy> {
    #[rcu_head]
    task:  MaybeUninit<Task<'q, F>>,
    home:  H,
    done:  Option<unsafe fn(NonNull<H>)>,
    /// A scope's task's ref on it, as into_raw() made it: written by
    /// start_in(), put by scoped_done()
    scope: *const (),
    alloc: A,
    given: usize,
}

impl<H, F: Future, A: Alloc + Copy> WithTask<'_, H, F, A> {
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
        let home = unsafe { alloc_mem::<H, FutureOf<M, H>, A, E>(alloc, home)? };
        Ok(Self { home, free: free_slot::<H, FutureOf<M, H>, A>, _p: PhantomData })
    }

    /// The task of the future @make makes from the home, to run @at. @done is
    /// the box's owner's cue that the task is done and it may drop the box;
    /// with none, the box frees itself. What @make holds outlives the box,
    /// unstarted: started, start_unchecked() says how long the task runs.
    pub fn init(self, at: Spawn<'q, '_>, list: &'q TaskList, make: M, done: Option<unsafe fn(NonNull<H>)>)
        -> Result<TaskBox<'q, H>, BchError>
    where
        M: 'q,
    {
        let home = ManuallyDrop::new(self).home;
        unsafe {
            let task = init_task::<H, _, A>(home, at, list, move || make(home.as_ref()), done, task_done::<H, _, A>)?;
            Ok(TaskBox { home, task, free: free::<H, FutureOf<M, H>, A>, _q: PhantomData })
        }
    }
}

impl<'q, H, M, A: Alloc + Copy> TaskSlot<'q, H, M, A, Exclusive>
where
    H: Send + 'q,
    M: FnOnce<(Pin<&'q mut H>,)>,
    FutureOfMut<'q, M, H>: Future + Send,
{
    /// An Exclusive task's TaskSlot, from @alloc.
    pub fn exclusive_in<E>(alloc: A, home: impl PinInit<H, E>) -> Result<Self, BchError>
    where
        BchError: From<E>,
    {
        let home = unsafe { alloc_mem::<H, FutureOfMut<'q, M, H>, A, E>(alloc, home)? };
        Ok(Self { home, free: free_slot::<H, FutureOfMut<'q, M, H>, A>, _p: PhantomData })
    }

    /// The task of the future @make makes from the home, started @at: the
    /// box is the task's from here, freed when it's done. @make is called and
    /// dropped before this returns, so what it holds need only outlive the
    /// call.
    ///
    /// # Safety
    /// @at's queue, and what the home and the future borrow, outlive the
    /// task; the home, handed @make for 'q, is borrowed by nothing but the
    /// future it makes.
    pub unsafe fn start_unchecked(self, at: Spawn<'_, '_>, list: &TaskList, make: M)
    where
        FutureOfMut<'q, M, H>: Future<Output = ()>,
    {
        let home = ManuallyDrop::new(self).home;
        unsafe {
            init_task::<H, _, A>(home, at, list, move || make(Pin::new_unchecked(&mut *home.as_ptr())), None,
                                 task_done::<H, _, A>)
                .expect("a task: only a thread's fails to start")
                .start()
        }
    }

    /// The task of the future @make makes from the home, started @at on @s:
    /// what it returns is put to @s, once the box has dropped the future and
    /// the home - so they may borrow for 'q, @s's, and the home's lent for
    /// it, which what's returned, 'static, can't hold. Its memory's given
    /// back a grace period on, to @A, which outlives that.
    pub fn start_in<T, Wt, W>(self, s: &Scope<'q, T, Wt, W>, at: Spawn<'q, '_>, list: &'q TaskList, make: M)
    where
        T: Fold<Item: 'static>,
        Wt: Waiter,
        W: Width,
        H: DropAtomic + DropAnytime,
        A: 'static,
        M: 'q,
        FutureOfMut<'q, M, H>: Future<Output = T::Item> + 'q,
    {
        let home = ManuallyDrop::new(self).home;
        unsafe {
            let task = init_task::<H, _, A>(home, at, list, move || make(Pin::new_unchecked(&mut *home.as_ptr())),
                                            None, scoped_done::<H, _, A, T, Wt, W>)
                .expect("a task: only a thread's fails to start");
            let w = WithTask::<H, FutureOfMut<'q, M, H>, A>::of_home(home).as_ptr();
            (&raw mut (*w).scope).write(s.get().into_raw().cast());
            task.start()
        }
    }
}

/// The size of an Exclusive box of @H and @F's task, from @A: what a pool's
/// elements are, for one.
pub fn exclusive_size<H, F: Future, A: Alloc + Copy>() -> usize {
    size_of::<WithTask<'static, H, F, A>>()
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

/// The memory of a box from @alloc - A and the size it gave written in for
/// its free - and the home made in it by @home. Not zeroed: each of its fields
/// is written.
///
/// # Safety
/// F is the future the box's task will be.
unsafe fn alloc_mem<'q, H, F: Future, A: Alloc + Copy, E>(
    alloc: A, home: impl PinInit<H, E>,
) -> Result<NonNull<H>, BchError>
where
    BchError: From<E>,
{
    let layout = Layout::new::<WithTask<'q, H, F, A>>();
    let mem = alloc.allocate(layout)?;
    let w = mem.cast::<WithTask<'q, H, F, A>>().as_ptr();
    unsafe {
        (&raw mut (*w).alloc).write(alloc);
        (&raw mut (*w).given).write(mem.len());
        if let Err(e) = init_at(home, &raw mut (*w).home) {
            alloc.deallocate(mem.cast(), Layout::from_size_align_unchecked(mem.len(), layout.align()));
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
unsafe fn init_task<'q, H, F: Future + Send, A: Alloc + Copy>(
    home: NonNull<H>, at: Spawn<'q, '_>, list: &'q TaskList, make: impl FnOnce() -> F,
    done: Option<unsafe fn(NonNull<H>)>, task_done: unsafe fn(NonNull<Task<'q, F>>, F::Output),
) -> Result<TaskRef, BchError> {
    unsafe {
        let w = WithTask::<'q, H, F, A>::of_home(home).as_ptr();
        (&raw mut (*w).done).write(done);
        // made where it lives: a future is its whole state machine, and a
        // move of one is a copy of all of it - what's moved here is what
        // @make holds, its args
        let future = init_with(make);
        let task = Task::new(at, list, future, task_done);
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

/// A TaskSlot's free: the home made, the task never, or gone. What's in it may
/// be found under RCU, so the memory's given back to A a grace period on.
///
/// # Safety
/// @home is a WithTask<H, F, A>'s, its task not made or dropped.
unsafe fn free_slot<H, F: Future, A: Alloc + Copy>(home: NonNull<H>) {
    unsafe {
        let w = WithTask::<'_, H, F, A>::of_home(home);
        if <H as HomeDrop>::deferred() {
            return Flavor::RCU.call(SlotMem::<H, F, A>(w.cast()), SlotMem::drop_free);
        }
        core::ptr::drop_in_place(home.as_ptr());
        #[cfg(kernel)]
        if A::KVFREE {
            // tiny RCU passes the head's offset as its callback
            const { assert!(<WithTask<'_, H, F, A> as HasRcuHead>::HEAD < 4096) };
            let head = w.byte_add(<WithTask<'_, H, F, A> as HasRcuHead>::HEAD).as_ptr();
            return crate::util::alloc::kvfree_rcu_head(w.as_ptr().cast(), head.cast());
        }
        Flavor::RCU.call(SlotMem::<H, F, A>(w.cast()), SlotMem::free);
    }
}

/// A scope's task's done: the task and its home dropped, as start_in()'s
/// home is at once, and last @out put to the scope - so nothing the scope
/// lent is touched once it's put.
///
/// # Safety
/// The executor's, as the task's done function: @task is finished, and its
/// box's scope written by start_in().
unsafe fn scoped_done<'q, H, F, A, T, Wt, W>(task: NonNull<Task<'q, F>>, out: T::Item)
where
    F: Future<Output = T::Item>,
    A: Alloc + Copy,
    T: Fold,
    Wt: Waiter,
    W: Width,
{
    unsafe {
        let w = task.byte_sub(core::mem::offset_of!(WithTask<'q, H, F, A>, task)).cast::<WithTask<'q, H, F, A>>();
        let r = GatherRef::<'q, T, Wt, W>::from_raw((*w.as_ptr()).scope.cast());
        free::<H, F, A>(NonNull::new_unchecked(&raw mut (*w.as_ptr()).home));
        r.put(out);
    }
}

/// When a home's dropped, as RcuFreeing's markers say: at once - or, with an
/// RcuDropMember, its readers reaching what its drop frees, a grace period on,
/// in the RCU callback.
trait HomeDrop {
    fn deferred() -> bool;
}

impl<H> HomeDrop for H {
    default fn deferred() -> bool {
        let () = DropSleeps::<H>::ERR;
        true
    }
}

impl<H: DropAtomic> HomeDrop for H {
    default fn deferred() -> bool {
        true
    }
}

impl<H: DropAtomic + DropAnytime> HomeDrop for H {
    fn deferred() -> bool {
        false
    }
}

struct DropSleeps<H>(PhantomData<H>);

impl<H> DropSleeps<H> {
    const ERR: () = panic!("a task's home dropped a grace period on, on the free queue - an RcuDropWorkMember's: \
                            not done yet");
}

/// A WithTask's memory, its task gone, and its home - but an RcuDropMember's,
/// dropped with it: A's again a grace period on.
struct SlotMem<H, F: Future, A: Alloc + Copy>(NonNull<WithTask<'static, H, F, A>>);

// memory, which any thread may give back to A - and an RcuDropMember's home,
// which any thread may drop: every home is Send, as TaskSlot's makers require
unsafe impl<H, F: Future, A: Alloc + Copy> Send for SlotMem<H, F, A> {}

// its home and task gone, the memory is free_slot()'s alone
unsafe impl<H, F: Future, A: Alloc + Copy> RcuOwned for SlotMem<H, F, A> {
    fn into_head(self) -> NonNull<RcuHead> {
        unsafe { self.0.byte_add(<WithTask<'_, H, F, A> as HasRcuHead>::HEAD).cast() }
    }

    unsafe fn from_head(head: NonNull<RcuHead>) -> Self {
        SlotMem(unsafe { head.byte_sub(<WithTask<'_, H, F, A> as HasRcuHead>::HEAD).cast() })
    }
}

impl<H, F: Future, A: Alloc + Copy> SlotMem<H, F, A> {
    /// An RcuDropMember's home dropped, then its memory freed.
    fn drop_free(self) {
        unsafe { core::ptr::drop_in_place(&raw mut (*self.0.as_ptr()).home) };
        self.free()
    }

    fn free(self) {
        let w = self.0.as_ptr();
        unsafe {
            let (alloc, given) = ((*w).alloc, (*w).given);
            alloc.deallocate(self.0.cast(),
                             Layout::from_size_align_unchecked(given, align_of::<WithTask<'_, H, F, A>>()));
        }
    }
}

/// The task first - an unstarted one's future borrows the home - then the
/// home.
///
/// # Safety
/// @home is a WithTask<H, F, A>'s, its task made and not running.
unsafe fn free<H, F: Future, A: Alloc + Copy>(home: NonNull<H>) {
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

impl<H> TaskBox<'static, H> {
    /// Started: its queue lasts, and its future borrows nothing beyond its
    /// home.
    pub fn start(self) {
        unsafe { self.start_unchecked() }
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

/// A task nothing joins, on the queue and task list @on finds through
/// @held: @make's future, in a box with @held, which it borrows. @held is
/// lent only as &H and dropped after the future, as the box frees itself, so
/// what @on found lasts the task - but for its work returning, which the
/// queue's drain waits on: the box borrows nothing outside itself.
pub fn spawn<H, M>(held: H, on: impl FnOnce(&H) -> (&Queue, &TaskList), make: M) -> Result<(), BchError>
where
    H: Send + Sync + 'static,
    M: for<'h> AsyncFnOnce<(&'h H,), Output = ()> + 'static,
    for<'h> <M as AsyncFnOnce<(&'h H,)>>::CallOnceFuture: Send,
{
    let slot = TaskSlot::alloc(held)?;
    let (q, list) = on(unsafe { slot.home.as_ref() });
    slot.init(Spawn::Queue(q), list, make, None)?.start();
    Ok(())
}

/// A task its spawner awaits, for what it returns: its CompletionReturn's
/// DoneWith, in the spawner's frame. The task completes it as its last act,
/// touching the spawner no more. spawn() is a plain call, so the task's
/// future goes straight into its box - a spawner that's a future holds the
/// Join and the CompletionReturn, not the task's state. The box is had before
/// the Done's armed, so failing to allocate it leaves nothing to wait on.
pub struct Join<T>(DoneWith<T>);

impl<T: Send> Join<T> {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Join(DoneWith::new())
    }

    /// @make's future run as a task on @q, for @waiter to wait on - the
    /// CompletionReturn, which outlives nothing the future borrows. Made in
    /// the box, as spawn()'s.
    pub fn spawn<'a, F>(&'a mut self, q: &Queue, list: &TaskList, make: impl FnOnce() -> F + Send + 'a,
                        waiter: waiter::Async) -> Result<CompletionReturn<'a, T>, BchError>
    where
        F: Future<Output = T> + Send + 'a,
    {
        let slot = TaskSlot::exclusive_in(Kmalloc, ())?;
        let (wait, ret) = CompletionReturn::new(&mut self.0, waiter);
        unsafe { slot.start_unchecked(Spawn::Queue(q), list, move |_| async move { ret.complete(make().await) }) };
        Ok(wait)
    }
}
