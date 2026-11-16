// SPDX-License-Identifier: GPL-2.0

//! SRCU domains, for userspace: the kernel's SRCU-fast, on liburcu's RCU.
//!
//! A domain counts its readers per CPU slot, in two pairs of lock and unlock
//! counts; a reader takes the pair ctr names, and a grace period flips ctr
//! and waits for the other pair's locks and unlocks to match. A reader's count
//! is made in an RCU read section - an implicit RCU reader, as
//! srcu_read_lock_fast()'s is with preemption off - so the updater's
//! synchronize_rcu() orders it, where classic SRCU had smp_mb()s on both
//! sides: srcu_readers_active_idx_check()'s X. The reader is in RCU's section
//! only for its count: holding the domain, it sleeps as it likes.

use core::ffi::{c_int, CStr};
use core::mem::offset_of;
use core::ptr;
use core::sync::atomic::{fence, AtomicU64, Ordering::{Acquire, Relaxed, Release, SeqCst}};
use pin_init::{pin_init_from_closure, PinInit};

use super::percpu::{CpuGuard, PerCpu};
use super::rcu::{self, Flavor, GpCookie, SrcuReadGuard};
use super::workqueue::{flush_work, work_init, Queue};
use crate::c;
use crate::errcode::BchError;
use crate::locking::{Locked, Mutex};
use crate::new_lock;
use crate::os::lockdep::LockClass;
use crate::util::alloc::init_at;
use crate::util::ffi::Opaque;
use crate::util::futex::EventCount;
use crate::util::projections::Volatile;
use crate::util::sleep::Sleep;

/// counts[LOCKS + idx], counts[UNLOCKS + idx]
const LOCKS: usize = 0;
const UNLOCKS: usize = 2;

/// What's queued with call(), FIFO through rcu_head.next, and what the work
/// item that runs grace periods is to do.
struct Callbacks {
    head:    *mut c::rcu_head,
    tail:    *mut c::rcu_head,
    /// call()s ever, for barrier()
    queued:  u64,
    /// The work item is queued or running: it stops once there's nothing in
    /// the list and gp_seq has reached @needed.
    running: bool,
    /// The furthest grace period a start_poll() asked for
    needed:  u64,
}

// SAFETY: the rcu_heads are their callers' to hand over, until their
// callbacks run
unsafe impl Send for Callbacks {}

/// An SRCU domain: srcu_struct.
pub struct Srcu {
    /// The pair readers take: ctr & 1
    ctr:    AtomicU64,
    /// Grace periods, two apiece: odd while one runs, as the kernel's
    /// rcu_seq.
    gp_seq: AtomicU64,
    /// unsigned long, as srcu_ctr's: a word, which another slot's sum reads
    /// whole
    counts: PerCpu<[Volatile<usize>; 4]>,
    cbs:    Locked<Mutex, Callbacks>,
    /// Callbacks run, for barrier()
    done:   AtomicU64,
    /// done or gp_seq moved
    event:  EventCount,
    work:   Opaque<c::work_struct>,
}

// SAFETY: a domain is any thread's to read under and wait on; its counts are
// each slot's, and the rest atomics or locked
unsafe impl Send for Srcu {}
unsafe impl Sync for Srcu {}

/// Whether gp_seq @s has reached @target: rcu_seq_done(), wrapping.
fn seq_done(s: u64, target: u64) -> bool {
    s.wrapping_sub(target) as i64 >= 0
}

impl Srcu {
    pub fn new(_name: &'static CStr, _key: LockClass) -> impl PinInit<Self, BchError> {
        let cbs = new_lock!(Callbacks {
            head:    ptr::null_mut(),
            tail:    ptr::null_mut(),
            queued:  0,
            running: false,
            needed:  0,
        }, "srcu_struct.cbs");
        let work = work_init(Self::work_fn);

        // SAFETY: every field written, @slot pinned from here on
        unsafe {
            pin_init_from_closure(move |slot: *mut Self| {
                let counts = PerCpu::alloc().map_err(|_| BchError::from(c::ENOMEM))?;
                (&raw mut (*slot).ctr).write(AtomicU64::new(0));
                (&raw mut (*slot).gp_seq).write(AtomicU64::new(0));
                (&raw mut (*slot).counts).write(counts);
                (&raw mut (*slot).done).write(AtomicU64::new(0));
                (&raw mut (*slot).event).write(EventCount::new());
                let Ok(()) = init_at(cbs, &raw mut (*slot).cbs);
                let Ok(()) = init_at(work, (&raw mut (*slot).work).cast());
                Ok(())
            })
        }
    }

    /// This slot's count @i, one more: by the slot's one writer, in the RCU
    /// read section @_rcu that orders it against grace periods.
    fn bump(&self, i: usize, _rcu: &rcu::ReadGuard) {
        let g = CpuGuard::new();
        let c = self.counts.this_cpu(&g).at(i);
        c.set(c.get().wrapping_add(1));
    }

    pub fn read_lock(&self) -> SrcuReadGuard<'_> {
        let rcu = rcu::read_lock();
        let idx = (self.ctr.load(Relaxed) & 1) as usize;
        self.bump(LOCKS + idx, &rcu);
        // SAFETY: the index just locked, of this domain
        unsafe { SrcuReadGuard::from_raw(self, idx as c_int) }
    }

    /// The guard's: on any thread, as the kernel's - the counts are summed.
    pub(super) fn read_unlock(&self, idx: c_int) {
        self.bump(UNLOCKS + idx as usize, &rcu::read_lock());
    }

    /// Its grace periods, for what waits on or queues to them.
    pub fn flavor(&self) -> Flavor<'_> {
        Flavor::srcu(self)
    }

    fn sum(&self, i: usize) -> usize {
        self.counts.at(i).each_cpu().fold(0, |s, c| s.wrapping_add(c.get()))
    }

    /// No reader holds pair @idx: srcu_readers_active_idx_check(). Unlocks,
    /// then a grace period - every reader whose unlock was counted is
    /// done, and every reader still to count its lock on @idx has - then
    /// locks: the same, and there are none.
    fn readers_gone(&self, idx: usize) -> bool {
        let unlocks = self.sum(UNLOCKS + idx);
        rcu::synchronize();
        self.sum(LOCKS + idx) == unlocks
    }

    /// One grace period: every reader that started before it has finished.
    ///
    /// The kernel's first scan, of the pair not in use, is for readers
    /// between reading ctr and counting their lock; here that's one RCU read
    /// section, which the last grace period's synchronize_rcu() waited out
    /// after its flip, so no reader takes the other pair until this one
    /// flips to it. Its synchronize_rcu() is still needed: a reader that sees
    /// the flip follows it, and so sees what was done before this grace
    /// period began - without it, its loads may be from before its count.
    fn grace_period(&self) {
        self.gp_seq.fetch_add(1, Relaxed);
        rcu::synchronize();

        let old = (self.ctr.fetch_add(1, SeqCst) & 1) as usize;

        let mut ms = 1;
        while !self.readers_gone(old) {
            super::kernel::msleep(ms);
            ms = (ms * 2).min(100);
        }

        self.gp_seq.fetch_add(1, Release);
        self.event.wake();
    }

    /// The grace periods, while something wants them, and the callbacks
    /// queued before each: the domain's one runner, as cbs.running keeps it.
    unsafe extern "C" fn work_fn(work: *mut c::work_struct) {
        // SAFETY: Srcu::work's, live until drop flushes it
        let s = unsafe { &*work.byte_sub(offset_of!(Srcu, work)).cast::<Srcu>() };

        loop {
            let mut head = {
                let mut cbs = s.cbs.lock();
                if cbs.head.is_null() && seq_done(s.gp_seq.load(Acquire), cbs.needed) {
                    cbs.running = false;
                    return;
                }
                cbs.tail = ptr::null_mut();
                core::mem::replace(&mut cbs.head, ptr::null_mut())
            };

            s.grace_period();

            let mut n = 0;
            while !head.is_null() {
                // SAFETY: queued by call(), whose caller keeps it live until
                // its callback runs - which may queue it again
                unsafe {
                    let next = (*head).next.next.cast::<c::rcu_head>();
                    ((*head).func.unwrap())(head);
                    head = next;
                }
                n += 1;
            }
            s.done.fetch_add(n, Release);
            s.event.wake();
        }
    }

    /// The work item running, if it isn't: with cbs held.
    fn kick(&self, cbs: &mut Callbacks) {
        if !cbs.running {
            cbs.running = true;
            // SAFETY: Srcu::work, initialized; flushed before Srcu goes
            unsafe { Queue::system_long().queue_work(self.work.as_ptr()) };
        }
    }

    /// The gp_seq a grace period starting now ends at: rcu_seq_snap().
    pub(super) fn get_state(&self) -> GpCookie {
        // what the caller did to what readers see, before the load
        fence(SeqCst);
        GpCookie((self.gp_seq.load(Acquire).wrapping_add(3)) & !1)
    }

    pub(super) fn start_poll(&self) -> GpCookie {
        let c = self.get_state();
        let mut cbs = self.cbs.lock();
        if seq_done(c.0, cbs.needed) {
            cbs.needed = c.0;
        }
        self.kick(&mut cbs);
        c
    }

    pub(super) fn poll(&self, c: GpCookie) -> bool {
        seq_done(self.gp_seq.load(Acquire), c.0)
    }

    /// call_srcu()
    ///
    /// # Safety
    /// @head isn't queued already, and lives until @func runs.
    pub(super) unsafe fn call(&self, head: *mut c::rcu_head, func: unsafe extern "C" fn(*mut c::rcu_head)) {
        unsafe {
            (*head).func = Some(func);
            (*head).next.next = ptr::null_mut();
        }

        let mut cbs = self.cbs.lock();
        match cbs.tail.is_null() {
            true  => cbs.head = head,
            // SAFETY: the list's, under cbs
            false => unsafe { (*cbs.tail).next.next = head.cast() },
        }
        cbs.tail = head;
        cbs.queued += 1;
        self.kick(&mut cbs);
    }

    /// srcu_barrier(): every callback queued before it has run.
    pub(super) fn barrier(&self) {
        let queued = self.cbs.lock().queued;
        let _ = self.event.wait_until(Sleep::UNINTERRUPTIBLE, || {
            seq_done(self.done.load(Acquire), queued).then_some(())
        });
    }
}

impl Drop for Srcu {
    fn drop(&mut self) {
        self.barrier();
        // SAFETY: Srcu::work; nothing queues it now
        unsafe { flush_work(self.work.as_ptr()) };

        for idx in 0..2 {
            let (locks, unlocks) = (self.sum(LOCKS + idx), self.sum(UNLOCKS + idx));
            if locks != unlocks {
                eprintln!("srcu_struct freed with {} readers on {idx}", locks.wrapping_sub(unlocks));
            }
        }
        self.counts.free();
    }
}
