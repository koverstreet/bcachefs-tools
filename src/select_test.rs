//! A timed async wait - a Select of a futex wake and a Timer - exercised end
//! to end, on the userspace workqueue shim. Nothing in normal operation runs
//! its timeout side: the allocator's stuck report is the one user, and
//! nothing wedges the allocator in a test. So both outcomes, and the race
//! between them, are here: a Select or Timer that broke would otherwise stay
//! silent until a stuck allocator didn't report.
//!
//! What each catches:
//! - `a_wait_with_no_wake_times_out`: the Timer never firing, or a timeout
//!   reported as a wake - it hangs, or returns Ok
//! - `a_wake_ends_the_wait_before_its_timeout`: the wake not completing the
//!   Select, or the Timer not deleted as it drops - the wait then sits out
//!   the whole timeout
//! - `a_wake_racing_the_timer`: both inputs completing the Select - a second
//!   completion that isn't a no-op asserts, one that's lost hangs

use bcachefs_kernel::os::kernel::HZ;
use bcachefs_kernel::os::workqueue::Queue;
use bcachefs_kernel::util::futex::{bch2_futex_init, EventCount};
use bcachefs_kernel::util::sleep::SleepError;
use bcachefs_kernel::util::task::{spawn_detached, Resume, TASKS};

use std::future::Future;
use std::sync::mpsc;
use std::sync::Once;
use std::time::{Duration, Instant};

fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(bch2_futex_init);
}

/// @f as a task on the system queue, nothing joining it.
fn spawn<F: Future<Output = ()> + Send + 'static>(f: F) {
    unsafe { spawn_detached(Queue::system_dfl(), &TASKS, move || f) }.unwrap();
}

/// @ev waited on for @timeout jiffies by a task: what it returned, and how
/// long the task took, from the wait to its end - the Timer's drop included.
fn timed_wait(ev: &'static EventCount, timeout: u64) -> mpsc::Receiver<(Result<(), SleepError>, Duration)> {
    let (tx, rx) = mpsc::channel();
    spawn(async move {
        let start = Instant::now();
        let seen = ev.snapshot();
        let ret = ev.wait_async_for(seen, Resume::queue(Queue::system_dfl()), timeout as _).await;
        tx.send((ret, start.elapsed())).unwrap();
    });
    rx
}

fn jiffies_to_duration(j: u64) -> Duration {
    Duration::from_millis(j * 1000 / HZ as u64)
}

#[test]
fn a_wait_with_no_wake_times_out() {
    init();
    static EV: EventCount = EventCount::new();

    let timeout = (HZ as u64 / 10).max(1);
    let (ret, took) = timed_wait(&EV, timeout).recv_timeout(Duration::from_secs(10))
        .expect("a timed wait with no wake never ended: the timer didn't fire");

    assert_eq!(ret, Err(SleepError::Timeout), "a wait no one woke returned as woken");
    assert!(took >= jiffies_to_duration(timeout).saturating_sub(Duration::from_millis(5)),
            "a timed wait ended before its timeout: {took:?}");
}

#[test]
fn a_wake_ends_the_wait_before_its_timeout() {
    init();
    static EV: EventCount = EventCount::new();

    let timeout = 30 * HZ as u64;
    let rx = timed_wait(&EV, timeout);
    // queued, most likely; if not, the snapshot it takes sees this wake
    std::thread::sleep(Duration::from_millis(50));
    EV.wake();

    let (ret, took) = rx.recv_timeout(Duration::from_secs(10))
        .expect("a woken timed wait didn't end: it sat out its Timer");
    assert_eq!(ret, Ok(()), "a woken wait returned as timed out");
    assert!(took < Duration::from_secs(5), "a woken wait took {took:?}: its Timer wasn't cancelled");
}

#[test]
fn a_wake_racing_the_timer() {
    init();
    static EV: EventCount = EventCount::new();

    // a wake about every jiffy: as often as the timer fires, so each wins
    // some, and some land together
    let jiffy = jiffies_to_duration(1).max(Duration::from_millis(1));
    let (tx, rx) = mpsc::channel::<()>();
    std::thread::spawn(move || {
        while let Err(mpsc::TryRecvError::Empty) = rx.try_recv() {
            EV.wake();
            std::thread::sleep(jiffy);
        }
    });

    let (mut woken, mut timed_out) = (0, 0);
    for i in 0..300 {
        let (ret, _) = timed_wait(&EV, 1).recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|_| panic!("timed wait {i}, its wake racing its timer, never ended"));
        match ret {
            Ok(())                    => woken += 1,
            Err(SleepError::Timeout) => timed_out += 1,
            Err(e)                    => panic!("timed wait {i}: {e:?}"),
        }
    }
    let _ = tx.send(());

    // not the property under test, but without both the race wasn't run
    assert!(woken != 0 && timed_out != 0, "no race: {woken} woken, {timed_out} timed out");
}
