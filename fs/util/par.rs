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
//! closure_sync_unbounded() does, what C's fan-outs mostly join with - and
//! scope_freezable()'s is freezable too, for jobs that freeze in place.
//!
//! A job can't borrow its caller's transaction - BtreeTrans isn't Sync - so
//! each gets its own, as C's workers do.
//!
//! A job is a task (util::task) - spawn_unstarted(), on its own kthread or
//! the workqueue - whose future runs the job's closure; joined by its
//! JoinHandle. That's what makes borrowing sound: the scope joins every
//! task it started, and drops every one it didn't, before it returns - as
//! std::thread::scope() joins its threads.

use crate::c;
use crate::errcode::BchError;
use crate::util::alloc::{flags::GFP_KERNEL, KVVec};
use crate::util::task::{spawn_unstarted, JoinHandle, Spawn, Unstarted};
use crate::util::workqueue::Queue;
use crate::util::Printbuf;
use core::cell::RefCell;
use core::fmt;
use core::marker::PhantomData;

/// Where a scope's jobs run.
#[derive(Clone, Copy)]
pub enum Backend<'a> {
    /// A kthread each.
    Kthreads,
    /// On a workqueue.
    Workqueue(&'a Queue),
}

type Ret = Result<(), BchError>;

enum Job {
    Made(Unstarted<Ret>),
    Started(JoinHandle<Ret>),
    Moved,
}

/// The body's: not Sync - only the body spawns.
pub struct Scope<'scope, 'env: 'scope> {
    backend: Backend<'env>,
    /// What the body's spawned, in order.
    jobs:    RefCell<KVVec<Job>>,
    scope:   PhantomData<&'scope mut &'scope ()>,
    env:     PhantomData<&'env mut &'env ()>,
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

/// scope(), the join freezable: for jobs that freeze in place, whose caller
/// must too.
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
        jobs:  RefCell::new(KVVec::new()),
        scope: PhantomData,
        env:   PhantomData,
    };
    let ret = f(&scope);

    // @f failed, or unwound: its jobs dropped unstarted, with @jobs
    let mut jobs = scope.jobs.take();
    ret?;

    for job in jobs.iter_mut() {
        if let Job::Made(u) = core::mem::replace(job, Job::Moved) {
            *job = Job::Started(u.start());
        }
    }

    let mut ret = Ok(());
    for job in jobs.iter_mut() {
        if let Job::Started(h) = core::mem::replace(job, Job::Moved) {
            let r = if freezable { h.join_sync_freezable() } else { h.join_sync() };
            if ret.is_ok() {
                ret = r;
            }
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
        // SAFETY: what @f borrows outlives 'scope, and run_scope() joins or
        // drops every job before 'scope ends; the queue is 'env's
        let job = unsafe {
            match self.backend {
                Backend::Kthreads => spawn_unstarted(Spawn::Thread(name), async move { f() })?,
                Backend::Workqueue(wq) => {
                    let desc = worker_desc(name);
                    spawn_unstarted(Spawn::Queue(wq), async move {
                        c::bch2_set_worker_desc(desc.as_ptr().cast());
                        f()
                    })?
                }
            }
        };
        // not pushed, it's dropped: discarded
        self.jobs.borrow_mut().push(Job::Made(job), GFP_KERNEL)?;
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
