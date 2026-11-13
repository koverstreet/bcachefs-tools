// SPDX-License-Identifier: GPL-2.0

//! The transaction: btree_trans!() and TransGuard, which own one from get to
//! put; beginning again, its memory, and restarts; the filesystem's list of
//! them, and their names.

pub use super::types::BtreeTrans;
use super::iter::{CommitOpts, TransAttempt};
use super::trans_attempt::AttemptKey;
use crate::alloc::capacity::DiskReservation;
use crate::btree::bkey::{bkey_val_to_text, BkeySC};
use crate::btree::path::{bch2_trans_paths_exit, bch2_trans_paths_init, BtreePath, PathIdx, Paths};
use crate::c;
use crate::data_structures::bitmap::Bitmap;
use crate::debug::trace::events;
use crate::errcode::{BchError, Errcode};
use crate::format::{Bpos, Btree};
use crate::fs::Fs;
use crate::journal::validate::journal_entry_to_text;
use crate::locking::MemallocFlags;
use crate::prt;
use crate::types::bits_to_longs;
use crate::util::alloc::{AllocError, Kmalloc, Vec};
use crate::util::mempool::Mempool;
use super::types::TransMem;
#[cfg(CONFIG_BCACHEFS_DEBUG)]
use crate::util::backtrace::{prt_backtrace, save_backtrace};
use crate::util::backtrace::{prt_symbol, prt_task_backtrace};
use crate::util::closure::ClosureRef;
use crate::util::ffi::{c_char, Opaque};
use crate::util::kernel::{cond_resched, local_clock, prefetch, time_after64};
#[cfg(any(CONFIG_BCACHEFS_DEBUG, CONFIG_BCACHEFS_TRANS_KMALLOC_TRACE))]
use crate::util::log::LogMsg;
use crate::util::log::{log_no_fs, LogLevel};
#[cfg(kernel)]
use crate::util::percpu::CpuGuard;
use crate::util::percpu::{bch2_task_pid, Counters};
use crate::util::rcu;
use crate::util::refcount::{RefGuard, TryRefcount};
use crate::util::sched;
use crate::util::time_stats::__bch2_time_stats_update;
use crate::util::vstructs::{Unvalidated, UnvalidatedEntries, VstructRefEntries};
use crate::util::{Printbuf, ToText};
use core::cell::UnsafeCell;
use core::ffi::{c_uint, c_ulong, CStr};
use core::marker::PhantomData;
use core::mem::{offset_of, size_of, size_of_val};
use core::ops::ControlFlow;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};

/// btree_trans!(fs): a transaction named for the function it's created in.
/// Transaction stats and lock contention are reported per function, under that
/// name; without it a transaction is "(unknown)".
///
/// XXX: C registers __func__ - a NUL-terminated string that lives forever - so
/// each call site carries a buffer to copy its name into. Clean this up once
/// enough is converted to key the stats on Rust's own names.
#[macro_export]
macro_rules! btree_trans {
    ($fs:expr) => {
        $crate::btree::trans::TransGuard::new_fn($fs, $crate::trans_fn_idx!())
    };
}

/// The index of the function this is expanded in, in TRANSACTION_FNS: its
/// name, registered by this call site's TransFnName on first use.
#[doc(hidden)]
#[macro_export]
macro_rules! trans_fn_idx {
    () => {{
        static FN: $crate::btree::trans::TransFnName = $crate::btree::trans::TransFnName::new();
        fn f() {}
        FN.idx(core::any::type_name_of_val(&f))
    }};
}

/// One btree_trans!() call site's registered name.
#[doc(hidden)]
pub struct TransFnName {
    idx:   AtomicU32,
    claim: AtomicBool,
    /* NUL-terminated, truncated to fit */
    name:  UnsafeCell<[u8; 64]>,
}

// name is written once, by the thread that claims the call site, and only read
// - by C, through TRANSACTION_FNS - once that's published it.
unsafe impl Sync for TransFnName {}

impl Default for TransFnName {
    fn default() -> Self {
        Self::new()
    }
}

impl TransFnName {
    pub const fn new() -> Self {
        TransFnName {
            idx:   AtomicU32::new(0),
            claim: AtomicBool::new(false),
            name:  UnsafeCell::new([0; 64]),
        }
    }

    /// This call site's index in TRANSACTION_FNS, registering the function
    /// @f_path names on first use: the type name of a fn item declared in it.
    /// 0 - "(unknown)" - while another thread is registering it, or if the
    /// table is full.
    pub fn idx(&self, f_path: &'static str) -> u32 {
        let idx = self.idx.load(Ordering::Acquire);
        if idx != 0 || self.claim.swap(true, Ordering::AcqRel) {
            return idx;
        }
        self.register(f_path)
    }

    #[cold]
    #[inline(never)]
    fn register(&self, f_path: &'static str) -> u32 {
        let mut name = f_path.strip_suffix("::f").unwrap_or(f_path);
        while let Some(n) = name.strip_suffix("::{{closure}}") {
            name = n;
        }
        let name = name.rsplit("::").next().unwrap_or(name);

        let buf = unsafe { &mut *self.name.get() };
        let n = name.len().min(buf.len() - 1);
        buf[..n].copy_from_slice(&name.as_bytes()[..n]);
        buf[n] = 0;

        let idx = bch2_trans_get_fn_idx(buf.as_ptr().cast());
        self.idx.store(idx, Ordering::Release);
        idx
    }
}

impl<'f> BtreeTrans<'f> {
    /// A transaction C has, for Rust that C calls with one.
    ///
    /// # Safety
    /// @raw is a live transaction, for 'a, on a filesystem that lives for 'f.
    pub unsafe fn borrow_raw<'a>(raw: *mut BtreeTrans<'_>) -> &'a BtreeTrans<'f> {
        unsafe { &*raw.cast() }
    }

    /// How many times this transaction has committed: what's cached from the
    /// btree is stale once it moves on.
    pub fn commit_count(&self) -> u32 {
        self.commit_count.get()
    }

    /// The journal sequence number the commit in progress is going into: for
    /// atomic triggers, which run with the journal reservation held.
    pub fn journal_res_seq(&self) -> u64 {
        self.journal_res_seq.get()
    }

    /// The function that got it, for messages.
    pub fn fn_name(&self) -> &'static str {
        transaction_fn_name(self.fn_idx.get())
    }

    /// Journal space for keys a trigger adds in the commit's atomic phase,
    /// through the write buffer, reserved with the transaction's own.
    pub fn reserve_extra_journal_u64s(&self, u64s: u32) {
        self.extra_journal_u64s.set(self.extra_journal_u64s.get() + u64s)
    }

    /// The attempt C began, for Rust that C calls partway through one.
    ///
    /// # Safety
    /// For C entry points only: C has an attempt in progress on this
    /// transaction, for as long as the TransAttempt lives. Nothing else can
    /// say that - Rust gets its attempts from begin() and the loops.
    pub(crate) unsafe fn attempt_in_progress<'a>(&'a self) -> TransAttempt<'a, 'f> {
        TransAttempt::new(self, self.restart_count.get())
    }

    /// bch2_trans_begin(), for a loop that tracks restarts itself: the new
    /// restart count. Inlined, so that last_begin_ip is the caller's address.
    /// Beginning is trans_attempt.rs's, which holds the key.
    #[inline(always)]
    pub(in crate::btree) fn begin_raw(&self, paths: &mut Paths<'_>, _: AttemptKey) -> u32 {
        self.begin_from(paths, crate::ret_ip!())
    }

    /// The key updates queued, without the path table borrowed: Paths::updates()
    /// is the way to them.
    ///
    /// # Safety
    /// Not held across anything that grows the path table or begins the
    /// transaction.
    pub(crate) unsafe fn updates_unguarded(&self) -> &[c::btree_insert_entry] {
        unsafe { core::slice::from_raw_parts(self.updates.get(), self.nr_updates.get() as usize) }
    }

    /// Whether anything is queued for the next commit: key updates, journal
    /// entries or accounting.
    pub fn has_updates(&self) -> bool {
        self.nr_updates.get() != 0 ||
            self.journal_entries.get().u64s != 0 ||
            self.accounting.get().u64s != 0
    }

    /// Drop everything queued for the next commit: as C's
    /// bch2_trans_reset_updates(), which every commit and begin runs.
    pub(crate) fn reset_updates(&self, paths: &mut Paths<'_>) {
        for i in 0..paths.updates().len() {
            let path = paths.updates()[i].path;
            self.path_put(paths, path, true);
        }

        self.nr_updates.set(0);
        // base too: begin empties the memory under it, and range() is bounded
        self.journal_entries.set(Default::default());
        self.accounting.set(Default::default());
        self.hooks.set(core::ptr::null_mut());
        self.extra_disk_res.set(0);
        self.extra_disk_res_replicas.set(0);
        self.extra_journal_u64s.set(0);
        self.set_has_interior_updates(false);
    }

    pub fn verify_not_restarted(&self, restart_count: u32) {
        if self.restart_count.get() != restart_count {
            self.restart_error(restart_count);
        }
    }

    pub fn restart_count(&self) -> u32 {
        self.restart_count.get()
    }

    /// For C, which has it by pointer.
    pub fn raw(&self) -> *mut BtreeTrans<'f> {
        ptr::from_ref(self).cast_mut()
    }

    /// C's path - trans->paths.
    pub(crate) fn path_idx_of(&self, path: *const BtreePath) -> PathIdx {
        unsafe { path.offset_from(self.paths.get()) as PathIdx }
    }

    /// The paths in use, by index - from 1: path 0 is never handed out.
    /// trans_for_each_path(): paths_allocated is read as it goes, as C reads
    /// it, so the loop may get and put paths.
    pub(crate) fn path_idxs(&self) -> impl Iterator<Item = PathIdx> + '_ {
        let mut idx = 1;
        core::iter::from_fn(move || {
            let i = self.next_path_idx(idx)?;
            idx = i + 1;
            Some(i)
        })
    }

    pub(crate) fn paths_allocated(&self) -> (&Bitmap, usize) {
        let nr = self.nr_paths.get() as usize;
        let words = unsafe { core::slice::from_raw_parts(self.paths_allocated.get().cast::<usize>(), bits_to_longs(nr)) };
        (Bitmap::from_words(words), nr)
    }

    fn next_path_idx(&self, idx: PathIdx) -> Option<PathIdx> {
        let (allocated, nr) = self.paths_allocated();
        allocated.next(idx as usize, nr).map(|i| i as PathIdx)
    }

    pub(crate) fn path_allocated(&self, idx: PathIdx) -> bool {
        self.paths_allocated().0.test(idx as usize)
    }

    /// How many paths are allocated - path 0, the sentinel, with them.
    pub(crate) fn nr_paths_allocated(&self) -> usize {
        self.paths_allocated().0.weight() as usize
    }

    /// TransAttempt::restart(), for what C calls in the middle of an attempt
    /// without one: the lock paths.
    pub(crate) fn restart(&self, error: Errcode) -> BchError {
        self.restart_ip(error, Self::restart as *const () as c_ulong)
    }

    /// A panic, in debug builds, if the transaction's unlocked or in a
    /// restart: for what reads the nodes its paths hold.
    pub(crate) fn verify_not_unlocked_or_in_restart(&self) {
        if cfg!(CONFIG_BCACHEFS_DEBUG) {
            if self.restarted().0 != 0 || !self.locked() {
                self.unlocked_or_in_restart_error()
            }
        }
    }

    pub fn fs_usage_delta_add(&self, d: c::bch_fs_usage_base) {
        self.fs_usage_delta.set(self.fs_usage_delta.get().wrapping_add(d));
    }

    pub(crate) fn fs(&self) -> &'f Fs {
        self.c
    }

    /// That a path of this transaction has @pos in @btree locked - as one an
    /// update went through stays, after its commit, until the transaction's
    /// unlocked. A panic if not, with debug_check_iterators on.
    pub fn assert_pos_locked(&self, btree: Btree, pos: Bpos) {
        if cfg!(CONFIG_BCACHEFS_DEBUG) && crate::debug::params::debug_check_iterators() {
            self.assert_pos_locked_now(&mut self.paths(), btree, pos)
        }
    }

    /// The static inline bch2_trans_commit_flush(), with
    /// __bch2_trans_commit()'s @lazy. journal_seq and flush are set, as C's
    /// every commit does: a C commit before ours, in the same transaction, may
    /// have left a pointer to its stack there.
    pub(super) fn commit_inner(
        &self,
        t:        &TransAttempt<'_, '_>,
        disk_res: Option<&DiskReservation<'_>>,
        flush:    Option<&Opaque<c::closure>>,
        flags:    CommitOpts,
        lazy:     bool,
    ) -> Result<(), BchError> {
        self.disk_res.set(disk_res.map_or(core::ptr::null_mut(), |r| r.as_mut_ptr()));
        self.journal_seq.set(core::ptr::null_mut());
        self.flush.set(flush.map_or(core::ptr::null_mut(), Opaque::as_ptr));
        self.commit_trans(t, flags, lazy)
    }
}

// ── The filesystem's transactions: c->btree.trans.list ──────────────────────

pub(crate) type TransList = crate::list!(BtreeTrans<'static>, list);

/// Another thread's transaction's flags: its own thread changes them -
/// READ_ONCE().
pub(crate) fn foreign_bits(trans: *const BtreeTrans<'_>) -> c::btree_trans_bits {
    unsafe { (&raw const (*trans).bits).cast::<c::btree_trans_bits>().read_volatile() }
}

/// A transaction off the list, kept live - its paths, its task - by the ref.
pub struct ListedTrans {
    trans: *mut BtreeTrans<'static>,
    _ref:  ClosureRef,
}

impl ListedTrans {
    /// Whether it holds the SRCU lock btree nodes are freed under.
    pub fn srcu_held(&self) -> bool {
        foreign_bits(self.trans).srcu_held()
    }

    pub fn backtrace_to_text(&self, out: &mut Printbuf) {
        let task = unsafe { (*self.trans).locking_wait.task.load(Ordering::Relaxed) };
        let _ = prt_task_backtrace(out, task, 0);
    }

    /// The cycle, if it's in one, written to @out.
    pub fn check_for_deadlock(&self, out: &mut Printbuf) -> bool {
        crate::btree::lock_graph::check_for_deadlock(self.trans, Some(out)).is_err()
    }
}

impl ToText for ListedTrans {
    fn to_text(&self, out: &mut Printbuf) {
        unsafe { crate::btree::lock_graph::bch2_btree_trans_to_text(out, self.trans) }
    }
}

impl Fs {
    pub(crate) fn with_trans_list<R>(&self, f: impl FnOnce(&mut TransList) -> R) -> R {
        let _guard = self.btree.trans.lock.lock();
        f(unsafe { TransList::from_c((&raw const self.btree.trans.list).cast_mut()) })
    }

    /// Whether this thread has a transaction of this filesystem's, locked.
    pub fn current_has_btree_trans(&self) -> bool {
        self.with_trans_list(|list| {
            list.iter().any(|trans| trans.locking_wait.task.load(Ordering::Relaxed) == sched::current() &&
                                    foreign_bits(trans).locked())
        })
    }

    /// Each transaction above @cursor (an address) that @pred takes, in
    /// address order, given to @f with the list unlocked - @f's to drop before
    /// anything that might wait. Printing sleeps, so the list is unlocked for
    /// @f, and the next is looked for afresh, from @cursor.
    pub fn for_each_trans_from<E>(
        &self,
        cursor: &mut usize,
        pred:   impl Fn(&ListedTrans) -> bool,
        mut f:  impl FnMut(ListedTrans) -> Result<ControlFlow<()>, E>,
    ) -> Result<(), E> {
        let addr = |t: &BtreeTrans<'_>| ptr::from_ref(t) as usize;

        while let Some(t) = self.with_trans_list(|list| loop {
            let trans = list.iter().filter(|t| addr(t) > *cursor).min_by_key(|t| addr(t))?;
            *cursor = addr(trans);

            let t = trans.ref_.tryget()
                .map(|r| ListedTrans { trans: ptr::from_ref(trans).cast_mut(), _ref: r })
                .filter(|t| pred(t));
            if t.is_some() {
                return t;
            }
        }) {
            if f(t)?.is_break() {
                break;
            }
        }
        Ok(())
    }
}

// ── Getting and putting ─────────────────────────────────────────────────────

fn bch2_trans_alloc(c: &Fs) -> NonNull<BtreeTrans<'_>> {
    #[cfg(kernel)]
    {
        let cpu = CpuGuard::new();
        let slot = crate::percpu_field!(c.btree.trans.bufs.this_cpu(&cpu), trans);
        let trans = slot.get();
        slot.set(ptr::null_mut());
        drop(cpu);

        if let Some(trans) = NonNull::new(trans) {
            /* not ->c: it stays on the list, where walkers read it */
            let start = offset_of!(BtreeTrans<'_>, paths_allocated);
            unsafe { trans.cast::<u8>().add(start).write_bytes(0, offset_of!(BtreeTrans<'_>, list) - start) };
            return trans.cast();
        }
    }

    let trans = {
        let _noio = MemallocFlags::noio();
        c.btree.trans.pool.alloc_raw()
    }.expect("a transaction outside NORECLAIM").cast::<BtreeTrans<'_>>();
    let p = trans.as_ptr();
    unsafe {
        p.write_bytes(0, 1);
        (&raw mut (*p).c).write(c);
    }

    c.with_trans_list(|list| {
        if cfg!(CONFIG_BCACHEFS_DEBUG) {
            let pid = unsafe { bch2_task_pid(sched::current()) };

            unsafe { (*p).locking_wait.task.store(sched::current(), Ordering::Relaxed) };

            for pos in list.iter() {
                let pos_task = pos.locking_wait.task.load(Ordering::Relaxed);
                /*
                 * We'd much prefer to be stricter here and completely
                 * disallow multiple btree_trans in the same thread -
                 * but the data move path calls bch2_write when we
                 * already have a btree_trans initialized.
                 */
                assert!(!(!pos_task.is_null() &&
                          pid == unsafe { bch2_task_pid(pos_task) } &&
                          foreign_bits(pos).locked()));
            }
        }

        unsafe { list.push_front(trans.cast().as_ref()) };
    });
    trans
}

/// A transaction, owned: got by new_fn(), put when dropped. btree_trans!()'s,
/// and C's from __bch2_trans_get() to bch2_trans_put(); used as the
/// &BtreeTrans it derefs to.
pub struct TransGuard<'f> {
    trans: NonNull<BtreeTrans<'f>>,
}

impl<'f> core::ops::Deref for TransGuard<'f> {
    type Target = BtreeTrans<'f>;

    fn deref(&self) -> &BtreeTrans<'f> {
        unsafe { self.trans.as_ref() }
    }
}

impl<'f> TransGuard<'f> {
    /// For C, which holds it until bch2_trans_put().
    pub fn into_raw(self) -> *mut BtreeTrans<'f> {
        core::mem::ManuallyDrop::new(self).trans.as_ptr()
    }

    /// # Safety
    /// @trans is into_raw()'s, and given back once.
    pub unsafe fn from_raw(trans: *mut BtreeTrans<'f>) -> Self {
        TransGuard { trans: unsafe { NonNull::new_unchecked(trans) } }
    }

    /// Use btree_trans!(), which names the transaction for its caller.
    #[doc(hidden)]
    pub fn new_fn(c: &'f Fs, fn_idx: u32) -> Self {
        /*
         * No multithreaded btree access until we've gone RW and are no longer
         * modifying the journal keys gap buffer
         */
        ebug_assert!(c.flags.get(c::FsFlag::may_go_rw) ||
                     c.flags.get(c::FsFlag::scrub_journal) ||
                     sched::current() == c.recovery_task.load(Ordering::Relaxed));

        let trans = TransGuard { trans: bch2_trans_alloc(c) };
        #[cfg(CONFIG_DEBUG_LOCK_ALLOC)]
        let _ = unsafe {
            ::pin_init::PinInit::__pinned_init(crate::new_lockdep_map!("bcachefs_btree"),
                                               &raw mut (*trans.trans.as_ptr()).dep_map)
        };

        /*
         * Warm the known btrees' root pointer cache lines: every btree access in
         * this trans goes through c->btree.cache.roots_b[], and the cold-line miss
         * shows up as ~3-7% of cycles in bch2_btree_path_traverse_one's
         * lock_root path. Issuing the prefetches here lets them complete
         * during the rest of trans_get's setup work.
         */
        let roots_b = &c.btree.cache.roots.roots_b;
        let roots = roots_b.as_ptr().cast::<u8>();
        for i in (0..Btree::NR as usize * size_of_val(&roots_b[0])).step_by(64) {
            prefetch(roots.wrapping_add(i));
        }

        let stats = (fn_idx < c::BCH_TRANSACTIONS_NR).then(|| &c.btree.trans.stats[fn_idx as usize]);

        let t = &*trans;

        t.fn_idx.set(fn_idx as u8);
        if let Some(s) = stats {
            t.nr_paths_max.set(s.lock.nr_max_paths.get() as _);
        }
        t.shard_cpu.set(if c.opts().shard_inode_numbers_bits.get() != 0 {
            c.inode_shard_cpu[c.inode_shard_idx() as usize].load(Ordering::Relaxed) as i16
        } else {
            -1
        });
        t.locking_wait.task.store(sched::current(), Ordering::Relaxed);
        t.set_journal_replay_not_finished(
            !c.journal.flags.get(c::JournalFlag::replay_done) &&
            c.journal_keys_get().map(RefGuard::leak).is_some());
        bch2_trans_paths_init(t);

        if let Some(s) = stats {
            let max_mem = s.lock.max_mem.get();
            if max_mem != 0 {
                let expected_mem_bytes = max_mem.next_power_of_two();

                let _nowarn = MemallocFlags::nowarn();
                let _ = t.mem_with(|m| m.reserve(expected_mem_bytes as usize));
            }
        }

        t.srcu_lock();
        t.mark_locked(false);

        let now = local_clock();
        t.locking_wait.trans_start_time.set(now);
        t.last_begin_time.set(now);

        unsafe { c::closure_init_stack_release(t.ref_.as_ptr()) };
        trans
    }
}

#[cfg(CONFIG_BCACHEFS_DEBUG)]
fn check_btree_paths_leaked(trans: &BtreeTrans<'_>, paths: &Paths<'_>) {
    let leaked = || trans.path_idxs().filter(|&i| paths[i].ref_ != 0);

    if leaked().next().is_some() {
        let c = trans.fs();
        let mut msg = LogMsg::new(c);

        prt!(&mut *msg, "btree paths leaked from {}!\n", trans.fn_name());
        for i in leaked() {
            prt!(&mut *msg, "btree {} ", paths[i].btree_id());
            prt_symbol(&mut msg, paths[i].ip_allocated());
            prt!(&mut *msg, "\n");
        }

        c.emergency_read_only(&mut msg);
    }
}

#[cfg(not(CONFIG_BCACHEFS_DEBUG))]
fn check_btree_paths_leaked(_trans: &BtreeTrans<'_>, _paths: &Paths<'_>) {}

unsafe extern "C" fn trans_rcu_free(rcu: *mut c::rcu_head) {
    unsafe {
        let trans = rcu.byte_sub(offset_of!(BtreeTrans<'_>, rcu)).cast::<BtreeTrans<'_>>();
        (*trans).c.btree.trans.pool.free_raw(NonNull::new_unchecked(trans.cast()));
    }
}

impl Drop for TransGuard<'_> {
    fn drop(&mut self) {
        let t = &**self;
        let c = t.fs();

        if t.restarted().0 != 0 {
            t.in_restart_error();
        }

        t.unlock_long();

        {
            let mut paths = t.paths();

            for i in 0..paths.updates().len() {
                let path = paths.updates()[i].path;
                paths.put_ll(path, true);
            }
            t.nr_updates.set(0);

            check_btree_paths_leaked(t, &paths);
        }

        if t.journal_replay_not_finished() {
            drop(unsafe { RefGuard::adopt(&c.journal_keys) });
        }

        /*
         * trans->ref protects trans->locking_wait.task, btree_paths array; used
         * by cycle detector
         */
        unsafe { c::bch2_closure_return_sync(t.ref_.as_ptr()) };
        t.locking_wait.task.store(ptr::null_mut(), Ordering::Relaxed);

        #[cfg(CONFIG_BCACHEFS_DEBUG)]
        drop(t.last_restarted_trace.take());
        #[cfg(CONFIG_BCACHEFS_TRANS_KMALLOC_TRACE)]
        drop(t.trans_kmalloc_trace.take());

        bch2_trans_paths_exit(t);

        t.mem.take().free(&c.btree.trans.malloc_pool);

        let trans = self.trans.as_ptr();

        /* Userspace doesn't have a real percpu implementation: */
        #[cfg(kernel)]
        let trans = {
            let cpu = CpuGuard::new();
            let slot = crate::percpu_field!(c.btree.trans.bufs.this_cpu(&cpu), trans);
            let old = slot.get();
            slot.set(trans.cast());
            old
        };

        if !trans.is_null() {
            c.with_trans_list(|list| unsafe { list.remove(&*trans.cast()) });

            /*
             * RCU-defer the mempool return: the cycle detector walks
             * wait_fifos under RCU without holding closure refs, so other
             * threads may be mid-access of this trans via a pointer they
             * read from a wait_fifo slot. Grace period ensures the memory
             * isn't reused for a different struct before those walks
             * finish; per-CPU cache reuse within a grace period is
             * identity-confusion (acceptable per probabilistic detection).
             */
            unsafe { rcu::Flavor::RCU.call(&raw mut (*trans).rcu, trans_rcu_free) };
        }
    }
}

// ── Memory, and beginning again ─────────────────────────────────────────────

impl TransMem {
    /// @u64s zeroed, off the top - None if they don't fit: it's never
    /// reallocated here, what it's handed out being in use.
    pub(crate) fn alloc(&mut self, u64s: usize) -> Option<NonNull<u64>> {
        self.buf.extend_zeroed_within_capacity(u64s).map(|s| NonNull::from(s).cast())
    }

    pub(crate) fn top_bytes(&self) -> usize {
        self.buf.len() * size_of::<u64>()
    }

    pub(crate) fn bytes(&self) -> usize {
        self.buf.capacity() * size_of::<u64>()
    }

    /// Whether @p is in what it's handed out.
    pub(crate) fn contains(&self, p: *const u8) -> bool {
        self.buf.as_ptr_range().contains(&p.cast())
    }

    /// @len u64s, from @start, of what it's handed out.
    pub(crate) fn range(&mut self, start: usize, len: usize) -> NonNull<[u64]> {
        NonNull::from(&mut self.buf[start..][..len])
    }

    /// Room for @bytes in all - the first buffer, or begin's: what's there is
    /// kept if it can't be had.
    fn reserve(&mut self, bytes: usize) -> Result<(), AllocError> {
        self.buf.reserve((bytes / size_of::<u64>()).saturating_sub(self.buf.len()))
    }

    /// A mempool element, in place of what it had, which is freed.
    fn adopt_pool_buf(&mut self, p: NonNull<u8>) {
        self.buf = unsafe { Vec::from_raw_parts(p.as_ptr().cast(), 0, c::BTREE_TRANS_MEM_MAX as usize / size_of::<u64>()) };
        self.used_mempool = true;
    }

    /// Its buffer freed: given back to the pool, if it's one of its elements.
    fn free(self, pool: &Mempool<Kmalloc>) {
        if self.used_mempool {
            let (p, _, _) = self.buf.into_raw_parts();
            unsafe { pool.free_raw(NonNull::new_unchecked(p).cast()) };
        }
    }
}

impl BtreeTrans<'_> {
    /// @f on the transaction's memory, out of its cell meanwhile.
    pub(crate) fn mem_with<R>(&self, f: impl FnOnce(&mut TransMem) -> R) -> R {
        let mut mem = self.mem.take();
        let ret = f(&mut mem);
        self.mem.set(mem);
        ret
    }
}

#[cfg(CONFIG_BCACHEFS_TRANS_KMALLOC_TRACE)]
#[cold]
pub(crate) fn kmalloc_trace_to_text(out: &mut Printbuf, trace: &[c::trans_kmalloc_trace]) {
    out.tabstops_reset();
    out.tabstop_push(60);

    for i in trace {
        prt_symbol(out, i.ip);
        prt!(out, "\t{}\n", i.bytes);
    }
}

impl BtreeTrans<'_> {
    /// @alloc in a NORECLAIM scope, and if it fails, again with the transaction
    /// unlocked - and left unlocked: allocate_dropping_locks_norelock().
    pub(crate) fn allocate_dropping_locks_norelock<T>(&self, paths: &mut Paths<'_>, mut alloc: impl FnMut() -> Option<T>)
        -> Option<T>
    {
        let first = {
            let _noreclaim = MemallocFlags::noreclaim();
            alloc()
        };
        first.or_else(|| {
            self.unlock_from(paths, crate::ret_ip!());
            alloc()
        })
    }

    pub(crate) fn maybe_inject_restart(&self, ip: c_ulong) -> Result<(), BchError> {
        #[cfg(CONFIG_BCACHEFS_INJECT_TRANSACTION_RESTARTS)]
        if crate::util::kernel::ktime_get_ns() & !(!0u64 << (10 + self.restart_count_this_trans.get()).min(63)) == 0 {
            self.fs().event_inc_trace(events::trans_restart_injected, |out| prt!(out, "{}", self.fn_name()));
            return Err(self.restart_ip(Errcode::BCH_ERR_transaction_restart_fault_inject, ip));
        }
        #[cfg(not(CONFIG_BCACHEFS_INJECT_TRANSACTION_RESTARTS))]
        let _ = ip;
        Ok(())
    }

    /// @bytes of the transaction's memory, or the transaction restarted to
    /// grow it.
    pub(crate) fn kmalloc_in(&self, bytes: usize) -> Result<*mut u8, BchError> {
        let size = bytes.next_multiple_of(size_of::<u64>());

        match self.mem_with(|m| m.alloc(size / size_of::<u64>())) {
            Some(p) => Ok(p.as_ptr().cast()),
            None    => Err(self.kmalloc_grow(size, crate::ret_ip!())),
        }
    }

    /// @size bytes that don't fit: the memory grown by the next begin, the
    /// transaction restarted for it - so the memory never moves within an
    /// attempt.
    pub(crate) fn kmalloc_grow(&self, size: usize, ret_ip: c_ulong) -> BchError {
        let c = self.fs();
        let (top, old_bytes) = self.mem_with(|m| (m.top_bytes(), m.bytes()));
        let new_top = (top + size) as c_uint;
        let new_bytes = new_top.next_power_of_two();

        if crate::warn_on_once!(new_bytes > c::BTREE_TRANS_MEM_MAX) {
            #[cfg(CONFIG_BCACHEFS_TRANS_KMALLOC_TRACE)]
            {
                let mut msg = LogMsg::new(c);
                prt!(&mut *msg, "bump allocator exceeded BTREE_TRANS_MEM_MAX ({})\n", c::BTREE_TRANS_MEM_MAX);

                let trace = self.trans_kmalloc_trace.take();
                kmalloc_trace_to_text(&mut msg, &trace);
                self.trans_kmalloc_trace.set(trace);
            }
        }

        if let Err(err) = self.maybe_inject_restart(ret_ip) {
            return err;
        }

        let s = &c.btree.trans.stats[self.fn_idx.get() as usize];
        if new_bytes > s.lock.max_mem.get() {
            let mut s = s.lock.lock();
            #[cfg(CONFIG_BCACHEFS_TRANS_KMALLOC_TRACE)]
            {
                let trace = self.trans_kmalloc_trace.take();
                let v = &mut s.trans_kmalloc_trace;
                v.clear();
                let _ = v.reserve(trace.len());
                let nr = v.capacity().min(trace.len());
                let _ = v.extend_from_slice(&trace[..nr]);
                self.trans_kmalloc_trace.set(trace);
            }
            s.published().max_mem.set(new_bytes);
        }

        if self.mem_with(|m| m.used_mempool) || new_bytes > c::BTREE_TRANS_MEM_MAX {
            ebug_assert!(old_bytes < new_bytes as usize);
            return Errcode::BCH_ERR_ENOMEM_trans_kmalloc.into();
        }

        self.mem_with(|m| m.realloc_bytes_required = new_bytes);
        c.event_inc_trace(events::trans_restart_mem_realloced, |out| {
            prt!(out, "{}\nbytes {}\n", self.fn_name(), new_bytes);
        });
        self.restart_ip(Errcode::BCH_ERR_transaction_restart_mem_realloced, ret_ip)
    }

    /**
     * bch2_trans_begin() - reset a transaction after a interrupted attempt
     *
     * Returns:	current restart counter, to be used with trans_was_restarted()
     *
     * While iterating over nodes or updating nodes a attempt to lock a btree node
     * may return BCH_ERR_transaction_restart when the trylock fails. When this
     * occurs bch2_trans_begin() should be called and the transaction retried.
     */
    fn begin_from(&self, paths: &mut Paths<'_>, ret_ip: c_ulong) -> u32 {
        let c = self.fs();

        c.event_inc_trace(events::transaction_begin, |out| {
            let (allocated, nr) = self.paths_allocated();
            prt!(out, "{}\nnr_paths {}/{}\n", self.fn_name(), allocated.weight(), nr);
        });

        /*
         * Discarding queued updates is correct after a transaction restart
         * (the restart loop re-queues them) and on error unwind — but a new
         * iteration starting with updates queued and no restart in flight
         * means the previous iteration forgot to commit them, and they're
         * about to be lost silently:
         */
        crate::warn_on_once!(cfg!(CONFIG_BCACHEFS_DEBUG) &&
                             self.has_updates() &&
                             self.restarted().0 == 0 && !self.in_traverse_all() &&
                             !self.begin_may_drop_updates());

        self.reset_updates(paths);

        self.restart_count.set(self.restart_count.get() + 1);
        self.mem_with(|m| m.buf.clear());

        /*
         * Long-lived trans (e.g. copygc) initialized before journal replay
         * finished hold a journal_keys ref and stay on the journal-overlay
         * iter path forever, reading stale post-replay journal_keys data.
         * Drop the ref + clear the flag once replay completes; subsequent
         * trans_begin calls see the flag already clear and skip cheaply.
         */
        if self.journal_replay_not_finished() &&
           c.journal.flags.get(c::JournalFlag::replay_done) {
            drop(unsafe { RefGuard::adopt(&c.journal_keys) });
            self.set_journal_replay_not_finished(false);
        }

        if self.restarted() == Errcode::BCH_ERR_transaction_restart_mem_realloced {
            self.mem_realloc(paths);
        }

        for i in self.path_idxs() {
            let free = {
                let path = &mut paths[i];

                path.set_should_be_locked(false);

                /*
                 * If the transaction wasn't restarted, we're presuming to be
                 * doing something new: dont keep iterators excpt the ones that
                 * are in use - except for the subvolumes btree:
                 */
                if self.restarted().0 == 0 && path.btree_id() != Btree::subvolumes {
                    path.set_preserve(false);
                }

                /*
                 * XXX: we probably shouldn't be doing this if the transaction
                 * was restarted, but currently we still overflow transaction
                 * iterators if we do that
                 */
                let free = path.ref_ == 0 && !path.preserve();
                if !free {
                    path.set_preserve(false);
                }
                free
            };
            if free {
                self.path_free(paths, i);
            }
        }

        let now = local_clock();

        if !cfg!(CONFIG_BCACHEFS_NO_LATENCY_ACCT) &&
           time_after64(now, self.last_begin_time.get() + 10) {
            __bch2_time_stats_update(&c.btree.trans.stats[self.fn_idx.get() as usize].duration,
                                     self.last_begin_time.get(), now);
        }

        self.srcu_begin(paths, ret_ip);

        if self.restarted().0 == 0 {
            self.locking_wait.trans_start_time.set(now);
        }
        self.last_begin_time.set(now);

        self.last_begin_ip.set(ret_ip);

        #[cfg(CONFIG_BCACHEFS_INJECT_TRANSACTION_RESTARTS)]
        if self.restarted().0 != 0 {
            self.restart_count_this_trans.set(self.restart_count_this_trans.get() + 1);
        } else {
            self.restart_count_this_trans.set(0);
        }

        #[cfg(CONFIG_BCACHEFS_TRANS_KMALLOC_TRACE)]
        {
            let mut trace = self.trans_kmalloc_trace.take();
            trace.clear();
            self.trans_kmalloc_trace.set(trace);
        }

        if self.restarted().0 != 0 {
            /* restart is hot — skip the resched check */
            let _ = self.path_traverse_all(paths);
            self.clear_notrace_relock_fail();
        } else if sched::need_resched() ||
                  time_after64(now, self.locking_wait.trans_start_time.get() +
                                    c::BTREE_TRANS_MAX_LOCK_HOLD_TIME_NS as u64) {
            self.unlock_from(paths, ret_ip);
            cond_resched();
        }

        self.mark_locked(false);

        self.verify_not_unlocked_or_in_restart();
        self.restart_count.get()
    }

    /// begin_from()'s, after a restart for room: the memory grown to what the
    /// attempt asked for - or, failing that, a mempool element.
    #[cold]
    #[inline(never)]
    fn mem_realloc(&self, paths: &mut Paths<'_>) {
        let new_bytes = self.mem_with(|m| m.realloc_bytes_required);
        ebug_assert!(new_bytes <= c::BTREE_TRANS_MEM_MAX);

        let reserved = self.allocate_dropping_locks_norelock(paths, || {
            self.mem_with(|m| m.reserve(new_bytes as usize)).ok()
        });
        if reserved.is_none() {
            let p = self.fs().btree.trans.malloc_pool.alloc_raw().expect("transaction memory, its locks dropped");
            self.mem_with(|m| m.adopt_pool_buf(p));
        }
    }

    /// The updates queued, for an error message.
    #[cold]
    #[inline(never)]
    pub fn updates_to_text(&self, out: &mut Printbuf) {
        let c = self.fs();

        prt!(out, "{} transaction updates for {} journal seq {}\n",
             self.nr_updates.get(), self.fn_name(), self.journal_res_seq.get());
        let mut out = out.indent(2);

        /* from wherever an error's found - path table borrowed or not; this reads it and is done */
        for i in unsafe { self.updates_unguarded() } {
            let k = i.key();
            let old = BkeySC { k: &i.old_k, v: i.old_v, iter: PhantomData, val: PhantomData };

            prt!(out, "update: btree={} cached={} ", i.bits.btree_id(), u32::from(i.bits.cached()));
            prt_symbol(&mut out, i.ip_allocated);
            prt!(out, "\n");

            let mut out = out.indent(2);

            prt!(out, "old ");
            bkey_val_to_text(&mut out, Some(c), old);
            prt!(out, "\n");

            prt!(out, "new ");
            bkey_val_to_text(&mut out, Some(c), BkeySC::from(k));
            prt!(out, "\n");
        }

        let subbuf = |base: u16, u64s: u16| {
            let p = self.mem_with(|m| m.range(usize::from(base), usize::from(u64s)));
            unsafe { core::slice::from_raw_parts(p.cast::<u8>().as_ptr(), p.len() * size_of::<u64>()) }
        };

        let journal_entries = self.journal_entries.get();
        for e in UnvalidatedEntries::<c::jset_entry<Unvalidated>>::new(subbuf(journal_entries.base, journal_entries.u64s))
            .map_while(Result::ok)
        {
            journal_entry_to_text(&mut out, Some(c), e);
            prt!(out, "\n");
        }

        let accounting = self.accounting.get();
        for k in VstructRefEntries::<c::bkey_i>::new(subbuf(accounting.base, accounting.u64s)).map_while(Result::ok) {
            bkey_val_to_text(&mut out, Some(c), BkeySC::from_bkey_i(&k));
            prt!(out, "\n");
        }
    }
}

// ── Restarts ────────────────────────────────────────────────────────────────

impl BtreeTrans<'_> {
    /// restart_ip(), less the backtrace: for a transaction this thread may not
    /// be running, whose restart this thread's backtrace wouldn't explain.
    pub(crate) fn restart_foreign_task(&self, err: Errcode, ip: c_ulong) -> BchError {
        assert!(BchError::from(err).matches(Errcode::BCH_ERR_transaction_restart));

        self.set_restarted(err);
        self.last_restarted_ip.set(ip);
        err.into()
    }

    pub(crate) fn restart_ip(&self, err: Errcode, ip: c_ulong) -> BchError {
        self.restart_foreign_task(err, ip);
        #[cfg(CONFIG_BCACHEFS_DEBUG)]
        {
            let _noreclaim = MemallocFlags::noreclaim();
            let mut trace = self.last_restarted_trace.take();
            let _ = save_backtrace(&mut trace, sched::current(), 0);
            self.last_restarted_trace.set(trace);
        }
        err.into()
    }

    #[cold]
    #[inline(never)]
    fn restart_error(&self, restart_count: u32) -> ! {
        let mut ip = Printbuf::new();
        prt_symbol(&mut ip, self.last_begin_ip.get());
        panic!("trans->restart_count {}, should be {}, last restarted by {ip}", self.restart_count.get(), restart_count);
    }

    #[cold]
    #[inline(never)]
    fn in_restart_error(&self) -> ! {
        let err = BchError::from(self.restarted());
        let mut by = Printbuf::new();
        #[cfg(CONFIG_BCACHEFS_DEBUG)]
        {
            prt_backtrace(&mut by, &self.last_restarted_trace.take());
            panic!("in transaction restart: {}, last restarted by\n{by}", err.msg());
        }
        #[cfg(not(CONFIG_BCACHEFS_DEBUG))]
        {
            prt_symbol(&mut by, self.last_restarted_ip.get());
            panic!("in transaction restart: {}, last restarted by {by}", err.msg());
        }
    }

    #[cold]
    #[inline(never)]
    fn unlocked_or_in_restart_error(&self) -> ! {
        if self.restarted().0 != 0 {
            self.in_restart_error();
        }

        if !self.locked() {
            let mut by = Printbuf::new();
            prt_symbol(&mut by, self.last_unlock_ip.get());
            panic!("trans should be locked, unlocked by {by}");
        }

        panic!("BUG");
    }
}

// ── Transaction names ───────────────────────────────────────────────────────

/// Each btree_trans!() call site's and C caller's name, by fn_idx: the
/// transaction's, and its stats'. 0 is "(unknown)". A name is a TransFnName's,
/// written as it's registered: published by a release, read by an acquire.
static TRANSACTION_FNS: [AtomicPtr<c_char>; c::BCH_TRANSACTIONS_NR as usize] = {
    let mut fns = [const { AtomicPtr::new(ptr::null_mut()) }; c::BCH_TRANSACTIONS_NR as usize];
    fns[0] = AtomicPtr::new(c"(unknown)".as_ptr().cast_mut().cast());
    fns
};

/// The name registered at @idx, if one is.
pub fn transaction_fn(idx: usize) -> Option<&'static CStr> {
    let p = TRANSACTION_FNS.get(idx)?.load(Ordering::Acquire);
    (!p.is_null()).then(|| unsafe { CStr::from_ptr(p.cast()) })
}

/// The name registered at @idx, for messages.
pub fn transaction_fn_name(idx: u8) -> &'static str {
    transaction_fn(idx.into()).and_then(|s| s.to_str().ok()).unwrap_or("?")
}

/// The names registered, by index, up to the first slot that isn't.
pub fn transaction_fns() -> impl Iterator<Item = &'static CStr> {
    (0..TRANSACTION_FNS.len()).map_while(transaction_fn)
}

pub fn bch2_trans_get_fn_idx(fn_: *const c_char) -> c_uint {
    for (i, slot) in TRANSACTION_FNS.iter().enumerate() {
        let cur = slot.load(Ordering::Acquire);
        if cur.is_null() || ptr::eq(cur, fn_) {
            slot.store(fn_.cast_mut(), Ordering::Release);
            return i as c_uint;
        }
    }

    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        log_no_fs(LogLevel::WARN, crate::message!("BCH_TRANSACTIONS_NR not big enough!"));
    }
    0
}

// ── C's entry points ────────────────────────────────────────────────────────

pub fn __bch2_trans_get(c: &Fs, fn_idx: c_uint) -> *mut BtreeTrans<'static> {
    TransGuard::new_fn(c, fn_idx).into_raw().cast()
}

