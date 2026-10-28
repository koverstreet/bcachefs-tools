// SPDX-License-Identifier: GPL-2.0

//! A moving context: what a mover's moves are issued and counted in - the
//! walks that issue them are move_data.rs. A move is asynchronous: move_extent()
//! returns once the read is issued, and the rest runs in the update's task
//! (moving_update.rs) on the context's run queue. The run queue runs only in
//! the context's waits, so all of a mover's updates run on its own thread,
//! between its own steps. The waits sleep on the run queue's EventCount,
//! woken by a task becoming ready and by every completion counted in the
//! context.
//!
//! The context's updates are a fifo, in the order they were issued: an
//! update's seq is its index there. It's what orders their writes - each
//! starts once every update issued before it has started its - and what
//! IoMark is: an update's seq, done once everything issued before it is. The
//! mover's thread's alone, issuing and finishing, so not locked.
//!
//! The context is pinned - it's on c->moving_context_list, its closure waited
//! on in place - and owns its transaction, trans(), which ratelimit(),
//! flush_all() and move_extent() unlock: keys borrowed from it are stale
//! after, as after any unlock. MovingCtxt owns one; moving through it is
//! Moving's, a borrow - of a MovingCtxt, or of a context C hands Rust. IO that
//! outlives the borrow, counted in the context, is a MovingIo.

use crate::btree::bkey::BkeySC;
use crate::btree::iter::{BtreeIter, BtreeTrans, TransFnName};
use crate::c;
use crate::c::bch_errcode;
use crate::data::copygc::bch2_copygc_wait_not_running;
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::function_name;
use crate::locking::Lock;
use crate::util::alloc::{alloc_pinned, free_pinned, AllocError, Flags};
use crate::util::list::List;
use crate::util::closure::ClosureRef;
use crate::util::ffi::Opaque;
use crate::util::{Printbuf, ToText};
use crate::util::refcount::Refcount;
use crate::util::kthread::Kthread;
use crate::util::task::RunQueue;
use crate::util::sleep::Sleep;
use crate::types::Fifo;
use crate::util::gather::GatherRef;
use core::marker::PhantomData;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicI32, AtomicPtr};
use core::sync::atomic::Ordering::Relaxed;
use cstruct_macros::c_export;

pub use c::MoveStats;

impl MoveStats {
    /// Zeroed, and named @name for the moving contexts' listing, as
    /// bch2_move_stats_init() - field by field, as it may be read meanwhile.
    pub fn reset(&self, name: &str) {
        let name = name.as_bytes();
        let len = name.len().min(self.name.len() - 1);
        for (i, b) in self.name.iter().enumerate() {
            b.set(if i < len { name[i] } else { 0 });
        }
        self.ret.set(c::bch_ioctl_data_event_ret(0));
        self.at.data_type.set(c::bch_data_type::BCH_DATA_user);
        self.at.pos.set(c::bbpos::default());
        self.at.dev.set(0);
        self.at.offset.set(0);

        for n in [&self.keys_moved, &self.keys_raced, &self.sectors_seen, &self.sectors_moved,
                  &self.sectors_raced, &self.sectors_error_corrected, &self.sectors_error_uncorrected] {
            n.store(0, Relaxed);
        }
        self.devs_error_uncorrected.store(&Default::default());
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

    /// Where whoever's moving through it is, for the listing.
    pub fn set_pos(&self, pos: c::bbpos) {
        self.at.pos.set(pos);
    }

    /// A walk's progress: at @iter's key, @sectors more seen.
    pub fn walked(&self, iter: &BtreeIter<'_>, sectors: u64) {
        self.set_pos(c::bbpos { btree: iter.btree(), pos: iter.pos() });
        self.add_sectors_seen(sectors);
    }

    /// For the moving contexts' listing.
    #[cold]
    pub fn to_text(&self, out: &mut Printbuf) {
        let mut name = [0u8; 32];
        for (b, n) in name.iter_mut().zip(&self.name) {
            *b = n.get();
        }
        let len = name.iter().position(|&b| b == 0).unwrap_or(name.len());
        write!(out, "{}: data type==", core::str::from_utf8(&name[..len]).unwrap_or("(invalid name)"));
        self.at.data_type.get().to_text(out);
        write!(out, " pos={}\n", self.at.pos.get());

        let mut out = out.indent(2);
        write!(out, "keys moved:\t{}\n", self.keys_moved.load(Relaxed));
        write!(out, "keys raced:\t{}\n", self.keys_raced.load(Relaxed));
        let sectors = [("seen", &self.sectors_seen), ("moved", &self.sectors_moved), ("raced", &self.sectors_raced)];
        for (what, n) in sectors {
            write!(out, "bytes {what}:\t");
            out.human_readable_u64((n.load(Relaxed) as u64) << 9);
            out.newline();
        }
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
    turn:    Option<GatherRef<()>>,
    started: bool,
    done:    bool,
}

/// Freed - once what's in flight is done - when it's dropped.
pub struct MovingCtxt<'a> {
    ctxt: NonNull<c::moving_context>,
    life: PhantomData<(&'a Fs, &'a MoveStats)>,
}

#[repr(transparent)]
pub struct Moving<'a>(c::moving_context, PhantomData<(&'a Fs, &'a MoveStats)>);

/// A read or a write in flight: counted in the context's read or write
/// counts, which ratelimit() throttles on, with a ref on the context's
/// closure, which flush_all() and the context's teardown wait for. Dropping
/// it ends the IO: it gives the counts back, wakes the context's waiters, and
/// puts the ref.
#[must_use = "the IO is done as soon as it's dropped"]
pub struct MovingIo {
    ctxt:    NonNull<c::moving_context>,
    write:   bool,
    sectors: u32,
    /// Dropped last: once it's put, the context may be gone
    _cl:     ClosureRef,
}

// SAFETY: it touches only the context's counters, which are atomics, its wait
// queue and its closure: any thread's. Its closure ref keeps the context.
unsafe impl Send for MovingIo {}

// SAFETY: evacuate_bucket()'s callers keep a bucket in flight until its count
// is back to zero - copygc's InFlight, until done()
unsafe impl Refcount for c::move_bucket {
    fn inc(&self) {
        self.count.fetch_add(1, Relaxed);
    }

    unsafe fn dec(&self) {
        self.count.fetch_sub(1, core::sync::atomic::Ordering::Release);
    }
}

fn io_counts(io: &c::moving_io_counts, write: bool) -> (&AtomicI32, &AtomicI32) {
    if write { (&io.write_sectors, &io.write_ios) } else { (&io.read_sectors, &io.read_ios) }
}

impl Drop for MovingIo {
    fn drop(&mut self) {
        // SAFETY: our closure ref keeps the context
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
        -> Result<MovingCtxt<'a>, AllocError>
    {
        // SAFETY: bch2_moving_ctxt_init() initializes all of it, in place
        let init = unsafe {
            ::pin_init::pin_init_from_closure(move |slot: *mut c::moving_context| {
                bch2_moving_ctxt_init(slot, fs, core::ptr::from_ref(stats).cast_mut(), wp, wait_on_copygc);
                Ok::<(), AllocError>(())
            })
        };
        let ctxt = alloc_pinned(init)?;
        Ok(MovingCtxt { ctxt: NonNull::new(ctxt).expect("alloc_pinned() returns its allocation"), life: PhantomData })
    }
}

/// c->moving_context_list, for the listing: every context, from init to exit.
type ContextList = List<c::moving_context, { core::mem::offset_of!(c::moving_context, list) }>;

impl Fs {
    /// The moving contexts, @f's alone - under moving_context_lock - while it
    /// runs.
    fn moving_contexts<R>(&self, f: impl FnOnce(&mut ContextList) -> R) -> R {
        let _lock = Lock::from_c(&self.moving_context_lock).lock();
        // SAFETY: the list, locked, and no other borrow of it in Rust: they're
        // all here, and @f can't reenter, as the lock isn't
        f(unsafe { ContextList::from_c((&raw const self.moving_context_list).cast_mut()) })
    }
}

/// A moving context at @ctxt: its moves counted into @stats, written to @wp,
/// and with @wait_on_copygc, ratelimit() waiting while copygc runs.
///
/// # Safety
/// @ctxt doesn't move until bch2_moving_ctxt_exit(), and @stats - null, or
/// not - outlives it.
#[c_export]
pub unsafe fn bch2_moving_ctxt_init(ctxt: *mut c::moving_context, c: &Fs, stats: *mut c::bch_move_stats,
                                    wp: c::write_point_specifier, wait_on_copygc: bool) {
    static FN: TransFnName = TransFnName::new();
    // SAFETY: the caller's. Zeroed, a context is empty: what's left zero is
    // valid so
    unsafe {
        ctxt.write_bytes(0, 1);
        (*ctxt).trans = c::__bch2_trans_get(c.as_ptr(), FN.idx(function_name!()));
        (*ctxt).stats = AtomicPtr::new(stats);
        (*ctxt).wp = wp;
        (*ctxt).wait_on_copygc = wait_on_copygc;
        c::closure_init_stack((*ctxt).cl.as_ptr());
    }

    // SAFETY: @ctxt stays where it is till exit() takes it off, under the lock
    c.moving_contexts(|l| unsafe { l.push_back(&*ctxt) });
}

/// bch2_moving_ctxt_init() undone, once what's in flight is done.
///
/// # Safety
/// @ctxt is bch2_moving_ctxt_init()'s, and nothing else uses it meanwhile.
#[c_export]
pub unsafe fn bch2_moving_ctxt_exit(ctxt: *mut c::moving_context) {
    // SAFETY: the caller's
    let trans = unsafe {
        let moving = Moving::from_raw(ctxt);
        moving.flush_all();

        #[cfg(CONFIG_BCACHEFS_DEBUG)]
        {
            let io = moving.io();
            for n in [&io.read_sectors, &io.write_sectors, &io.read_ios, &io.write_ios] {
                assert!(n.load(Relaxed) == 0, "a moving context's IO still counted at exit");
            }
        }

        // on this list, if any
        moving.trans().fs().moving_contexts(|l| l.remove(&*ctxt));
        (*ctxt).trans
    };

    // Putting a transaction mid-restart is an unhandled restart, but a mover
    // can be told to exit before it's retried: so begin first
    // SAFETY: the context's, no longer borrowed; zeroed, it's as init()
    // found it, its fifo's buffer freed first
    unsafe {
        c::bch2_trans_begin(trans);
        c::bch2_trans_put(trans);
        drop(core::mem::take(&mut *(*ctxt).updates.get()));
        ctxt.write_bytes(0, 1);
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
    pub unsafe fn from_raw<'r>(ctxt: *mut c::moving_context) -> &'r Self {
        // SAFETY: the caller's; repr(transparent) over it
        unsafe { &*ctxt.cast() }
    }

    /// For an entry point C calls with its context: safe, as an Opaque is
    /// always a live one.
    pub fn from_c(ctxt: &'a Opaque<c::moving_context>) -> &'a Self {
        // SAFETY: as above
        unsafe { Self::from_raw(ctxt.as_ptr()) }
    }

    /// For C that takes the context: stripe repair, which moves through it.
    pub(crate) fn raw(&self) -> *mut c::moving_context {
        core::ptr::from_ref(&self.0).cast_mut()
    }

    /// A read or write of @sectors, in flight in this context until the
    /// MovingIo's dropped.
    pub fn io_start(&self, write: bool, sectors: u32) -> MovingIo {
        let (s, ios) = io_counts(self.io(), write);
        s.fetch_add(sectors as i32, Relaxed);
        ios.fetch_add(1, Relaxed);
        MovingIo { ctxt: NonNull::from(&self.0), write, sectors, _cl: self.0.cl.get() }
    }

    pub fn trans(&self) -> &BtreeTrans<'a> {
        // SAFETY: the context's, live as long as it is
        unsafe { BtreeTrans::borrow_raw(self.0.trans) }
    }

    /// Count moves from here on into @stats - as the listing has it, which
    /// reads the pointer racily, as it does in C.
    pub fn set_stats(&self, stats: &'a MoveStats) {
        self.0.stats.store(core::ptr::from_ref(stats).cast_mut(), Relaxed)
    }

    pub fn stats(&self) -> &MoveStats {
        self.stats_opt().expect("a Rust moving context has stats")
    }

    /// A context C made may have no stats.
    pub(crate) fn stats_opt(&self) -> Option<&MoveStats> {
        // SAFETY: null, or set from a &'a MoveStats
        unsafe { self.0.stats.load(Relaxed).as_ref() }
    }

    pub(crate) fn io(&self) -> &c::moving_io_counts {
        &self.0.io
    }

    pub(crate) fn write_point(&self) -> c::write_point_specifier {
        self.0.wp
    }

    fn with_updates<R>(&self, f: impl FnOnce(&mut Fifo<InFlightUpdate, u64>) -> R) -> R {
        // SAFETY: the mover's thread's alone, and not reentered: @f is
        // ours, and touches nothing of the context's but the fifo
        f(unsafe { &mut *self.0.updates.get() })
    }

    /// Room for one more update, for update_issue().
    pub(crate) fn updates_reserve(&self, gfp: Flags) -> Result<(), AllocError> {
        self.with_updates(|f| f.reserve(1, gfp))
    }

    /// An update issued: its seq. @turn holds its turn to write till every
    /// update issued before it has started its write.
    pub(crate) fn update_issue(&self, turn: GatherRef<()>) -> u64 {
        let w = self.0.write_next.get();
        self.with_updates(|f| {
            let seq = f.back_idx();
            let turn = if seq == w {
                turn.put(());
                None
            } else {
                Some(turn)
            };
            if f.push_back(InFlightUpdate { turn, ..Default::default() }).is_err() {
                unreachable!("an update issued without updates_reserve()");
            }
            seq
        })
    }

    /// Update @seq's write starting - or, at exit, never to: the next update's
    /// turn, if every update before it has started.
    pub(crate) fn update_write_start(&self, seq: u64) {
        let mut w = self.0.write_next.get();
        self.with_updates(|f| {
            f.get_mut(seq).expect("an update not in flight").started = true;
            while f.get(w).is_some_and(|u| u.started) {
                w += 1;
            }
            if let Some(turn) = f.get_mut(w).and_then(|u| u.turn.take()) {
                turn.put(());
            }
        });
        self.0.write_next.set(w);
    }

    /// Update @seq done: off the fifo, once everything issued before it is.
    pub(crate) fn update_done(&self, seq: u64) {
        self.update_write_start(seq);
        self.with_updates(|f| {
            f.get_mut(seq).expect("an update not in flight").done = true;
            while f.peek_front().is_some_and(|u| u.done) {
                f.pop_front();
            }
        });
    }

    /// No update in flight.
    pub(crate) fn updates_idle(&self) -> bool {
        self.with_updates(|f| f.is_empty())
    }

    pub(crate) fn wake(&self) {
        self.runq().events().wake()
    }

    /// Wait until fewer than the filesystem's limits are in flight - and, if
    /// it waits on copygc, until copygc isn't running: errors if the
    /// filesystem's going read-only, or the thread's stopping.
    pub fn ratelimit(&self) -> Result<(), BchError> {
        let fs = self.trans().fs();
        if self.0.wait_on_copygc && fs.copygc.running.get() {
            self.flush_all();
            bch2_copygc_wait_not_running(fs);
        }

        if Kthread::should_stop() {
            return fs.throw(bch_errcode::BCH_ERR_kthread_cancelled);
        }
        if fs.flag(c::bch_fs_flags::BCH_FS_going_ro) {
            return fs.throw(bch_errcode::BCH_ERR_erofs_no_writes);
        }
        if Kthread::freezing() {
            self.trans().unlock_long();
            Kthread::try_to_freeze();
        }

        // XXX: these limits really ought to be per device, SSDs and hard
        // drives will want different limits
        let opts = fs.opts();
        let (sectors, ios) = (opts.move_bytes_in_flight >> 9, opts.move_ios_in_flight);
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
    /// their turn to write, and the closure refs of anything else moving
    /// through it.
    pub fn flush_all(&self) {
        let io = self.io();
        // Waited for freezably: the closure_sync() after only covers the
        // closure_put() that follows each completion's wakeup
        self.wait_until(|| {
            (io.read_ios.load(Relaxed) == 0 && io.write_ios.load(Relaxed) == 0 &&
             self.updates_idle()).then_some(())
        });
        self.trans().unlock_long();
        // SAFETY: the context's closure, which this thread owns
        unsafe { c::closure_sync(self.0.cl.as_ptr()) };
    }

    /// Waits until @cond, running the context's tasks as they become ready and
    /// sleeping with the transaction unlocked. @cond is called once per wake,
    /// so it may be a trylock.
    pub fn wait_until<T>(&self, mut cond: impl FnMut() -> Option<T>) -> T {
        let runq = self.runq();
        let events = runq.events();
        loop {
            // A task may run a transaction of its own - an EC allocation
            // failure parks its extent - which mustn't wait on ours
            if runq.has_ready() {
                self.trans().unlock_long();
                runq.run();
            }
            // before @cond, so a wake after it isn't missed
            let seen = events.snapshot();
            if let Some(v) = cond() {
                return v;
            }
            self.trans().unlock_long();

            if !runq.has_ready() {
                let Ok(()) = events.wait(seen, Sleep::UNINTERRUPTIBLE.freezable());
            }
        }
    }

    pub(crate) fn runq(&self) -> core::pin::Pin<&RunQueue> {
        // SAFETY: the context's, pinned - it's on c->moving_context_list -
        // and live while borrowed
        unsafe { core::pin::Pin::new_unchecked(&self.0.runq) }
    }

    #[cold]
    fn to_text(&self, out: &mut Printbuf) {
        let opts = self.trans().fs().opts();
        let (ios, sectors) = (opts.move_ios_in_flight, opts.move_bytes_in_flight >> 9);

        if out.nr_tabstops() == 0 {
            out.tabstop_push(32);
        }
        if let Some(stats) = self.stats_opt() {
            stats.to_text(out);
        }

        let io = self.io();
        let mut out = out.indent(2);
        write!(out, "reads: ios {}/{ios} sectors {}/{sectors}\n",
               io.read_ios.load(Relaxed), io.read_sectors.load(Relaxed));
        write!(out, "writes: ios {}/{ios} sectors {}/{sectors}\n",
               io.write_ios.load(Relaxed), io.write_sectors.load(Relaxed));
    }

    pub fn io_mark(&self) -> IoMark {
        IoMark(self.with_updates(|f| f.back_idx()))
    }

    /// Whether everything issued before @mark has completed.
    pub fn io_done(&self, mark: IoMark) -> bool {
        self.with_updates(|f| f.front_idx()) >= mark.0
    }
}

impl Fs {
    /// The moving contexts' listing: every context, its stats and IO in flight.
    #[cold]
    pub fn moving_ctxts_to_text(&self, out: &mut Printbuf) {
        self.moving_contexts(|l| {
            let mut cursor = l.cursor_mut();
            while let Some(ctxt) = cursor.current() {
                // SAFETY: on the list from init() to exit(), which takes it
                // off under the lock
                unsafe { Moving::from_raw(core::ptr::from_ref(ctxt).cast_mut()) }.to_text(out);
                cursor.move_next();
            }
        })
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

#[c_export]
pub fn bch2_moving_ctxt_flush_all(ctxt: &Opaque<c::moving_context>) {
    Moving::from_c(ctxt).flush_all()
}

impl Drop for MovingCtxt<'_> {
    fn drop(&mut self) {
        // SAFETY: ours, from new(); bch2_moving_ctxt_exit() waits for what's
        // in flight before taking it off the list and putting the transaction
        unsafe {
            bch2_moving_ctxt_exit(self.ctxt.as_ptr());
            free_pinned(self.ctxt.as_ptr());
        }
    }
}
