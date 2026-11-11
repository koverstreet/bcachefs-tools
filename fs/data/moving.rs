// SPDX-License-Identifier: GPL-2.0

//! A moving context: what a mover's moves are issued and counted in - the
//! walks that issue them are move_data.rs. A move is asynchronous: move_extent()
//! returns once the read is issued, and the rest runs in the update's task
//! (moving_update.rs) on the movers' workqueue - not on the mover's stack, which
//! may already be deep: a stripe repair's moves run inside reconcile's walk.
//! The context's waits sleep on its EventCount, woken by every completion
//! counted in the context.
//!
//! The context's updates are a fifo, in the order they were issued: an
//! update's seq is its index there. It's what orders their writes - each
//! starts once every update issued before it has started its - and what
//! IoMark is: an update's seq, done once everything issued before it is. The
//! mover issues to it and its updates' tasks finish in it, so it's locked.
//!
//! The context is pinned - its IO holds refs on it - and owns its
//! transaction, trans(), which ratelimit(), flush_all() and move_extent()
//! unlock: keys borrowed from it are stale after, as after any unlock.
//! MovingCtxt owns one; moving through it is Moving's, a borrow of one. IO
//! that outlives the borrow, counted in the context, is a MovingIo.

use crate::btree::bkey::BkeySC;
use crate::btree::iter::{BtreeIter, BtreeTrans};
use crate::btree::trans::{__bch2_trans_get, TransFnName, TransGuard};
use crate::c;
use crate::c::bch_errcode;
use crate::errcode::BchError;
use crate::format::DataType;
use crate::fs::Fs;
use crate::function_name;
use crate::util::alloc::{alloc_pinned, free_pinned, AllocError};
use crate::util::refcount::Refcount;
use crate::util::kthread::Kthread;
use crate::util::task::waiter;
use crate::util::futex::EventCount;
use crate::util::sleep::Sleep;
use crate::types::Fifo;
use crate::util::gather::{Gather, GatherRef};
use core::marker::PhantomData;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicI32, AtomicPtr};
use core::sync::atomic::Ordering::Relaxed;
use crate::locking::Locked;
use crate::new_lock;
use pin_init::{pin_data, pin_init};

pub use c::MoveStats;

impl MoveStats {
    /// Zeroed - field by field, as a data job's progress may be read
    /// meanwhile.
    pub fn reset(&self) {
        self.ret.set(c::bch_ioctl_data_event_ret(0));
        self.at.data_type.set(DataType::user);
        self.at.pos.set(c::bbpos::default());
        self.at.dev.set(0);
        self.at.offset.set(0);

        for n in [&self.keys_moved, &self.keys_raced, &self.sectors_seen, &self.sectors_moved,
                  &self.sectors_raced, &self.sectors_error_corrected, &self.sectors_error_uncorrected] {
            n.store(0, Relaxed);
        }
    }

    pub fn sectors_seen(&self) -> u64 {
        self.sectors_seen.load(Relaxed) as u64
    }

    pub fn add_sectors_seen(&self, sectors: u64) {
        self.sectors_seen.fetch_add(sectors as i64, Relaxed);
    }

    pub fn sectors_moved(&self) -> u64 {
        self.sectors_moved.load(Relaxed) as u64
    }

    /// Where whoever's moving through it is, for a data job's progress.
    pub fn set_pos(&self, pos: c::bbpos) {
        self.at.pos.set(pos);
    }

    /// A walk's progress: at @iter's key, @sectors more seen.
    pub fn walked(&self, iter: &BtreeIter<'_>, sectors: u64) {
        self.set_pos(c::bbpos { btree: iter.btree(), pos: iter.pos() });
        self.add_sectors_seen(sectors);
    }
}

/// @k's sectors, for a walk's progress: an extent's size, a btree node
/// pointer's node's.
pub fn key_sectors(fs: &Fs, k: BkeySC<'_>) -> u64 {
    if k.k.is_btree_ptr() { fs.btree_sectors().into() } else { k.k.size.into() }
}

/// A point in a context's IO: io_done() says whether everything issued
/// before it has completed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoMark(u64);

/// A data update in flight in its context, in the context's updates fifo.
#[derive(Default)]
pub struct InFlightUpdate {
    /// What holds its turn to write, till every update issued before it has
    /// started its write
    turn:    Option<GatherRef<(), waiter::Async>>,
    started: bool,
    done:    bool,
}

/*
 * Tracks in-flight data movement IO for ratelimiting.
 *
 * Four atomic counters track sectors and IOs in flight:
 *  - read_sectors/read_ios: extent read submit -> its task taking the
 *    completed read
 *  - write_sectors/write_ios: then -> write completion
 *
 * Moving::ratelimit() blocks the caller until all counters are below
 * c->opts.move_bytes_in_flight / move_ios_in_flight.
 *
 * Moves (Moving::move_extent()) and stripe repairs (ec/repair.rs)
 * both account through these counters.
 *
 * Lifetime: every in-flight IO holds a ref on in_flight.
 * Moving::flush_all() waits for the counters to drain, and MovingCtxt's
 * drop asserts they have, then waits in_flight's refs out.
 */
#[pin_data]
pub struct MovingContext {
    trans:          TransGuard<'static>,
    stats:          AtomicPtr<MoveStats>,
    wp:             c::write_point_specifier,
    wait_on_copygc: bool,

    /* A ref each for what's in flight - MovingIo's - which drop waits out */
    #[pin]
    in_flight:      Gather<(), waiter::Sync>,

    io:             IoCounts,

    /* Woken by every completion, for wait_until(): Moving::wake() */
    events:         EventCount,
    /*
     * Its data updates in flight - the mover's, and its updates' tasks', on
     * the movers' workqueue
     */
    #[pin]
    updates:        Locked<c::mutex, Updates>,
}

/// A context's data updates in flight, in the order they were issued, by seq -
/// an update's index in the fifo - and the first whose write hasn't started.
struct Updates {
    fifo:       Fifo<InFlightUpdate, u64>,
    write_next: u64,
}

impl Updates {
    /// Update @seq's write starting: the next update's turn, if every update
    /// before it has started.
    fn write_start(&mut self, seq: u64) {
        self.fifo.get_mut(seq).expect("an update not in flight").started = true;
        while self.fifo.get(self.write_next).is_some_and(|u| u.started) {
            self.write_next += 1;
        }
        if let Some(turn) = self.fifo.get_mut(self.write_next).and_then(|u| u.turn.take()) {
            turn.put(());
        }
    }
}

/* In flight: what ratelimit() throttles on, and flush_all() waits out */
#[derive(Default)]
pub struct IoCounts {
    pub read_sectors:  AtomicI32,
    pub write_sectors: AtomicI32,
    pub read_ios:      AtomicI32,
    pub write_ios:     AtomicI32,
}

/// Freed - once what's in flight is done - when it's dropped.
pub struct MovingCtxt<'a> {
    ctxt: NonNull<MovingContext>,
    life: PhantomData<(&'a Fs, &'a MoveStats)>,
}

#[repr(transparent)]
pub struct Moving<'a>(MovingContext, PhantomData<(&'a Fs, &'a MoveStats)>);

/// A read or a write in flight: counted in the context's read or write
/// counts, which ratelimit() throttles on and flush_all() waits out, with a
/// ref on the context's in_flight, which its teardown waits for. Dropping it
/// ends the IO: it gives the counts back, wakes the context's waiters, and
/// puts the ref.
#[must_use = "the IO is done as soon as it's dropped"]
pub struct MovingIo {
    ctxt:    NonNull<MovingContext>,
    write:   bool,
    sectors: u32,
    /// Dropped last: once it's put, the context may be gone
    _ref:    GatherRef<(), waiter::Sync>,
}

// SAFETY: it touches only the context's counters, which are atomics, and its
// wait queue: any thread's. Its ref keeps the context.
unsafe impl Send for MovingIo {}

// SAFETY: evacuate_bucket()'s callers keep a bucket in flight until its count
// is back to zero - copygc's InFlight, until done()
unsafe impl Refcount for c::move_bucket {
    unsafe fn inc(&self) {
        self.count.fetch_add(1, Relaxed);
    }

    unsafe fn dec(&self) {
        self.count.fetch_sub(1, core::sync::atomic::Ordering::Release);
    }
}

fn io_counts(io: &IoCounts, write: bool) -> (&AtomicI32, &AtomicI32) {
    if write { (&io.write_sectors, &io.write_ios) } else { (&io.read_sectors, &io.read_ios) }
}

impl MovingIo {
    /// What the context it's in flight in counts into.
    pub fn stats(&self) -> &MoveStats {
        unsafe { Moving::from_raw(self.ctxt.as_ptr()) }.stats()
    }
}

impl Drop for MovingIo {
    fn drop(&mut self) {
        // SAFETY: our ref keeps the context
        let moving = unsafe { Moving::from_raw(self.ctxt.as_ptr()) };
        let (sectors, ios) = io_counts(moving.io(), self.write);
        sectors.fetch_sub(self.sectors as i32, Relaxed);
        ios.fetch_sub(1, Relaxed);
        moving.wake();
    }
}

impl<'a> MovingCtxt<'a> {
    /// Moves counted into @stats, written to @wp; with @wait_on_copygc,
    /// ratelimit() waits while copygc runs.
    pub fn new(fs: &'a Fs, stats: &'a MoveStats, wp: c::write_point_specifier, wait_on_copygc: bool)
        -> Result<MovingCtxt<'a>, BchError>
    {
        static FN: TransFnName = TransFnName::new();
        let init = pin_init!(MovingContext {
            trans:          unsafe { TransGuard::from_raw(__bch2_trans_get(fs, FN.idx(function_name!()))) },
            stats:          AtomicPtr::new(ptr::from_ref(stats).cast_mut()),
            wp,
            wait_on_copygc,
            in_flight <- Gather::new(),
            io:             Default::default(),
            events:         EventCount::new(),
            updates <- new_lock!(Updates { fifo: Fifo::new(), write_next: 0 }, (), "moving_ctxt.updates"),
        }? BchError);
        let ctxt = alloc_pinned(init)?;
        Ok(MovingCtxt { ctxt: NonNull::new(ctxt).expect("alloc_pinned() returns its allocation"), life: PhantomData })
    }
}

impl<'a> core::ops::Deref for MovingCtxt<'a> {
    type Target = Moving<'a>;

    fn deref(&self) -> &Moving<'a> {
        // SAFETY: ours, live and pinned until we're dropped
        unsafe { Moving::from_raw(self.ctxt.as_ptr()) }
    }
}

impl<'a> Moving<'a> {
    /// # Safety
    /// @ctxt is initialized, and outlives the borrow, and its filesystem and
    /// stats live for 'a.
    pub unsafe fn from_raw<'r>(ctxt: *mut MovingContext) -> &'r Self {
        // SAFETY: the caller's; repr(transparent) over it
        unsafe { &*ctxt.cast() }
    }

    /// For a data update's pointer back to the context it's in flight in.
    pub(crate) fn raw(&self) -> *mut MovingContext {
        core::ptr::from_ref(&self.0).cast_mut()
    }

    /// A read or write of @sectors, in flight in this context until the
    /// MovingIo's dropped.
    pub fn io_start(&self, write: bool, sectors: u32) -> MovingIo {
        let (s, ios) = io_counts(self.io(), write);
        s.fetch_add(sectors as i32, Relaxed);
        ios.fetch_add(1, Relaxed);
        MovingIo { ctxt: NonNull::from(&self.0), write, sectors, _ref: self.0.in_flight.get() }
    }

    pub fn trans(&self) -> &TransGuard<'a> {
        &self.0.trans
    }

    /// Count moves from here on into @stats.
    pub fn set_stats(&self, stats: &'a MoveStats) {
        self.0.stats.store(ptr::from_ref(stats).cast_mut(), Relaxed)
    }

    pub fn stats(&self) -> &'a MoveStats {
        // SAFETY: set from a &'a MoveStats
        unsafe { &*self.0.stats.load(Relaxed) }
    }

    pub(crate) fn io(&self) -> &IoCounts {
        &self.0.io
    }

    pub(crate) fn write_point(&self) -> c::write_point_specifier {
        self.0.wp
    }

    fn with_updates<R>(&self, f: impl FnOnce(&mut Updates) -> R) -> R {
        f(&mut self.0.updates.lock())
    }

    /// Room for one more update, for update_issue().
    pub(crate) fn updates_reserve(&self) -> Result<(), AllocError> {
        self.with_updates(|u| u.fifo.reserve(1))
    }

    /// An update issued: its seq. @turn holds its turn to write till every
    /// update issued before it has started its write.
    pub(crate) fn update_issue(&self, turn: GatherRef<(), waiter::Async>) -> u64 {
        self.with_updates(|u| {
            let seq = u.fifo.back_idx();
            let turn = if seq == u.write_next {
                turn.put(());
                None
            } else {
                Some(turn)
            };
            if u.fifo.push_back(InFlightUpdate { turn, ..Default::default() }).is_err() {
                unreachable!("an update issued without updates_reserve()");
            }
            seq
        })
    }

    /// Update @seq's write starting - or, at exit, never to: the next update's
    /// turn, if every update before it has started.
    pub(crate) fn update_write_start(&self, seq: u64) {
        self.with_updates(|u| u.write_start(seq))
    }

    /// Update @seq done: off the fifo, once everything issued before it is.
    pub(crate) fn update_done(&self, seq: u64) {
        self.with_updates(|u| {
            u.write_start(seq);
            u.fifo.get_mut(seq).expect("an update not in flight").done = true;
            while u.fifo.peek_front().is_some_and(|u| u.done) {
                u.fifo.pop_front();
            }
        });
    }

    /// No update in flight.
    pub(crate) fn updates_idle(&self) -> bool {
        self.with_updates(|u| u.fifo.is_empty())
    }

    pub(crate) fn wake(&self) {
        self.0.events.wake()
    }

    /// Wait until fewer than the filesystem's limits are in flight - and, if
    /// it waits on copygc, until copygc isn't running: errors if the
    /// filesystem's going read-only, or the thread's stopping.
    pub fn ratelimit(&self) -> Result<(), BchError> {
        let fs = self.trans().fs();
        if self.0.wait_on_copygc && fs.copygc.running.get() {
            self.flush_all();
            fs.copygc_wait_not_running();
        }

        if Kthread::should_stop() {
            return fs.throw(bch_errcode::BCH_ERR_kthread_cancelled);
        }
        if fs.flags.get(c::bch_fs_flags::going_ro) {
            return fs.throw(bch_errcode::BCH_ERR_erofs_no_writes);
        }
        if Kthread::freezing() {
            self.trans().unlock_long();
            Kthread::try_to_freeze();
        }

        // XXX: these limits really ought to be per device, SSDs and hard
        // drives will want different limits
        let opts = fs.opts();
        let (sectors, ios) = (opts.move_bytes_in_flight.get() >> 9, opts.move_ios_in_flight.get());
        // compared unsigned, so a count gone negative is over the limit
        let under = |n: &core::sync::atomic::AtomicI32, max: u32| (n.load(Relaxed) as u32) < max;
        let io = self.io();
        self.wait_until(|| {
            (under(&io.write_sectors, sectors) && under(&io.read_sectors, sectors) &&
             under(&io.write_ios, ios) && under(&io.read_ios, ios)).then_some(())
        });
        Ok(())
    }

    pub fn wait_for_io(&self) {
        let w = &self.io().write_sectors;
        let pending = w.load(Relaxed);
        self.wait_until(|| {
            let now = w.load(Relaxed);
            (now == 0 || now != pending).then_some(())
        })
    }

    /// Waits for everything in flight in the context, including reads waiting
    /// their turn to write.
    pub fn flush_all(&self) {
        let io = self.io();
        self.wait_until(|| {
            (io.read_ios.load(Relaxed) == 0 && io.write_ios.load(Relaxed) == 0 &&
             self.updates_idle()).then_some(())
        });
        self.trans().unlock_long();
    }

    /// Waits until @cond, sleeping with the transaction unlocked. @cond is
    /// called once per wake, so it may be a trylock.
    pub fn wait_until<T>(&self, mut cond: impl FnMut() -> Option<T>) -> T {
        let events = &self.0.events;
        loop {
            // before @cond, so a wake after it isn't missed
            let seen = events.snapshot();
            if let Some(v) = cond() {
                return v;
            }
            self.trans().unlock_long();
            let Ok(()) = events.wait(seen, Sleep::UNINTERRUPTIBLE.freezable());
        }
    }

    pub fn io_mark(&self) -> IoMark {
        IoMark(self.with_updates(|u| u.fifo.back_idx()))
    }

    /// Whether everything issued before @mark has completed.
    pub fn io_done(&self, mark: IoMark) -> bool {
        self.with_updates(|u| u.fifo.front_idx()) >= mark.0
    }
}

impl c::data_update {
    /// The moving context the update is in flight in, if it's a mover's.
    pub(crate) fn moving(&self) -> Option<&Moving<'_>> {
        // SAFETY: an update is in flight in its context from init() to exit(),
        // and the context's flush_all() waits for it before the context goes
        NonNull::new(self.ctxt).map(|c| unsafe { Moving::from_raw(c.as_ptr()) })
    }
}

impl Drop for MovingCtxt<'_> {
    fn drop(&mut self) {
        self.flush_all();

        #[cfg(CONFIG_BCACHEFS_DEBUG)]
        {
            let io = self.io();
            for n in [&io.read_sectors, &io.write_sectors, &io.read_ios, &io.write_ios] {
                assert!(n.load(Relaxed) == 0, "a moving context's IO still counted at exit");
            }
        }
        // an IO's counts are given back, and its waiters woken, before its ref
        self.0.in_flight.wait_sync();

        // Putting a transaction mid-restart is an unhandled restart, but a mover
        // can be told to exit before it's retried
        unsafe {
            BtreeTrans::abandon_restart(&self.ctxt.as_ref().trans);
            free_pinned(self.ctxt.as_ptr());
        }
    }
}
