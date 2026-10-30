//! What util::task has to guarantee on the userspace workqueue shim,
//! exercised end to end.
//!
//! These can't live beside `fs/util/task.rs`: a `cargo test -p
//! bcachefs-kernel` binary links `-lbcachefs_static_wrappers` but not
//! libbcachefs.a, so `queue_work()` and the system queues come out undefined.
//! This crate's build.rs links libbcachefs whole-archive, which is also what
//! makes `linux/workqueue.c`'s `constructor(102)` run, so the system queues
//! exist here.
//!
//! One test is of linux/workqueue.c's contract - par's workqueue jobs block,
//! and rely on the queue having more than one worker; one of the executor's
//! own - a task never polled twice at once, which its state word, not the
//! workqueue, guarantees. Both silent if they break: a deadlock elsewhere, or
//! a future polled concurrently.
//!
//! Each was checked against the break it is meant to catch, because a test
//! that has never failed is not yet evidence of anything:
//!
//! - `max_active` forced to 1 -> `a_blocking_task_does_not_own_the_queue`
//!   times out
//! - a wake in a poll dispatching the task rather than marking it NOTIFIED
//!   -> passes: the shim's non-reentrancy covers for it. An inline wake,
//!   which bypasses the workqueue, made it fail; that test goes until
//!   Resume::Inline is implemented again.
//!
//! Both share `Queue::system_dfl()`, so a task wedged by one can starve the
//! other: a single fault can show up as two failures.

use bcachefs_kernel::util::alloc::flags::GFP_KERNEL;
use bcachefs_kernel::util::task::{waiter, Completion, Done, Resumable, Spawn};
use bcachefs_kernel::util::task::spawn_detached;
use bcachefs_kernel::util::workqueue::Queue;

use std::future::{poll_fn, Future};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

/// @f as a task on @q, nothing joining it.
fn spawn<F: Future<Output = ()> + Send + 'static>(q: &'static Queue, f: F) {
    // SAFETY: 'static: it borrows nothing
    unsafe { spawn_detached(Spawn::Queue(q), f, GFP_KERNEL) }.unwrap();
}

/// Run `f` on a helper thread and fail rather than hang if it wedges.
///
/// Deadlock is the failure mode under test, and a test that hangs reports
/// nothing - it just makes the suite time out somewhere else, pointing at
/// whatever ran last.
fn within(secs: u64, f: impl FnOnce() + Send + 'static) {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        f();
        let _ = tx.send(());
    });
    assert!(
        rx.recv_timeout(Duration::from_secs(secs)).is_ok(),
        "no progress in {secs}s - deadlocked"
    );
}

/// Two tasks on one queue, the first blocking until the second runs.
///
/// What one worker per queue cannot serve: the worker blocks inside task A,
/// and task B - the thing that would release it - is queued behind A on the
/// same queue. par's workqueue jobs block like A.
#[test]
fn a_blocking_task_does_not_own_the_queue() {
    within(30, || {
        let q = Queue::system_dfl();
        let (release, blocked) = mpsc::channel::<()>();
        let (finish, finished) = mpsc::channel::<()>();

        // A synchronous wait inside the poll, deliberately: work items are
        // allowed to block, and this executor's tasks are work items.
        spawn(q, async move {
            blocked.recv().unwrap();
            finish.send(()).unwrap();
        });

        spawn(q, async move {
            release.send(()).unwrap();
        });

        finished.recv().unwrap();
    });
}

/// A task that wakes itself from inside its own poll must still never be polled
/// twice at once.
///
/// The wake finds it RUNNING, and marks it NOTIFIED: the poll in progress
/// repolls when it's done. Were it dispatched instead - queued again, with
/// this run still going - only workqueue non-reentrancy would keep a second
/// worker off it. The state word is what makes the future's `UnsafeCell`
/// sound, so it deserves a test that fails loudly rather than a comment that
/// reads plausibly.
#[test]
fn a_task_is_never_polled_concurrently() {
    /// @inner, counting polls that overlap.
    struct Measured<F> {
        inner: Pin<Box<F>>,
        inside: Arc<AtomicBool>,
        overlaps: Arc<AtomicUsize>,
    }

    impl<F: Future> Future for Measured<F> {
        type Output = F::Output;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
            if self.inside.swap(true, Ordering::SeqCst) {
                self.overlaps.fetch_add(1, Ordering::SeqCst);
            }
            let r = self.inner.as_mut().poll(cx);
            // Widen the window a second poll would have to land in.
            std::thread::sleep(Duration::from_millis(1));
            self.inside.store(false, Ordering::SeqCst);
            r
        }
    }

    let overlaps = Arc::new(AtomicUsize::new(0));
    let seen = overlaps.clone();

    within(30, move || {
        let (finish, finished) = mpsc::channel::<()>();

        let body = async move {
            for _ in 0..99 {
                // Woken from inside the poll: the wake lands while this run is
                // still in progress, which is the whole point.
                let mut slot = None;
                let done = AtomicPtr::<Option<Done>>::new(&raw mut slot);
                let mut c = Completion::new(&mut slot, waiter::Async::current().await);
                let mut woken = false;
                poll_fn(|cx| {
                    if woken {
                        return Poll::Ready(());
                    }
                    woken = true;
                    let _ = Pin::new(&mut c).poll_resume(cx);
                    // SAFETY: by its address, as an endio completes one
                    unsafe { (*done.load(Ordering::Relaxed)).as_ref() }.unwrap().complete();
                    Poll::Pending
                })
                .await;
            }
            finish.send(()).unwrap();
        };
        spawn(Queue::system_dfl(), Measured { inner: Box::pin(body), inside: Arc::new(AtomicBool::new(false)), overlaps });

        finished.recv().unwrap();
    });

    assert_eq!(
        seen.load(Ordering::SeqCst),
        0,
        "task was polled concurrently - the executor's state word is broken"
    );
}
