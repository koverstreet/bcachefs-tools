// SPDX-License-Identifier: GPL-2.0

//! The cycle detector, and the graph of waiting transactions it walks: see
//! "Cycle detection" in locking.rs.

use crate::btree::iter::BtreeTrans;
use crate::btree::locking::{BtreeNodeLockedType, btree_node_pos};
use crate::btree::node::BtreeNode;
use crate::btree::path::{table_allocated, table_header, BtreePath, PathNode};
use crate::btree::trans::foreign_bits;
use crate::c;
use crate::errcode::{Errcode, BchError};
use crate::format::Btree;
use crate::fs::Fs;
use crate::locking::MemallocFlags;
use crate::locking::six::six_lock_counts;
use crate::prt;
use crate::debug::trace::events;
use crate::util::alloc::AllocError;
use crate::util::backtrace::prt_task_backtrace;
use crate::util::closure::ClosureRef;
use crate::data_structures::bitmap::AtomicBitmap;
use crate::data_structures::inline_vec::InlineVec;
use crate::types::bits_to_longs;
use crate::util::kernel::time_after64;
use crate::util::percpu::{bch2_raw_smp_processor_id, bch2_set_current_wake_cpu, bch2_task_pid, CpuGuard, PerCpu,
                          PerCpuRef};
use crate::util::log::{print_str, LogLevel};
use crate::util::rcu::{self, ReadGuard};
use crate::util::Printbuf;
use core::ffi::{c_int, c_uint, c_ulong, c_void};
use core::mem::offset_of;
use core::ptr::{self, null_mut};
use core::sync::atomic::{fence, AtomicPtr, AtomicUsize, Ordering::{Acquire, Relaxed, Release, SeqCst}};

/// A transaction the walk reaches, of whichever thread: it's read as C reads
/// it, never through a reference - another thread's is changing under us.
/// Its memory is RCU-freed (bch2_trans_put()), and the walk is in an RCU
/// read-side section.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Trans(*mut BtreeTrans<'static>);

impl Trans {
    fn locking(self) -> *mut c::btree_bkey_cached_common {
        unsafe { (&raw const (*self.0).locking).cast::<*mut c::btree_bkey_cached_common>().read_volatile() }
    }

    fn lock_want(self) -> c::SixLockType {
        unsafe { (*self.0).locking_wait.lock_want.get() }
    }

    fn task(self) -> *mut c::task_struct {
        unsafe { (*self.0).locking_wait.task.load(Relaxed) }
    }

    fn start_time(self) -> u64 {
        unsafe { (*self.0).locking_wait.trans_start_time.get() }
    }

    fn may_not_fail(self) -> bool {
        unsafe { (&raw const (*self.0).bits).cast::<c::btree_trans_bits>().read_volatile() }.lock_may_not_fail()
    }

    fn must_abort(self) -> bool {
        unsafe { (*self.0).lock_must_abort.load(Relaxed) != 0 }
    }

    fn set_must_abort(self) {
        unsafe { (*self.0).lock_must_abort.store(1, Relaxed) }
    }

    fn paths(self) -> *mut BtreePath {
        unsafe { (*(&raw const (*self.0).paths).cast::<AtomicPtr<BtreePath>>()).load(Acquire) }
    }

    fn fs(self) -> &'static Fs {
        unsafe { (*self.0).c }
    }

    fn fn_name(self) -> &'static str {
        crate::btree::trans::transaction_fn_name(unsafe { (*self.0).fn_idx.get() })
    }

    /// For the waiting transaction's own thread: the one the walk starts from.
    fn own(&self) -> &BtreeTrans<'_> {
        unsafe { BtreeTrans::borrow_raw(self.0) }
    }

    fn wake_up(self) {
        if let Some(_r) = ClosureRef::tryget(unsafe { &(*self.0).ref_ }) {
            unsafe { c::wake_up_process(self.task()) };
        }
    }

    fn restart_foreign_task(self, err: Errcode) -> BchError {
        let ip = Self::restart_foreign_task as *const () as c_ulong;
        self.own().restart_foreign_task(err, ip)
    }

    /// container_of(@w, struct btree_trans, locking_wait): every waiter on a
    /// btree node lock is a transaction's.
    fn of_waiter(w: *mut c::six_lock_waiter) -> Self {
        Trans(unsafe { w.byte_sub(offset_of!(BtreeTrans<'_>, locking_wait)) }.cast())
    }
}

/// btree_node_locked_type() of a path another thread may be changing.
fn path_locked_type(path: *const BtreePath, level: usize) -> BtreeNodeLockedType {
    BtreeNodeLockedType::of(unsafe { (&raw const (*path).nodes_locked).read_volatile() }, level)
}

/// trans_paths_allocated() and trans_paths_nr(), of a path table another
/// thread may be using: the bitmap, then nr_paths, sit before the paths.
fn paths_allocated<'a>(paths: *mut BtreePath) -> (&'a AtomicBitmap, usize) {
    let nr = unsafe { (*table_header(paths)).nr_paths } as usize;
    let words = table_allocated(paths, nr).cast::<AtomicUsize>();
    (AtomicBitmap::from_words(unsafe { core::slice::from_raw_parts(words, bits_to_longs(nr)) }), nr)
}

fn bkey_cached_common_to_text(out: &mut Printbuf, b: &c::btree_bkey_cached_common) {
    let c = six_lock_counts(b.lock.six());

    let pid = {
        let _rcu = rcu::read_lock();
        let owner = b.lock.six().owner.load(Relaxed);
        if owner.is_null() { 0 } else { unsafe { bch2_task_pid(owner) } }
    };

    prt!(out, "\t{:p} {} {} l={}:{}", b, if b.cached { 'c' } else { 'b' },
         Btree(b.btree_id.into()), b.level, btree_node_pos(b));
    prt!(out, "\t locks {}:{}:{} held by pid {}", c.n[0], c.n[1], c.n[2], pid);
}

fn btree_lock_type_char(lock_type: i32) -> char {
    match lock_type {
        0 => 'r',
        1 => 'i',
        2 => 'w',
        _ => '?',
    }
}

/// # Safety
/// @trans is live - held by a ref on its closure, or reached under RCU - and
/// any thread's.
#[cold]
pub(crate) unsafe fn bch2_btree_trans_to_text(out: &mut Printbuf, trans: *mut BtreeTrans<'static>) {
    let trans = Trans(trans);
    let task = trans.task();

    /* before rcu_read_lock(): */
    out.make_room(4096);

    if out.nr_tabstops() == 0 {
        out.tabstop_push(16);
        out.tabstop_push(32);
    }

    prt!(out, "{} {}\n", if task.is_null() { 0 } else { unsafe { bch2_task_pid(task) } }, trans.fn_name());

    if foreign_bits(trans.0).journal_replay_not_finished() {
        prt!(out, "has journal_keys ref\n");
    }

    /* trans->paths is rcu protected vs. freeing */
    let _rcu = rcu::read_lock();
    let _noreclaim = MemallocFlags::noreclaim();

    let paths = trans.paths();
    if paths.is_null() {
        return;
    }

    let (allocated, nr) = paths_allocated(paths);
    let mut idx = 1;
    while let Some(i) = allocated.next(idx, nr) {
        idx = i + 1;

        let path = unsafe { ptr::read_volatile(paths.add(i)) };
        if path.nodes_locked == 0 {
            continue;
        }

        prt!(out, "  path {} {} {} l={}:{}\n", i, if path.cached() { 'c' } else { 'b' },
             path.btree_id(), path.level(), path.pos);

        for l in 0..c::BTREE_MAX_DEPTH as usize {
            let lock_type = BtreeNodeLockedType::of(path.nodes_locked, l);

            if lock_type != BtreeNodeLockedType::BTREE_NODE_UNLOCKED {
                if let Ok(Some(b)) = path.l[l].b.get() {
                    prt!(out, "    {} l={} ", btree_lock_type_char(lock_type as i32), l);
                    bkey_cached_common_to_text(out, unsafe { b.as_ref() });
                    prt!(out, "\n");
                }
            }
        }
    }

    let b = trans.locking();
    if !b.is_null() {
        prt!(out, "  blocked on:\n");
        prt!(out, "    {}", btree_lock_type_char(trans.lock_want() as i32));
        bkey_cached_common_to_text(out, unsafe { &*b });
        prt!(out, "\n");
    }
}

fn lock_type_conflicts(t1: BtreeNodeLockedType, t2: c::SixLockType) -> bool {
    t1 as i32 + t2 as i32 > 1
}

/// Why a walk stopped short.
pub(crate) enum Cycle {
    /// For a check - debugfs, of a transaction that may not be waiting at
    /// all: a cycle, written out - or no memory to look.
    Found,
    /// The waiting transaction restarted: to break a cycle, or because the
    /// walk couldn't finish.
    Restart(BchError),
}

/* State used for the cycle detector */
/*
 * @trans wants to lock @b with type @type
 */
struct TransWaitingForLock {
    trans:        *mut BtreeTrans<'static>,
    node_want:    *mut c::btree_bkey_cached_common,
    lock_want:    c::SixLockType,

    /* for iterating over held locks :*/
    level:        usize,
    path_idx:     usize,
    waitlist_idx: usize,
    node_have:    *mut c::btree_bkey_cached_common,

    /*
     * Conflicting waiters we found on the lock at (@path_idx, @level),
     * cached at snapshot time so iteration is stable against concurrent
     * wakeup activity. @waitlist_idx is the next entry to descend into.
     */
    waitlist:     InlineVec<*mut BtreeTrans<'static>, 16>,
}

impl TransWaitingForLock {
    const fn new() -> Self {
        Self {
            trans:        null_mut(),
            node_want:    null_mut(),
            lock_want:    c::SixLockType::SIX_LOCK_read,
            level:        0,
            path_idx:     0,
            waitlist_idx: 0,
            node_have:    null_mut(),
            waitlist:     InlineVec::new(),
        }
    }
}

const LOCK_GRAPH_DEPTH: usize = 8;

struct LockGraph {
    g:             [TransWaitingForLock; LOCK_GRAPH_DEPTH],
    nr:            usize,
    printed_chain: bool,
}

/// Where the walk goes from a frame whose held locks it's scanned: into the
/// waiters it found, or back up.
enum Step {
    Next,
    Up,
}

impl LockGraph {
    const fn new() -> Self {
        Self { g: [const { TransWaitingForLock::new() }; LOCK_GRAPH_DEPTH], nr: 0, printed_chain: false }
    }

    fn frames(&self) -> &[TransWaitingForLock] {
        &self.g[..self.nr]
    }

    #[cold]
    #[inline(never)]
    fn print_cycle(&self, out: &mut Printbuf) {
        write!(out, "Found lock cycle ({} entries):\n", self.nr);

        for (n, i) in self.frames().iter().enumerate() {
            let task = Trans(i.trans).task();
            if task.is_null() {
                continue;
            }

            unsafe { bch2_btree_trans_to_text(out, i.trans) };
            let _ = prt_task_backtrace(out, task, if n == 0 { 5 } else { 1 });
        }
    }

    #[cold]
    #[inline(never)]
    fn print_chain(&mut self, out: &mut Printbuf) {
        if self.printed_chain || self.nr <= 1 {
            return;
        }
        self.printed_chain = true;

        for (n, i) in self.frames().iter().enumerate() {
            let task = Trans(i.trans).task();
            if n != 0 {
                write!(out, "<- ");
            }
            write!(out, "{} ", if task.is_null() { 0 } else { unsafe { bch2_task_pid(task) } });
        }
        out.newline();
    }

    fn down(&mut self, trans: Trans) {
        /*
         * Field-by-field init rather than aggregate: we keep waitlist_snap's
         * data/size across walks so any grown heap buffer is reused.
         */
        let top = &mut self.g[self.nr];
        self.nr += 1;

        top.trans        = trans.0;
        top.node_want    = trans.locking();
        top.lock_want    = trans.lock_want();
        top.level        = 0;
        top.path_idx     = 0;
        top.waitlist_idx = 0;
        top.node_have    = null_mut();
        top.waitlist.clear();

        self.printed_chain = false;

        if self.nr > 1 && self.g[self.nr - 1].node_want != self.g[self.nr - 2].node_have {
            self.nr -= 1;
        }
    }

    /*
     * Revalidate the "who is blocked on whom" chain we've built up in @g before
     * acting on a suspected cycle.
     *
     * Between the time we descended into frame @i and now, @i's trans could have
     * acquired its lock and moved on (possibly blocked waiting on something else
     * entirely). Two staleness checks, per frame:
     *
     *   - @from->trans->locking != @from->node_want
     *     @from's trans is no longer waiting for the node we recorded at descent.
     *
     *   - i[0].node_have != i[1].node_want
     *     The parent frame is no longer looking at the node the child frame was
     *     blocked on - the edge we built between them is stale.
     *
     * Either makes the cycle hypothesis invalid; pop from the stale frame down
     * and let the caller retry.
     */
    fn remove_non_waiters(&mut self, from: usize) -> bool {
        if Trans(self.g[from].trans).locking() != self.g[from].node_want {
            self.nr = from;
            return true;
        }

        for i in from + 1..self.nr {
            if self.g[i - 1].node_have != self.g[i].node_want {
                self.nr = i;
                return true;
            }
        }

        false
    }

    fn trace_would_deadlock(&self, trans: Trans) {
        trans.fs().event_inc_trace(events::trans_restart_would_deadlock, |out| {
            write!(out, "{}\n", trans.fn_name());
            self.print_cycle(out);
        });
    }

    fn abort_lock(&self, i: usize) -> Result<(), Cycle> {
        let trans = Trans(self.g[i].trans);

        if i == 0 {
            self.trace_would_deadlock(trans);
            Err(Cycle::Restart(trans.restart_foreign_task(Errcode::BCH_ERR_transaction_restart_would_deadlock)))
        } else {
            trans.set_must_abort();
            trans.wake_up();
            Ok(())
        }
    }

    fn abort_preference(&self, l: usize, r: usize) -> usize {
        let (lt, rt) = (Trans(self.g[l].trans), Trans(self.g[r].trans));

        if lt.may_not_fail() != rt.may_not_fail() {
            return if lt.may_not_fail() { r } else { l };
        }

        if time_after64(lt.start_time(), rt.start_time()) { l } else { r }
    }

    #[cold]
    #[inline(never)]
    fn break_cycle_fail(&self) -> ! {
        let trans = Trans(self.g[0].trans);
        let fs = trans.fs();
        let mut out = Printbuf::new();

        #[cfg(kernel)]
        write!(out, "bcachefs ({}): ", fs.name().to_str().unwrap_or("?"));
        write!(out, "cycle of nofail locks\n");

        for i in self.frames() {
            unsafe { bch2_btree_trans_to_text(&mut out, i.trans) };

            write!(out, "backtrace:\n");
            {
                let out = &mut *out.indent(2);
                let _ = prt_task_backtrace(out, Trans(i.trans).task(), 2);
            }
            out.newline();
        }

        print_str(fs, LogLevel::ERR, &mut out);
        panic!("cycle of nofail locks");
    }

    #[inline(never)]
    fn break_cycle(&mut self, cycle: Option<&mut Printbuf>, from: usize) -> Result<(), Cycle> {
        if self.remove_non_waiters(from) {
            return Ok(());
        }

        /* Only checking, for debugfs: */
        if let Some(out) = cycle {
            self.print_cycle(out);
            self.nr = 0;
            return Err(Cycle::Found);
        }

        let abort = (from..self.nr).reduce(|a, i| self.abort_preference(a, i)).expect("a cycle has frames");

        if Trans(self.g[abort].trans).may_not_fail() {
            self.break_cycle_fail();
        }

        let ret = self.abort_lock(abort);
        self.nr = if ret.is_err() { 0 } else { abort };
        ret
    }

    #[cold]
    #[inline(never)]
    fn recursion_limit(&mut self, trans: Trans, cycle: Option<&mut Printbuf>) -> Result<(), Cycle> {
        if cycle.is_none() {
            trans.fs().event_inc_trace(events::trans_restart_would_deadlock_recursion_limit, |out| {
                write!(out, "{}", trans.fn_name());
            });
        }

        if Trans(self.g[0].trans).may_not_fail() {
            /* Other threads will have to rerun the cycle detector: */
            for i in &self.frames()[1..] {
                Trans(i.trans).wake_up();
            }
            return Ok(());
        }

        self.break_cycle(cycle, 0)
    }

    fn descend(&mut self, trans: Trans, cycle: Option<&mut Printbuf>) -> Result<(), Cycle> {
        if let Some(i) = self.frames().iter().position(|f| f.trans == trans.0) {
            return self.break_cycle(cycle, i);
        }

        if self.nr == LOCK_GRAPH_DEPTH {
            return self.recursion_limit(trans, cycle);
        }

        self.down(trans);
        Ok(())
    }

    #[cold]
    #[inline(never)]
    fn waitlist_alloc_failed(&self, cycle: bool) -> Cycle {
        if cycle {
            return Cycle::Found;
        }

        let trans = Trans(self.g[0].trans);
        trans.fs().event_inc_trace(events::trans_restart_deadlock_waitlist_alloc, |out| {
            write!(out, "{}", trans.fn_name());
        });

        Cycle::Restart(trans.own().restart(Errcode::BCH_ERR_transaction_restart_deadlock_waitlist_alloc))
    }

    fn scan_held_locks(&mut self, rcu: &ReadGuard, cycle: bool) -> Result<Step, Cycle> {
        let top = self.nr - 1;

        let paths = Trans(self.g[top].trans).paths();
        if paths.is_null() {
            return Ok(Step::Up);
        }

        let (allocated, nr_paths) = paths_allocated(paths);
        let mut from = self.g[top].path_idx;

        while let Some(path_idx) = allocated.next(from, nr_paths) {
            from = path_idx + 1;

            let path = unsafe { paths.add(path_idx) };
            if unsafe { (&raw const (*path).nodes_locked).read_volatile() } == 0 {
                continue;
            }

            if path_idx != self.g[top].path_idx {
                self.g[top].path_idx = path_idx;
                self.g[top].level    = 0;
            }

            while self.g[top].level < c::BTREE_MAX_DEPTH as usize {
                let level = self.g[top].level;
                let lock_held = path_locked_type(path, level);

                if lock_held == BtreeNodeLockedType::BTREE_NODE_UNLOCKED {
                    self.g[top].level += 1;
                    continue;
                }

                let b = unsafe { (&raw const (*path).l[level].b).read_volatile() };
                self.g[top].node_have = b.as_ptr();
                if !b.is_node() {
                    /*
                     * If we get here, it means we raced with the
                     * other thread updating its btree_path
                     * structures - which means it can't be blocked
                     * waiting on a lock:
                     */
                    if !self.remove_non_waiters(0) {
                        /*
                         * If lock_graph_remove_non_waiters()
                         * didn't do anything, it must be
                         * because we're being called by debugfs
                         * checking for lock cycles, which
                         * invokes us on btree_transactions that
                         * aren't actually waiting on anything.
                         * Just bail out:
                         */
                        self.nr = 0;
                    }

                    return Ok(Step::Next);
                }

                /*
                 * Lockless walk of wait_fifo: we're under guard(rcu).
                 * The fifo is rcu_dereference'd and, on realloc, the old
                 * buffer is freed via kfree_rcu_mightsleep; trans memory
                 * is RCU-deferred in bch2_trans_put; and slots only
                 * transition between NULL and a valid pointer (never torn -
                 * insert publishes .w via smp_store_release). Per-CPU cache
                 * reuse inside a grace period can aim us at a reused trans —
                 * benign, cycles missed this pass are caught next.
                 *
                 * Snapshot the conflicting trans pointers into a per-frame
                 * darray so iteration is stable across concurrent wakeups.
                 * Heap allocation is GFP_NOWAIT|__GFP_NOWARN (can't sleep
                 * under rcu+preempt). If growth past the inline buffer
                 * fails, silently truncating would risk missing a cycle;
                 * bail out with a dedicated restart type + counter so we
                 * can tell if this ever actually fires in the wild.
                 */
                let lock = unsafe { (*self.g[top].node_have).lock.six() };
                let _nowarn = MemallocFlags::nowarn();
                for (w, want) in lock.waiters(rcu) {
                    let trans = Trans::of_waiter(w.as_ptr());

                    if trans.0 != self.g[top].trans &&
                       lock_type_conflicts(lock_held, want) &&
                       self.g[top].waitlist.push(trans.0).is_err() {
                        return Err(self.waitlist_alloc_failed(cycle));
                    }
                }

                self.g[top].level += 1;

                if self.g[top].waitlist_idx < self.g[top].waitlist.len() {
                    return Ok(Step::Next);
                }
            }
        }

        Ok(Step::Up)
    }
}

/// The per-CPU graphs: a PerCpu's pointer, allocated at module init.
static GRAPHS: AtomicPtr<LockGraph> = AtomicPtr::new(null_mut());

unsafe extern "C" fn graph_init(g: *mut LockGraph, _ctx: *mut c_void, _cpu: c_uint) {
    unsafe { g.write(LockGraph::new()) }
}

pub(crate) fn graphs_init() -> Result<(), AllocError> {
    let graphs = unsafe { PerCpu::alloc_init(graph_init, null_mut::<c_void>()) }?;
    GRAPHS.store(graphs.into_raw(), Release);
    Ok(())
}

pub(crate) fn graphs_exit() {
    let graphs = GRAPHS.swap(null_mut(), Acquire);
    if graphs.is_null() {
        return;
    }

    for g in unsafe { PerCpuRef::from_raw(graphs) }.cpus() {
        unsafe { ptr::drop_in_place(g.as_ptr().cast_mut()) };
    }
    unsafe { c::free_percpu(graphs.cast()) };
}

/// With @cycle, a check, as debugfs's: see Cycle.
pub(crate) fn check_for_deadlock(trans: *mut BtreeTrans<'_>, mut cycle: Option<&mut Printbuf>)
    -> Result<(), Cycle>
{
    let trans = Trans(trans.cast());

    /* trans->paths is rcu protected vs. freeing */
    let rcu = rcu::read_lock();
    let cpu = CpuGuard::new();

    let g = unsafe { PerCpuRef::from_raw(GRAPHS.load(Acquire)).this_cpu(&cpu).as_mut() };
    g.nr = 0;

    if trans.must_abort() && !trans.may_not_fail() {
        if cycle.is_some() {
            return Err(Cycle::Found);
        }

        g.trace_would_deadlock(trans);
        return Err(Cycle::Restart(trans.own().restart(Errcode::BCH_ERR_transaction_restart_would_deadlock)));
    }

    g.down(trans);

    while g.nr != 0 {
        let top = g.nr - 1;

        if g.g[top].waitlist_idx < g.g[top].waitlist.len() {
            let next = Trans(g.g[top].waitlist[g.g[top].waitlist_idx]);
            g.g[top].waitlist_idx += 1;

            g.descend(next, cycle.as_deref_mut())?;
            continue;
        }

        g.g[top].waitlist_idx = 0;
        g.g[top].waitlist.clear();

        if let Step::Up = g.scan_held_locks(&rcu, cycle.is_some())? {
            if let Some(out) = cycle.as_deref_mut() {
                g.print_chain(out);
            }
            g.nr -= 1;
        }
    }

    Ok(())
}

fn node_reuse_race(trans: &BtreeTrans<'_>, b: &BtreeNode) -> bool {
    let (hash_val, root_id) = (trans.locking_hash_val.get(), trans.locking_root_id.get());
    if hash_val != 0 {
        hash_val != b.hash_val()
    } else if root_id.0 != u32::MAX {
        !trans.c.btree_id_root(root_id.0).is_some_and(|root| ptr::eq(root, b))
    } else {
        false
    }
}

/// The six lock's should_sleep_fn, for btree node locks: before @trans sleeps
/// on @b's lock.
pub(crate) fn bch2_six_check_for_deadlock(trans: &BtreeTrans<'_>, b: &c::btree_bkey_cached_common) -> c_int {
    /*
     * Store->load fence: orders publishing ourselves as a waiter (done by
     * six's WaitListLocked::insert() before should_sleep_fn ran) before the two
     * reads of shared state that follow. Both are the store-buffer litmus
     * test - {publish self; read shared state} racing the mirror op on
     * another CPU - and only a full barrier forbids the both-miss outcome.
     * smp_rmb() is not enough (it orders load->load, but the publish is a
     * store), and the fence cannot fold into the publish as a release: a
     * release is one-way, it orders the publish after prior writes but
     * can't fence it before these later reads. So one full barrier here,
     * serving both reads:
     *
     *  1. The walk below reads every other waiter. Two waiters closing a
     *     cycle each do {publish; look for the other}; both missing means
     *     both park with the cycle undetected - and once every participant
     *     is parked there is no next lock request, hence no next pass to
     *     catch it ("missed this pass, caught next" does not fire). The
     *     cycle must be caught on the spot.
     *
     *  2. The node-reuse check just below reads b->hash_val, pairing with
     *     the reclaim side (clear hash_val; smp_mb; six_lock_wakeup_all in
     *     bch2_btree_node_transition_state_locked). Both missing means we
     *     read a stale-valid hash_val and park on a reused node while the
     *     waker misses our just-published entry.
     *
     * Per-edge ordering - reading a waiter's held-lock state once we
     * observe its slot - is the separate smp_store_release(&.w)/
     * smp_load_acquire(&.w) pair in the wait_fifo, not this barrier.
     */
    fence(SeqCst);

    /*
     * The btree node we're about to sleep on may have been reclaimed/reused
     * since the caller picked the lock — the path's b pointer is still
     * valid memory, but the identity behind it is gone. Don't sleep on a
     * phantom; force a restart so the trans re-traverses to the real
     * current node (or learns there isn't one).
     *
     * Only btree nodes need this: interior updates take node locks
     * off-path (e.g. via btree_node_reclaim's six_trylock_intent), so the
     * cycle detector can't see the holder. Key cache entries don't have
     * that pattern — they're always held via a path the detector walks.
     *
     * Compare against the hash_val snapshotted at lock-attempt time in
     * btree_node_lock_nopath. Checking !hash_val alone is insufficient:
     * the node may already have been freed *and* re-hashed to a different
     * identity, in which case hash_val is non-zero but ≠ what we wanted.
     *
     * The store->load ordering against the reclaim side
     * (bch2_btree_node_transition_state_locked) is the smp_mb() above,
     * point 2.
     */
    if let Some(b) = BtreeNode::from_common(b) {
        if node_reuse_race(trans, b) {
            return -trans.fs().err(Errcode::BCH_ERR_no_btree_node_reused).raw();
        }
    }

    /*
     * Wake-CPU hint, set at the moment of sleep: nudge the scheduler
     * toward the CPU whose L1/L2 owns this task's shard's btree-node
     * working set. Soft — sched is free to override under load; writes
     * nothing when already matched. Placed here (vs. trans_begin)
     * because select_task_rq_fair() consults wake_cpu only at wakeup,
     * so the hint has to survive from the schedule() that follows.
     */
    let shard_cpu = trans.shard_cpu.get();
    if shard_cpu >= 0 && shard_cpu as c_uint != unsafe { bch2_raw_smp_processor_id() } {
        unsafe { bch2_set_current_wake_cpu(shard_cpu.into()) };
    }

    match check_for_deadlock(ptr::from_ref(trans).cast_mut(), None) {
        Ok(())                 => 0,
        Err(Cycle::Restart(e)) => -e.raw(),
        Err(Cycle::Found)      => -1,
    }
}
