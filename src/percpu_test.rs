//! The userspace percpu's slot lifetime (os/linux_user/percpu.rs):
//!
//! - Churn: four times as many short-lived threads as there are slots, one
//!   after another. Each must get a slot - exited threads give theirs back, or
//!   thread MAX_CPUS aborts the process - and what each added to a percpu
//!   counter must still be counted after it has gone, as a CPU's counters
//!   outlive the tasks that ran on it.
//! - Live threads never share a slot: concurrently live threads each have a
//!   distinct copy, and a percpu counter they all hammer sums exactly.

use std::sync::Barrier;
use std::thread;

use bcachefs_kernel::os::percpu::{CpuGuard, PerCpu, MAX_CPUS};

const CHURN_THREADS: usize = 4 * MAX_CPUS;
const LIVE_THREADS:  usize = 16;
const LIVE_ADDS:     usize = 10000;

struct Counter(PerCpu<u64>);

// Shared with the threads adding to it, as a struct's percpu counter is
unsafe impl Sync for Counter {}

impl std::ops::Deref for Counter {
    type Target = PerCpu<u64>;

    fn deref(&self) -> &PerCpu<u64> {
        &self.0
    }
}

#[test]
fn percpu_slot_churn() {
    let mut ctr = Counter(PerCpu::alloc().expect("alloc_percpu"));

    /*
     * Without slots coming back, thread number MAX_CPUS aborts the process.
     * Counting slots used instead wouldn't work: cargo runs other tests'
     * threads alongside this. Joined explicitly: a scope's implicit join
     * doesn't wait for the thread's exit, where its slot comes back.
     */
    thread::scope(|s| {
        for _ in 0..CHURN_THREADS {
            s.spawn(|| ctr.get().add(1)).join().unwrap();
        }
    });

    assert_eq!(ctr.sum(), CHURN_THREADS as u64);
    ctr.0.free();
}

#[test]
fn percpu_slot_live_distinct() {
    let mut ctr = Counter(PerCpu::alloc().expect("alloc_percpu"));
    let barrier = Barrier::new(LIVE_THREADS);

    let mut copies: Vec<usize> = thread::scope(|s| {
        let threads: Vec<_> = (0..LIVE_THREADS).map(|_| s.spawn(|| {
            let g = CpuGuard::new();
            let copy = unsafe { ctr.this_cpu(&g).as_mut() } as *mut u64 as usize;
            drop(g);

            /* Everyone holds a slot at once before anyone can exit and free one: */
            barrier.wait();

            for _ in 0..LIVE_ADDS {
                ctr.get().add(1);
            }

            barrier.wait();
            copy
        })).collect();

        threads.into_iter().map(|t| t.join().unwrap()).collect()
    });

    copies.sort();
    copies.dedup();
    assert_eq!(copies.len(), LIVE_THREADS, "live threads share a copy");

    assert_eq!(ctr.sum(), (LIVE_THREADS * LIVE_ADDS) as u64);
    ctr.0.free();
}
