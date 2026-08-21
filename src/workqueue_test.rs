//! What the userspace workqueue shim has to guarantee, exercised end to end.
//!
//! These belong beside `fs/util/async_exec.rs` and cannot live there: a
//! `cargo test -p bcachefs-kernel` binary links `-lbcachefs_static_wrappers`
//! but not libbcachefs.a, so `queue_work()` and `system_unbound_wq` come out
//! undefined. This crate's build.rs links libbcachefs whole-archive, which is
//! also what makes `linux/workqueue.c`'s `constructor(102)` run, so the system
//! queues actually exist here.
//!
//! Both tests are about linux/workqueue.c's contract rather than any Rust
//! logic, which is the point: async_exec's soundness rests on that contract,
//! the contract is invisible from Rust, and it has already been broken once by
//! a change to workqueue.c that looked unrelated to async.
//!
//! Each was checked against the break it is meant to catch, because a test that
//! has never failed is not yet evidence of anything:
//!
//! - `find_runnable_work()` reverted to taking the head unconditionally ->
//!   `a_task_is_never_polled_concurrently` sees 36 overlaps in 100 polls
//! - `max_active` forced to 1 -> `a_blocking_task_does_not_own_the_queue`
//!   times out
//!
//! Caveat from that second run: both tests share `system_unbound()`, the only
//! queue the shim exposes to Rust, so a task wedged by one test can starve the
//! other and a single fault shows up as two failures. Both still mean "the
//! workqueue contract is broken", so this is noise in attribution rather than
//! in the verdict - but if the shim ever grows an `alloc_workqueue` binding,
//! give each test its own queue.

use bcachefs_kernel::util::async_exec::{spawn, system_unbound};

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

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
/// This is what the executor exists for, and what one worker per queue cannot
/// serve: the worker blocks inside task A, and task B - the thing that would
/// release it - is queued behind A on the same queue.
#[test]
fn a_blocking_task_does_not_own_the_queue() {
    within(30, || {
        let q = system_unbound();
        let (release, blocked) = mpsc::channel::<()>();
        let (finish, finished) = mpsc::channel::<()>();

        // A synchronous wait inside the poll, deliberately: work items are
        // allowed to block, and this executor's tasks are work items.
        spawn(q, async move {
            blocked.recv().unwrap();
            finish.send(()).unwrap();
        })
        .unwrap();

        spawn(q, async move {
            release.send(()).unwrap();
        })
        .unwrap();

        finished.recv().unwrap();
    });
}

/// A task that wakes itself from inside its own poll must still never be polled
/// by two workers at once.
///
/// The pending bit does not give this: it is cleared before the work function
/// runs, so the self-wake re-queues successfully and the item is back on the
/// pending list while this very run is still going. What keeps a second worker
/// off it is workqueue non-reentrancy - which is what makes async_exec's
/// `UnsafeCell` access and its `unsafe impl Sync` sound, so it deserves a test
/// that fails loudly rather than a comment that reads plausibly.
#[test]
fn a_task_is_never_polled_concurrently() {
    struct SelfWaking {
        polls: u32,
        inside: Arc<AtomicBool>,
        overlaps: Arc<AtomicUsize>,
        finish: mpsc::Sender<()>,
    }

    impl Future for SelfWaking {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.inside.swap(true, Ordering::SeqCst) {
                self.overlaps.fetch_add(1, Ordering::SeqCst);
            }

            self.polls += 1;
            let again = self.polls < 100;

            // Wake from inside the poll: the re-enqueue lands while this run is
            // still in progress, which is the whole point. Not on the last one
            // though - a wake that outlives `Ready` buys one more poll, after
            // the waiter has gone.
            if again {
                cx.waker().wake_by_ref();
            }

            // Widen the window a second poll would have to land in.
            std::thread::sleep(Duration::from_millis(1));
            self.inside.store(false, Ordering::SeqCst);

            if again {
                Poll::Pending
            } else {
                self.finish.send(()).unwrap();
                Poll::Ready(())
            }
        }
    }

    let overlaps = Arc::new(AtomicUsize::new(0));
    let seen = overlaps.clone();

    within(30, move || {
        let (finish, finished) = mpsc::channel::<()>();

        spawn(
            system_unbound(),
            SelfWaking {
                polls: 0,
                inside: Arc::new(AtomicBool::new(false)),
                overlaps,
                finish,
            },
        )
        .unwrap();

        finished.recv().unwrap();
    });

    assert_eq!(
        seen.load(Ordering::SeqCst),
        0,
        "task was polled concurrently - workqueue non-reentrancy is broken"
    );
}
