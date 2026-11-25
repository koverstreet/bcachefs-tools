// SPDX-License-Identifier: GPL-2.0

//! util/rcu.rs's SRCU domains, as the kernel's and userspace's give them -
//! built only with CONFIG_BCACHEFS_TESTS, run by perf_test, and by cargo test
//! with EXTRA_CFLAGS=-DCONFIG_BCACHEFS_TESTS.
//!
//! test_srcu: a reader that sleeps holds off its domain's grace periods, and
//! not another domain's; a callback waits for the readers before it, and
//! barrier() for the callbacks before it; a cookie is done only after both.
//!
//! test_srcu_torture: two domains, each with readers on every CPU and a
//! writer replacing the object they read, the old one freed by a callback or
//! once a polled grace period is over. An object is recycled from a pool, so
//! each incarnation has a life count, and a reader holding the domain checks
//! the object it read is still live and the same incarnation, before and
//! after sleeping now and then: one freed under it is the bug.

use core::mem::offset_of;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering::{Acquire, Relaxed, Release}};
use pin_init::stack_try_pin_init;

use super::TestRet;
use crate::c;
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::new_srcu;
use crate::os::kernel::{local_clock, msleep, num_online_cpus, random_u64_below};
use crate::util::par::{self, Backend};
use crate::util::rcu::{Flavor, RcuHead, RcuOwned, Srcu};
use crate::util::task::TASKS;

#[cfg(not(kernel))]
use bcachefs_shim as kernel;

fn failed(what: core::fmt::Arguments<'_>) -> TestRet {
    kernel::pr_err!("test_srcu: {what}\n");
    Err(BchError::from(c::Errcode::EINVAL_test_srcu_failed))
}

/// Waits up to @secs for @cond, a grace period to end: whether it did.
fn wait_for(secs: u64, mut cond: impl FnMut() -> bool) -> bool {
    let end = local_clock() + secs * c::NSEC_PER_SEC as u64;
    while !cond() {
        if local_clock() > end {
            return false;
        }
        msleep(1);
    }
    true
}

/// A callback's: whether it's run.
#[repr(C)]
struct Flagged {
    head: c::rcu_head,
    ran:  AtomicBool,
}

impl Flagged {
    fn new() -> Self {
        Flagged { head: unsafe { core::mem::zeroed() }, ran: AtomicBool::new(false) }
    }

    /// # Safety
    /// Not queued already, and lives until barrier()'s returned.
    unsafe fn call(&self, flavor: Flavor<'_>) {
        flavor.call(unsafe { Queued::new(self) }, |q| q.0.ran.store(true, Release));
    }
}

// its rcu_head is the domain's while queued; the rest atomic
unsafe impl Sync for Flagged {}

/// A test's object, given up to a grace period.
struct Queued<'a, T>(&'a T);

impl<'a, T> Queued<'a, T> {
    /// # Safety
    /// @t isn't queued already, and lives until its domain's barrier().
    unsafe fn new(t: &'a T) -> Self {
        Queued(t)
    }
}

// new()'s callers keep it until its callback's run
unsafe impl<T: HasHead> RcuOwned for Queued<'_, T> {
    fn into_head(self) -> NonNull<RcuHead> {
        unsafe { NonNull::from(self.0).byte_add(T::HEAD).cast() }
    }

    unsafe fn from_head(head: NonNull<RcuHead>) -> Self {
        Queued(unsafe { head.byte_sub(T::HEAD).cast().as_ref() })
    }
}

/// Where a test object's rcu_head is
trait HasHead {
    const HEAD: usize;
}

impl HasHead for Flagged {
    const HEAD: usize = offset_of!(Flagged, head);
}

/// A reader that sleeps holds off its domain, and not @other.
fn readers(a: &Srcu, b: &Srcu) -> TestRet {
    let g = a.read_lock();
    let (ca, cb) = (a.flavor().start_poll(), b.flavor().start_poll());

    if !wait_for(10, || b.flavor().poll(cb)) {
        return failed(format_args!("a reader of another domain held off a grace period for 10s"));
    }
    msleep(100);
    if a.flavor().poll(ca) {
        return failed(format_args!("a grace period ended under a reader asleep in it"));
    }

    drop(g);
    if !wait_for(10, || a.flavor().poll(ca)) {
        return failed(format_args!("a grace period didn't end 10s after its last reader"));
    }
    Ok(())
}

/// A callback waits for the readers before it; barrier() for the callbacks.
fn callbacks(a: &Srcu) -> TestRet {
    let (first, second) = (Flagged::new(), Flagged::new());

    a.flavor().barrier();

    let g = a.read_lock();
    // neither queued yet; both live past barrier()
    unsafe { first.call(a.flavor()) };
    msleep(100);
    if first.ran.load(Acquire) {
        return failed(format_args!("a callback ran under a reader that began before it"));
    }
    drop(g);

    unsafe { second.call(a.flavor()) };
    a.flavor().barrier();
    if !first.ran.load(Acquire) || !second.ran.load(Acquire) {
        return failed(format_args!("barrier() returned before the callbacks queued before it ran: {} {}",
                                   first.ran.load(Acquire), second.ran.load(Acquire)));
    }
    Ok(())
}

fn srcu() -> TestRet {
    stack_try_pin_init!(let a =? new_srcu!("test_srcu.a"));
    stack_try_pin_init!(let b =? new_srcu!("test_srcu.b"));

    readers(&a, &b)?;
    readers(&b, &a)?;
    callbacks(&a)
}

pub(super) fn test_srcu(_fs: &Fs, _nr: u64) -> TestRet {
    srcu()
}

const POOL: usize = 64;
const LIVE: u64 = 1;
const DEAD: u64 = 2;

#[repr(C)]
struct Obj {
    head:  c::rcu_head,
    state: AtomicU64,
    life:  AtomicU64,
    free:  AtomicBool,
}

impl Obj {
    fn new() -> Self {
        Obj {
            head:  unsafe { core::mem::zeroed() },
            state: AtomicU64::new(DEAD),
            life:  AtomicU64::new(0),
            free:  AtomicBool::new(true),
        }
    }

    fn retire(&self) {
        self.state.store(DEAD, Release);
        self.free.store(true, Release);
    }
}

impl HasHead for Obj {
    const HEAD: usize = offset_of!(Obj, head);
}

// its rcu_head is the domain's while queued, and only the writer queues it;
// the rest atomics
unsafe impl Sync for Obj {}

/// One domain's torture: its readers' failures and counts, its writer's.
struct Torture<'a> {
    srcu:   &'a Srcu,
    cur:    AtomicPtr<Obj>,
    pool:   [Obj; POOL],
    reads:  AtomicU64,
    swaps:  AtomicU64,
    failed: AtomicU64,
}

impl<'a> Torture<'a> {
    fn new(srcu: &'a Srcu) -> Self {
        Torture {
            srcu,
            cur:    AtomicPtr::new(core::ptr::null_mut()),
            pool:   core::array::from_fn(|_| Obj::new()),
            reads:  AtomicU64::new(0),
            swaps:  AtomicU64::new(0),
            failed: AtomicU64::new(0),
        }
    }

    fn reader(&self, end: u64) -> TestRet {
        let mut n = 0;
        while local_clock() < end && self.failed.load(Relaxed) == 0 {
            let g = self.srcu.read_lock();
            let p = self.cur.load(Acquire);
            if p.is_null() {
                drop(g);
                msleep(1);
                continue;
            }
            let o = unsafe { &*p };
            let life = o.life.load(Acquire);

            for check in 0..2 {
                let (state, now) = (o.state.load(Acquire), o.life.load(Acquire));
                if state != LIVE || now != life {
                    self.failed.fetch_add(1, Relaxed);
                    return failed(format_args!(
                        "object {} freed under a reader ({}): state {state}, life {life} now {now}",
                        (p as usize - self.pool.as_ptr() as usize) / size_of::<Obj>(),
                        if check == 0 { "as read" } else { "after sleeping" }));
                }
                if random_u64_below(16) == 0 {
                    msleep(random_u64_below(3) as u32);
                }
            }
            drop(g);
            n += 1;
        }
        self.reads.fetch_add(n, Relaxed);
        Ok(())
    }

    fn writer(&self, end: u64) -> TestRet {
        let mut n = 0u64;
        while local_clock() < end && self.failed.load(Relaxed) == 0 {
            let Some(o) = self.pool.iter().find(|o| o.free.load(Acquire)) else {
                self.srcu.flavor().barrier();
                continue;
            };
            o.free.store(false, Relaxed);
            o.life.fetch_add(1, Relaxed);
            o.state.store(LIVE, Relaxed);

            let old = self.cur.swap((o as *const Obj).cast_mut(), Release);
            if let Some(old) = unsafe { old.as_ref() } {
                if n % 8 == 0 {
                    let cookie = self.srcu.flavor().start_poll();
                    if !wait_for(30, || self.srcu.flavor().poll(cookie)) {
                        self.failed.fetch_add(1, Relaxed);
                        return failed(format_args!("a polled grace period didn't end in 30s"));
                    }
                    old.retire();
                } else {
                    // retired once per incarnation; the pool outlives the
                    // domain's barrier()
                    self.srcu.flavor().call(unsafe { Queued::new(old) }, |q| q.0.retire());
                }
            }
            n += 1;
            if random_u64_below(4) == 0 {
                msleep(1);
            }
        }
        self.swaps.fetch_add(n, Relaxed);
        Ok(())
    }
}

fn torture(secs: u64) -> TestRet {
    let end = local_clock() + secs * c::NSEC_PER_SEC as u64;

    stack_try_pin_init!(let a =? new_srcu!("test_srcu_torture.a"));
    stack_try_pin_init!(let b =? new_srcu!("test_srcu_torture.b"));
    let tortures = [Torture::new(&a), Torture::new(&b)];
    let nr_readers = num_online_cpus().max(2);

    let ret = par::scope(Backend::Kthreads, &TASKS, |s| {
        for (d, t) in tortures.iter().enumerate() {
            s.spawn(format_args!("srcu torture writer[{d}]"), move || t.writer(end))?;
            for i in 0..nr_readers {
                s.spawn(format_args!("srcu torture reader[{d}.{i}]"), move || t.reader(end))?;
            }
        }
        Ok(())
    });

    // the callbacks reach the pools: done before they go
    a.flavor().barrier();
    b.flavor().barrier();

    for (d, t) in tortures.iter().enumerate() {
        kernel::pr_info!("test_srcu_torture: domain {d}: {} reads, {} objects freed under {nr_readers} readers\n",
                         t.reads.load(Relaxed), t.swaps.load(Relaxed));
    }
    ret
}

pub(super) fn test_srcu_torture(_fs: &Fs, nr: u64) -> TestRet {
    torture(if nr != 0 { nr } else { 5 })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(name: &str, f: fn() -> TestRet) {
        crate::os::sched::linux_thread_init();
        let ret = f();
        crate::os::sched::linux_thread_exit();
        ret.unwrap_or_else(|e| panic!("{name}: {e:?}"));
    }

    #[test]
    fn test_srcu() {
        run("srcu", srcu)
    }

    #[test]
    fn test_srcu_torture() {
        run("srcu torture", || torture(3))
    }
}
