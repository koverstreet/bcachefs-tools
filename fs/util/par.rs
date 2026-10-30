// SPDX-License-Identifier: GPL-2.0

//! Fork-join, as rayon's scope() and std::thread::scope(): jobs run in
//! parallel - each on a kthread of its own, or on a workqueue, the caller's
//! choice - borrowing from their caller, and all joined before scope()
//! returns. A job's name is its kthread's, seen in ps - a thread's cost
//! each - or its worker's description while it runs, in workqueue dumps and
//! backtraces, a workqueue's workers being lighter. For a long-lived thread
//! of one's own, a daemon, there's util::kthread.
//!
//! The jobs the scope's body spawns are made, not started, until it returns:
//! then they're started together - if it succeeded. If it didn't, none of
//! them runs, and its error is the scope's; a job's error is the scope's
//! only when its body succeeded, the first by the order they were spawned.
//!
//! Only the body spawns: a Scope isn't Sync, so a job can't take it along.
//! Nothing needs a job spawning more; when something does, rayon's way -
//! a job handed a scope of its own - is how it'd go.
//!
//! The join sleeps TASK_IDLE, without hung task warnings - as
//! closure_sync_unbounded() does, what C's fan-outs mostly join with.
//! scope_freezable() is for jobs that freeze in place: its join is
//! freezable, and so are its kthreads - each job makes its own so
//! (Kthread::set_freezable()); a workqueue's workers are as the queue is,
//! WQ_FREEZABLE.
//!
//! A job can't borrow its caller's transaction - BtreeTrans isn't Sync - so
//! each gets its own, as C's workers do.
//!
//! A job is a task (util::task) in a BoxWithTask, on its own kthread or the
//! workqueue, whose future runs the job's closure and keeps what it returned
//! in the box. Started, it holds a ref on the scope's join, a Gather, which
//! its done puts - its last touch - and the scope frees it after the join.
//! That's what makes borrowing sound: the scope joins every job it started,
//! and drops every one it didn't, before it returns - as std::thread::scope()
//! joins its threads.

use crate::c;
use crate::errcode::BchError;
use crate::util::alloc::{flags::GFP_KERNEL, KVVec};
use crate::util::gather::{Gather, GatherRef};
use crate::util::kthread::Kthread;
use crate::util::task::{waiter, BoxWithTask, Spawn};
use crate::util::workqueue::Queue;
use crate::util::Printbuf;
use core::cell::{RefCell, UnsafeCell};
use core::fmt;
use core::marker::PhantomData;
use core::ptr::NonNull;
use pin_init::{init, stack_pin_init};

/// Where a scope's jobs run.
#[derive(Clone, Copy)]
pub enum Backend<'a> {
    /// A kthread each.
    Kthreads,
    /// On a workqueue.
    Workqueue(&'a Queue),
}

type Ret = Result<(), BchError>;

/// A job's, beside its task: what it returned, and its ref on the join.
struct JobHome {
    ret:  UnsafeCell<Option<Ret>>,
    join: UnsafeCell<Option<GatherRef<(), waiter::Sync>>>,
}

// SAFETY: @ret is the job's to write, then the scope's to read after the
// join; @join the scope's to set before the start, then the job's done's
unsafe impl Send for JobHome {}
unsafe impl Sync for JobHome {}

type Job<'env> = BoxWithTask<'env, JobHome>;

/// # Safety
/// @home is a started job's: its last touch.
unsafe fn job_done(home: NonNull<JobHome>) {
    drop(unsafe { (*(*home.as_ptr()).join.get()).take() });
}

/// The body's: not Sync - only the body spawns.
pub struct Scope<'scope, 'env: 'scope> {
    backend:   Backend<'env>,
    freezable: bool,
    /// What the body's spawned, in order.
    jobs:      RefCell<KVVec<Job<'env>>>,
    scope:     PhantomData<&'scope mut &'scope ()>,
    env:       PhantomData<&'env mut &'env ()>,
}

/// Run @f, which spawns jobs on @backend that may borrow what outlives the
/// call; if it succeeded, start them, and join them: @f's error, or else the
/// first a job returned.
pub fn scope<'env, F>(backend: Backend<'env>, f: F) -> Ret
where
    F: for<'scope> FnOnce(&'scope Scope<'scope, 'env>) -> Ret,
{
    run_scope(backend, false, f)
}

/// scope(), for jobs that freeze in place: their kthreads freezable, and the
/// join, as their caller must be too.
pub fn scope_freezable<'env, F>(backend: Backend<'env>, f: F) -> Ret
where
    F: for<'scope> FnOnce(&'scope Scope<'scope, 'env>) -> Ret,
{
    run_scope(backend, true, f)
}

fn run_scope<'env, F>(backend: Backend<'env>, freezable: bool, f: F) -> Ret
where
    F: for<'scope> FnOnce(&'scope Scope<'scope, 'env>) -> Ret,
{
    let scope = Scope {
        backend,
        freezable,
        jobs:  RefCell::new(KVVec::new()),
        scope: PhantomData,
        env:   PhantomData,
    };
    let ret = f(&scope);

    // @f failed, or unwound: its jobs dropped unstarted, with @jobs
    let jobs = scope.jobs.take();
    ret?;

    stack_pin_init!(let join = Gather::<(), waiter::Sync>::new());
    for job in jobs.iter() {
        // SAFETY: set before its start, taken by its done
        unsafe { *job.join.get() = Some(join.get()) };
        // SAFETY: freed only after the join
        unsafe { job.task().start() };
    }
    if freezable {
        join.wait_sync_freezable();
    } else {
        join.wait_sync();
    }

    let mut ret = Ok(());
    for job in jobs.iter() {
        // SAFETY: joined
        let r = unsafe { (*job.ret.get()).take() }.expect("a joined job's return");
        if ret.is_ok() {
            ret = r;
        }
    }
    ret
}

impl<'scope, 'env> Scope<'scope, 'env> {
    /// @f, named @name, made to run on the scope's backend when the body
    /// returns. Nothing's run if it can't be made.
    pub fn spawn<F>(&'scope self, name: fmt::Arguments<'_>, f: F) -> Ret
    where
        F: FnOnce() -> Ret + Send + 'scope,
    {
        let (at, desc, freezable) = match self.backend {
            Backend::Kthreads => (Spawn::Thread(name), None, self.freezable),
            // a worker's freezable as its queue is
            Backend::Workqueue(wq) => (Spawn::Queue(wq), Some(worker_desc(name)), false),
        };
        let home = init!(JobHome { ret: UnsafeCell::new(None), join: UnsafeCell::new(None) }? BchError);
        // SAFETY: what @f borrows outlives 'scope, and run_scope() joins or
        // drops every job before 'scope ends; the queue is 'env's
        let job = unsafe {
            BoxWithTask::new(at, home, move |home: &JobHome| async move {
                if freezable {
                    Kthread::set_freezable();
                }
                if let Some(desc) = desc {
                    c::bch2_set_worker_desc(desc.as_ptr().cast());
                }
                *home.ret.get() = Some(f());
            }, job_done, GFP_KERNEL)
        }?;
        // not pushed, it's dropped: discarded
        self.jobs.borrow_mut().push(job, GFP_KERNEL)?;
        Ok(())
    }
}

/// @name, as set_worker_desc() takes it: WORKER_DESC_LEN, NUL included -
/// it'd truncate there anyway.
fn worker_desc(name: fmt::Arguments<'_>) -> [u8; 32] {
    let mut buf = Printbuf::new();
    write!(buf, "{name}");
    let name = buf.as_c_str().to_bytes();
    let mut desc = [0u8; 32];
    let len = name.len().min(desc.len() - 1);
    desc[..len].copy_from_slice(&name[..len]);
    desc
}
