// SPDX-License-Identifier: GPL-2.0

//! The transaction: btree_trans!() and TransGuard, which own one from get to
//! put; beginning again, its memory, and restarts; the filesystem's list of
//! them, and their names.

use super::iter::{CommitOpts, TransAttempt};
use super::trans_attempt::AttemptKey;
use crate::alloc::{capacity::DiskReservation, buckets::FsUsageBase};
use crate::btree::bkey::{bkey_val_to_text, BkeySC, BkeyI, BKEY_U64S};
use crate::btree::path::{trans_paths_exit, trans_paths_init, table_arrays, PathIdx, Paths,
    BTREE_ITER_INITIAL, BtreeTransPaths, BtreePath};
use crate::c;
use crate::btree::FsBtree;
use crate::btree::locking::BtreeBkeyCachedCommon;
use crate::btree::update::{BtreeInsertEntry, BtreeTransCommitHook, BtreeTransSubbuf};
use crate::c::rcu_head;
use crate::data::extents_format::BKEY_BTREE_PTR_VAL_U64s_MAX;
use crate::journal::journal::JournalFlag;
use crate::journal::pins::JournalEntryPin;
use crate::locking::seqmutex::Seqmutex;
use crate::locking::six::SixLockWaiter;
use crate::journal::flush_wait::Flush;
use crate::data_structures::{bitmap::Bitmap, dst_room::DstRoom};
use crate::debug::trace::events;
use crate::errcode::{BchError, Errcode};
use crate::format::{Bpos, Btree, JsetEntry};
use crate::fs::{Fs, FsFlag};
use crate::journal::validate::journal_entry_to_text;
use crate::locking::{MemallocFlags, Locked, LockedBy, Mutex};
use crate::{prt, new_lock};
use crate::types::{bits_to_longs, NeBytes};
use crate::util::alloc::{AllocError, Vec, Global};
use crate::util::mempool::Mempool;
use core::cell::Cell;
use core::sync::atomic::AtomicU8;
use crate::os::percpu::PerCpu;
use crate::util::ffi::Opaque;
use crate::util::projections::Volatile;
use crate::util::rcu::Srcu;
use crate::util::time_stats::{TimeStatsArray, TimeStats};
use crate::util::vstructs::Validated;
use cstruct_macros::{bitfield, BitfieldMembers, Init, MaybeZeroable, placed, TypeInfo};
use pin_init::pin_data;

pub const BTREE_TRANS_MEM_MAX: u32 = 1 << 16;

pub const BTREE_TRANS_MAX_LOCK_HOLD_TIME_NS: isize = c::NSEC_PER_MSEC as isize;

#[repr(C)]
#[derive(Clone, Copy, Default, MaybeZeroable)]
pub struct TransKmallocTrace {
    pub ip: usize,
    pub bytes: usize,
}

/// The transaction's memory: a vector of u64s, bump-allocated from - what it
/// hands out is in use, by reference - and cleared on every begin; so it's
/// reallocated only by begin, when nothing it handed out is used again.
#[repr(C)]
#[derive(MaybeZeroable, Default)]
pub struct TransMem {
    pub(crate) buf:                    crate::util::alloc::Vec<u64, Global, u32>,
    /* bytes: what the next begin grows it to, restarted for room */
    pub(crate) realloc_bytes_required: u32,
    /* a mempool element: the pool's to have back */
    pub(crate) used_mempool:           bool,
}

/*
 * Transaction context for btree operations.
 *
 * Holds iterators/paths, pending updates, locks, and a memory arena for a
 * single logical btree operation.  On lock contention or memory pressure,
 * the transaction restarts: releases all locks, resets state, and retries
 * (via lockrestart_do() or similar retry loops).
 *
 *  - mem/mem_top: bump allocator, invalidated on every restart
 *  - Paths kept sorted in lock order to prevent deadlocks
 *  - SRCU read lock protects btree node memory from being freed;
 *    released periodically to avoid stalling reclaim
 */
#[bitfield(u64, repr = NeBytes<6>, from = NeBytes::<6>::from_u64, into = NeBytes::<6>::to_u64)]
pub struct BtreeTransBits {
    lock_may_not_fail:              bool,
    locked:                         bool,
    migrate_disabled:               bool,
    write_locked:                   bool,
    srcu_held:                      bool,
    /*
     * IO was submitted under the srcu read lock (submit_bio() can block for
     * an unbounded time): suppress the next bch2_trans_unlock_long() "held
     * too long" warning. Scoped to one locked attempt — cleared on relock /
     * bch2_trans_begin().
     */
    srcu_io_submitted:              bool,
    btree_cache_cannibalize_locked: bool,
    pf_memalloc_noio:               bool,
    in_traverse_all:                bool,
    paths_sorted:                   bool,
    memory_allocation_failure:      bool,
    journal_transaction_names:      bool,
    journal_replay_not_finished:    bool,
    pub(self) notrace_relock_fail:  bool,
    /*
     * Exempt bch2_trans_begin() from the dropped-updates warning. Set
     * around a nested transaction - one that grabs trans->restart_count,
     * does work whose inner commits discard the outer's queued updates, and
     * returns trans_was_restarted() (e.g. fsck counting i_sectors, which
     * spans too many extents to be a single transaction). The begin can't
     * see the restart that's about to be returned, so the caller vouches
     * for it here. Set it with CLASS(trans_may_drop_updates), which
     * restores it at the end of the scope.
     */
    begin_may_drop_updates:         bool,
    has_interior_updates:           bool,
    /* Rust's: the path table is borrowed (btree/path.rs's Paths) */
    paths_borrowed:                 bool,
    /* Rust's: the TransAttempts made and not yet ended */
    #[bits(8)] attempts_live:       u8,
    __pad25:                        u7,
    #[bits(16, from = BchError::opt_from_bits, into = BchError::opt_into_bits)]
    pub restarted: Option<BchError>,
    __pad:                          u16,
}

/*
 * Transaction context for btree operations.
 *
 * Holds iterators/paths, pending updates, locks, and a memory arena for a
 * single logical btree operation.  On lock contention or memory pressure,
 * the transaction restarts: releases all locks, resets state, and retries
 * (via lockrestart_do() or similar retry loops).
 *
 *  - mem/mem_top: bump allocator, invalidated on every restart
 *  - Paths kept sorted in lock order to prevent deadlocks
 *  - SRCU read lock protects btree node memory from being freed;
 *    released periodically to avoid stalling reclaim
 */
#[repr(C)]
#[derive(MaybeZeroable, BitfieldMembers)]
pub struct BtreeTrans<'f> {
    pub c: &'f Fs,

    /* the path table, by its paths: TableLayout has the rest around them */
    pub paths: Cell<*mut BtreePath>,

    /* bump allocator, invalidated on transaction restart */
    pub mem: Cell<TransMem>,
    #[cfg(CONFIG_BCACHEFS_TRANS_KMALLOC_TRACE)]
    pub trans_kmalloc_trace: Cell<Vec<TransKmallocTrace>>,

    pub nr_sorted: Cell<PathIdx>,
    pub nr_paths: Cell<PathIdx>,
    pub nr_paths_max: Cell<PathIdx>,
    pub nr_updates: Cell<PathIdx>,
    pub shard_cpu: Cell<i16>,
    pub fn_idx: Cell<u8>,
    pub lock_must_abort: AtomicU8,
    #[members]
    pub bits: Cell<BtreeTransBits>,
    pub restart_count: Cell<u32>,
    #[cfg(CONFIG_BCACHEFS_INJECT_TRANSACTION_RESTARTS)]
    pub restart_count_this_trans: Cell<u32>,
    /*
     * Incremented on every successful (non-empty) commit; for detecting
     * that cached state derived from btree reads may be stale:
     */
    pub commit_count: Cell<u32>,

    pub last_begin_time: Cell<u64>,
    pub last_begin_ip: Cell<usize>,
    pub last_restarted_ip: Cell<usize>,
    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    pub last_restarted_trace: Cell<Vec<usize>>,
    pub last_unlock_ip: Cell<usize>,
    pub srcu_lock_time: Cell<usize>,
    pub srcu_idx: Cell<i32>,
    pub locking_root_id: Cell<Btree>,

    pub locking_hash_val: Cell<u64>,
    pub locking: Cell<*mut BtreeBkeyCachedCommon>,
    /*
     * Snapshot of locking->{btree}.hash_val at lock-attempt time, used by
     * six_check_for_deadlock() to detect that the node identity
     * rotated while we were about to sleep on it. 0 for cached entries.
     */
    pub locking_wait: SixLockWaiter,

    /* update path: */
    pub journal_entries: Cell<BtreeTransSubbuf<JsetEntry<Validated>>>,
    pub accounting: Cell<BtreeTransSubbuf<BkeyI>>,

    pub hooks: Cell<*mut BtreeTransCommitHook>,
    pub journal_pin: Cell<*mut JournalEntryPin>,

    pub journal_res_seq: Cell<u64>,
    /*
     * Input for BCH_TRANS_COMMIT_no_journal_res: with no reservation to
     * derive a seq from, the caller names the journal entry the btree node
     * write has to pin.
     */
    pub journal_seq_to_pin: Cell<u64>,
    pub journal_seq: Cell<*mut u64>,
    pub disk_res: Cell<*mut DiskReservation<'static>>,
    pub flush: Cell<*mut Option<crate::journal::flush_wait::Flush<'static>>>,

    pub fs_usage_delta: Cell<FsUsageBase>,

    pub journal_u64s: Cell<u32>,
    pub extra_journal_u64s: Cell<u32>,

    /*
     * Space a trigger needs on top of @disk_res, charged at commit.
     * Charge sites may disagree on the count, so keep the max: erring
     * high only over-reserves.
     */
    pub extra_disk_res: Cell<u64>,
    pub extra_disk_res_replicas: Cell<u8>,

    pub btree_path_down: LockedBy<DstRoom<BkeyI, [u64; BKEY_U64S + BKEY_BTREE_PTR_VAL_U64s_MAX]>>,

    #[cfg(CONFIG_DEBUG_LOCK_ALLOC)]
    pub dep_map: crate::locking::LockdepMap,
    /* Entries before this are zeroed out on every bch2_trans_get() call */

    pub list: c::list_head,
    pub ref_: crate::util::refcount::AtomicRef<()>,
    pub rcu: rcu_head,

    /* the first path table: Paths borrows into it */
    pub _paths_allocated: Opaque<[usize; bits_to_longs(BTREE_ITER_INITIAL as usize)]>,
    pub trans_paths: Opaque<BtreeTransPaths>,
    pub _paths: Opaque<[BtreePath; BTREE_ITER_INITIAL as usize]>,
    pub _sorted: Opaque<[PathIdx; BTREE_ITER_INITIAL as usize + 4]>,
    pub _updates: Opaque<[BtreeInsertEntry; BTREE_ITER_INITIAL as usize]>,
}

impl BtreeTrans<'_> {
    pub fn notrace_relock_fail(&self) -> bool {
        self.bits.get().notrace_relock_fail()
    }

    /// Relock failures after IO it submitted are expected, not counted.
    pub fn set_notrace_relock_fail(&self) {
        self.bits.set(self.bits.get().with_notrace_relock_fail(true))
    }

    /// For the next attempt: begin()'s, after a restart.
    pub fn clear_notrace_relock_fail(&self) {
        self.bits.set(self.bits.get().with_notrace_relock_fail(false))
    }
}

#[repr(C)]
#[derive(Clone, Copy, MaybeZeroable)]
pub struct BtreeTransBuf {
    pub trans: *mut BtreeTrans<'static>,
}

/// A transaction function's stats' lock's: what's kept of its biggest - its
/// memory's allocations, and its paths, as text.
#[repr(C)]
#[derive(MaybeZeroable)]
pub struct TransStatsLocked {
    #[cfg(CONFIG_BCACHEFS_TRANS_KMALLOC_TRACE)]
    pub trans_kmalloc_trace: Vec<TransKmallocTrace>,
    pub max_paths_text: Printbuf,
}

crate::published! {
    /// Its high water marks: raised under the lock, read without it - by
    /// btree_trans!(), sizing the transaction it gets.
    #[derive(MaybeZeroable)]
    pub struct TransStatsMax {
        pub nr_max_paths: Volatile<u32>,
        pub max_mem: Volatile<u32>,
    }
}

#[pin_data]
#[repr(C)]
#[derive(MaybeZeroable, Init)]
pub struct BtreeTransactionStats {
    #[pin]
    #[init(crate::new_lock!(<- ::pin_init::init_zeroed(), ::pin_init::zeroed(), "&s->lock"))]
    pub lock: Locked<Mutex, TransStatsLocked, TransStatsMax>,
}

pub const TRANSACTIONS_NR: u32 = 128;

#[placed(FsBtree.trans)]
#[pin_data]
#[repr(C)]
#[derive(MaybeZeroable, TypeInfo, Init)]
pub struct FsBtreeTrans {
    #[pin]
    /* the filesystem's transactions: zeroed, it's empty - List's first push sets it up */
    #[init(new_lock!(::pin_init::zeroed(), "&c->btree.trans.lock"))]
    pub list: Locked<Seqmutex, crate::btree::trans::TransList>,
    #[pin]
    #[init(Mempool::new(1, core::mem::size_of::<BtreeTrans<'_>>()))]
    pub pool: Mempool,
    #[pin]
    #[init(Mempool::new(1, BTREE_TRANS_MEM_MAX as usize))]
    pub malloc_pool: Mempool,
    #[pin]
    #[init(PerCpu::alloc(), ENOMEM_fs_other_alloc)]
    pub bufs: PerCpu<BtreeTransBuf>,

    #[pin]
    #[init(crate::new_srcu!("btree_trans_barrier"))]
    pub barrier: Srcu,

    #[pin]
    #[init(::pin_init::pin_init_array_from_fn(|_| BtreeTransactionStats::init()))]
    pub stats: [BtreeTransactionStats; TRANSACTIONS_NR as usize],
    /* each transaction function's, as stats[] */
    #[pin]
    #[init(TimeStatsArray::new())]
    pub duration: TimeStatsArray<TimeStats, { TRANSACTIONS_NR as usize }>,
    #[pin]
    #[init(TimeStatsArray::new())]
    pub lock_hold_times: TimeStatsArray<TimeStats, { TRANSACTIONS_NR as usize }>,
    #[pin]
    #[init(TimeStatsArray::new())]
    pub lock_wait_times: TimeStatsArray<TimeStats, { TRANSACTIONS_NR as usize }>,

    #[pin]
    #[init(new_lock!(TransStatsJson { buf: Printbuf::new() }, "&c->btree.trans.stats_json_lock"))]
    pub stats_json: Locked<Mutex, TransStatsJson>,
}

/// The transaction stats as JSON, for sysfs: made at a read's offset 0, read
/// a page at a time after.
#[repr(C)]
#[derive(MaybeZeroable)]
pub struct TransStatsJson {
    pub buf: Printbuf,
}

#[cfg(CONFIG_BCACHEFS_DEBUG)]
use crate::util::backtrace::{prt_backtrace, save_backtrace};
use crate::util::backtrace::{prt_symbol, prt_task_backtrace};
#[cfg(kernel)]
use kernel::sync::SetOnce;
#[cfg(not(kernel))]
use bcachefs_shim::sync::SetOnce;
use crate::os::kernel::{cond_resched, local_clock, prefetch, time_after64};
#[cfg(any(CONFIG_BCACHEFS_DEBUG, CONFIG_BCACHEFS_TRANS_KMALLOC_TRACE))]
use crate::util::log::LogMsg;
use crate::util::log::{log_no_fs, LogLevel};
use crate::os::{percpu::{Counters, CpuGuard}, sched};
use crate::util::{rcu, Printbuf, ToText};
use crate::util::refcount::{AtomicRef, RefGuard, TryRefcount, WaitableRef};
use crate::util::vstructs::{ValidatedEntries, ViewEntries};
use core::marker::PhantomData;
use core::mem::{offset_of, size_of, size_of_val};
use core::ops::ControlFlow;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

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

/// One btree_trans!() call site's index in TRANSACTION_FNS, once its name is
/// registered.
#[doc(hidden)]
pub struct TransFnName {
    idx:   AtomicU32,
    claim: AtomicBool,
}

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

        let idx = trans_get_fn_idx(name);
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

    /// begin(), for a loop that tracks restarts itself: the new
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
    pub(crate) unsafe fn updates_unguarded(&self) -> &[BtreeInsertEntry] {
        let (_, _, updates) = table_arrays(self.paths.get(), self.nr_paths.get().into());
        unsafe { core::slice::from_raw_parts(updates, self.nr_updates.get() as usize) }
    }

    /// Whether anything is queued for the next commit: key updates, journal
    /// entries or accounting.
    pub fn has_updates(&self) -> bool {
        self.nr_updates.get() != 0 ||
            self.journal_entries.get().u64s() != 0 ||
            self.accounting.get().u64s() != 0
    }

    /// Drop everything queued for the next commit, as every commit and
    /// begin does.
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
        let (allocated, _, _) = table_arrays(self.paths.get(), nr);
        let words = unsafe { core::slice::from_raw_parts(allocated.cast::<usize>(), bits_to_longs(nr)) };
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
        self.restart_ip(error, Self::restart as *const () as usize)
    }

    /// A panic, in debug builds, if the transaction's unlocked or in a
    /// restart: for what reads the nodes its paths hold.
    pub(crate) fn verify_not_unlocked_or_in_restart(&self) {
        if cfg!(CONFIG_BCACHEFS_DEBUG) {
            if self.restarted().is_some() || !self.locked() {
                self.unlocked_or_in_restart_error()
            }
        }
    }

    pub fn fs_usage_delta_add(&self, d: FsUsageBase) {
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

    /// commit_flush(), with commit()'s @lazy. journal_seq and flush are set
    /// every commit, as C set them: a commit before ours, in the same
    /// transaction, may have left a pointer to its stack there.
    pub(super) fn commit_inner<R>(
        &self,
        t:         &TransAttempt<'_, '_>,
        disk_res:  Option<&DiskReservation<'_>>,
        flush:     Option<&mut Option<Flush<'_>>>,
        flags:     CommitOpts,
        lazy:      bool,
        committed: impl FnOnce(&Paths<'_>) -> R,
    ) -> Result<R, BchError> {
        self.disk_res.set(disk_res.map_or(core::ptr::null_mut(), |r| r.as_mut_ptr()));
        self.journal_seq.set(core::ptr::null_mut());
        self.flush.set(flush.map_or(core::ptr::null_mut(), |f| core::ptr::from_mut(f).cast()));
        self.commit_trans(t, flags, lazy, committed)
    }
}

// ── The filesystem's transactions: c->btree.trans.list ──────────────────────

pub(crate) type TransList = crate::list!(BtreeTrans<'static>, list);

/// Another thread's transaction's flags: its own thread changes them -
/// READ_ONCE().
pub(crate) fn foreign_bits(trans: &BtreeTrans<'_>) -> BtreeTransBits {
    unsafe { (&raw const trans.bits).cast::<BtreeTransBits>().read_volatile() }
}

/// A transaction off the list, kept live - its paths, its task - by the ref.
pub struct ListedTrans {
    trans: *mut BtreeTrans<'static>,
    _ref:  RefGuard<AtomicRef<()>>,
}

impl ListedTrans {
    /// Whether it holds the SRCU lock btree nodes are freed under.
    pub fn srcu_held(&self) -> bool {
        foreign_bits(unsafe { &*self.trans }).srcu_held()
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
        unsafe { crate::btree::lock_graph::btree_trans_to_text(out, self.trans) }
    }
}

impl Fs {
    pub(crate) fn with_trans_list<R>(&self, f: impl FnOnce(&mut TransList) -> R) -> R {
        f(&mut self.btree.trans.list.lock())
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

fn trans_alloc(c: &Fs) -> NonNull<BtreeTrans<'_>> {
    let cached = {
        let cpu = CpuGuard::new();
        let slot = crate::percpu_field!(c.btree.trans.bufs.this_cpu(&cpu), trans);
        let trans = slot.get();
        slot.set(ptr::null_mut());
        trans
    };

    if let Some(trans) = NonNull::new(cached) {
        /* not ->c: it stays on the list, where walkers read it */
        let start = offset_of!(BtreeTrans<'_>, paths);
        unsafe { trans.cast::<u8>().add(start).write_bytes(0, offset_of!(BtreeTrans<'_>, list) - start) };
        return trans.cast();
    }

    let trans = {
        let _noio = MemallocFlags::noio();
        c.btree.trans.pool.alloc_raw()
    }.expect("a transaction outside NORECLAIM").cast::<BtreeTrans<'_>>();
    let p = trans.as_ptr();
    unsafe {
        p.write_bytes(0, 1);
        (&raw mut (*p).c).write(c);
        // stopped, as an exited one's is: on the list from here, it's got
        // from walkers only once it's begun
        (&raw mut (*p).ref_).write(AtomicRef::new(()));
        (*p).ref_.stop();
    }

    c.with_trans_list(|list| {
        if cfg!(CONFIG_BCACHEFS_DEBUG) {
            let pid = sched::current_pid();

            unsafe { (*p).locking_wait.task.store(sched::current(), Ordering::Relaxed) };

            for pos in list.iter() {
                let pos_task = pos.locking_wait.task.load(Ordering::Relaxed);
                /*
                 * We'd much prefer to be stricter here and completely
                 * disallow multiple btree_trans in the same thread -
                 * but the data move path calls write() when we
                 * already have a btree_trans initialized.
                 */
                assert!(!(!pos_task.is_null() &&
                          pid == unsafe { sched::task_pid(pos_task) } &&
                          foreign_bits(pos).locked()));
            }
        }

        unsafe { list.push_front(trans.cast().as_ref()) };
    });
    trans
}

/// A transaction, owned: got by new_fn(), put when dropped. btree_trans!()'s,
/// used as the
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
    /// For C, which holds it until it puts it.
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
        ebug_assert!(c.flags.get(FsFlag::may_go_rw) ||
                     c.flags.get(FsFlag::scrub_journal) ||
                     sched::current() == c.recovery_task.load(Ordering::Relaxed));

        let trans = TransGuard { trans: trans_alloc(c) };
        #[cfg(CONFIG_DEBUG_LOCK_ALLOC)]
        let _ = unsafe {
            crate::util::alloc::init_at(crate::new_lockdep_map!("bcachefs_btree"),
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

        let stats = (fn_idx < TRANSACTIONS_NR).then(|| &c.btree.trans.stats[fn_idx as usize]);

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
            !c.journal.flags.get(JournalFlag::replay_done) &&
            c.journal_keys_get().map(RefGuard::leak).is_some());
        trans_paths_init(t);

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

        t.ref_.restart();
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

/// A transaction's memory, out of its percpu cache: the pool's again a grace
/// period on.
struct PooledTrans(NonNull<BtreeTrans<'static>>);

// out of the percpu cache, the transaction's memory is the pool's alone
unsafe impl rcu::RcuOwned for PooledTrans {
    fn into_head(self) -> NonNull<rcu::RcuHead> {
        unsafe { NonNull::new_unchecked(&raw mut (*self.0.as_ptr()).rcu).cast() }
    }

    unsafe fn from_head(head: NonNull<rcu::RcuHead>) -> Self {
        PooledTrans(unsafe { head.byte_sub(offset_of!(BtreeTrans<'_>, rcu)).cast() })
    }
}

// memory, which any thread may give back to the pool
unsafe impl Send for PooledTrans {}

impl PooledTrans {
    fn free(self) {
        unsafe { (*self.0.as_ptr()).c.btree.trans.pool.free_raw(self.0.cast()) };
    }
}

impl Drop for TransGuard<'_> {
    fn drop(&mut self) {
        let t = &**self;
        let c = t.fs();

        if let Some(e) = t.restarted() {
            t.in_restart_error(e);
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
        t.ref_.stop();
        t.ref_.wait();
        t.locking_wait.task.store(ptr::null_mut(), Ordering::Relaxed);

        #[cfg(CONFIG_BCACHEFS_DEBUG)]
        drop(t.last_restarted_trace.take());
        #[cfg(CONFIG_BCACHEFS_TRANS_KMALLOC_TRACE)]
        drop(t.trans_kmalloc_trace.take());

        trans_paths_exit(t);

        t.mem.take().free(&c.btree.trans.malloc_pool);

        let trans = self.trans.as_ptr();

        let trans = {
            let cpu = CpuGuard::new();
            let slot = crate::percpu_field!(c.btree.trans.bufs.this_cpu(&cpu), trans);
            let old = slot.get();
            slot.set(trans.cast());
            old
        };

        if let Some(trans) = NonNull::new(trans) {
            cached_trans_retire(c, trans);
        }
    }
}

/// A transaction out of its percpu slot, done with: off the transaction list,
/// its memory the pool's again a grace period on.
pub(crate) fn cached_trans_retire(c: &Fs, trans: NonNull<BtreeTrans<'static>>) {
    c.with_trans_list(|list| unsafe { list.remove(trans.as_ref()) });

    /*
     * RCU-defer the mempool return: the cycle detector walks
     * wait_fifos under RCU without holding closure refs, so other
     * threads may be mid-access of this trans via a pointer they
     * read from a wait_fifo slot. Grace period ensures the memory
     * isn't reused for a different struct before those walks
     * finish; per-CPU cache reuse within a grace period is
     * identity-confusion (acceptable per probabilistic detection).
     */
    rcu::Flavor::RCU.call(PooledTrans(trans), PooledTrans::free);
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
        self.buf = unsafe { Vec::from_raw_parts(p.as_ptr().cast(), 0, BTREE_TRANS_MEM_MAX as usize / size_of::<u64>()) };
        self.used_mempool = true;
    }

    /// Its buffer freed: given back to the pool, if it's one of its elements.
    fn free(self, pool: &Mempool) {
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
pub(crate) fn kmalloc_trace_to_text(out: &mut Printbuf, trace: &[TransKmallocTrace]) {
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

    pub(crate) fn maybe_inject_restart(&self, ip: usize) -> Result<(), BchError> {
        #[cfg(CONFIG_BCACHEFS_INJECT_TRANSACTION_RESTARTS)]
        if crate::os::kernel::ktime_get_ns() & !(!0u64 << (10 + self.restart_count_this_trans.get()).min(63)) == 0 {
            self.fs().event_inc_trace(events::trans_restart_injected, |out| prt!(out, "{}", self.fn_name()));
            return Err(self.restart_ip(Errcode::transaction_restart_fault_inject, ip));
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
    pub(crate) fn kmalloc_grow(&self, size: usize, ret_ip: usize) -> BchError {
        let c = self.fs();
        let (top, old_bytes) = self.mem_with(|m| (m.top_bytes(), m.bytes()));
        let new_top = (top + size) as u32;
        let new_bytes = new_top.next_power_of_two();

        if crate::warn_on_once!(new_bytes > BTREE_TRANS_MEM_MAX) {
            #[cfg(CONFIG_BCACHEFS_TRANS_KMALLOC_TRACE)]
            {
                let mut msg = LogMsg::new(c);
                prt!(&mut *msg, "bump allocator exceeded BTREE_TRANS_MEM_MAX ({})\n", BTREE_TRANS_MEM_MAX);

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

        if self.mem_with(|m| m.used_mempool) || new_bytes > BTREE_TRANS_MEM_MAX {
            ebug_assert!(old_bytes < new_bytes as usize);
            return Errcode::ENOMEM_trans_kmalloc.into();
        }

        self.mem_with(|m| m.realloc_bytes_required = new_bytes);
        c.event_inc_trace(events::trans_restart_mem_realloced, |out| {
            prt!(out, "{}\nbytes {}\n", self.fn_name(), new_bytes);
        });
        self.restart_ip(Errcode::transaction_restart_mem_realloced, ret_ip)
    }

    /**
     * bch2_trans_begin() - reset a transaction after a interrupted attempt
     *
     * Returns:	current restart counter, to be used with trans_was_restarted()
     *
     * While iterating over nodes or updating nodes a attempt to lock a btree node
     * may return transaction_restart when the trylock fails. When this
     * occurs bch2_trans_begin() should be called and the transaction retried.
     */
    fn begin_from(&self, paths: &mut Paths<'_>, ret_ip: usize) -> u32 {
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
                             self.restarted().is_none() && !self.in_traverse_all() &&
                             !self.begin_may_drop_updates());

        self.reset_updates(paths);

        // a generation, compared for equality: it wraps, as C's u32 did
        self.restart_count.set(self.restart_count.get().wrapping_add(1));
        self.mem_with(|m| m.buf.clear());

        /*
         * Long-lived trans (e.g. copygc) initialized before journal replay
         * finished hold a journal_keys ref and stay on the journal-overlay
         * iter path forever, reading stale post-replay journal_keys data.
         * Drop the ref + clear the flag once replay completes; subsequent
         * trans_begin calls see the flag already clear and skip cheaply.
         */
        if self.journal_replay_not_finished() &&
           c.journal.flags.get(JournalFlag::replay_done) {
            drop(unsafe { RefGuard::adopt(&c.journal_keys) });
            self.set_journal_replay_not_finished(false);
        }

        if self.restarted() == Some(Errcode::transaction_restart_mem_realloced.into()) {
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
                if self.restarted().is_none() && path.btree_id() != Btree::subvolumes {
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
            c.btree.trans.duration.get(self.fn_idx.get() as usize).update_at(self.last_begin_time.get(), now);
        }

        self.srcu_begin(paths, ret_ip);

        if self.restarted().is_none() {
            self.locking_wait.trans_start_time.set(now);
        }
        self.last_begin_time.set(now);

        self.last_begin_ip.set(ret_ip);

        #[cfg(CONFIG_BCACHEFS_INJECT_TRANSACTION_RESTARTS)]
        if self.restarted().is_some() {
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

        if self.restarted().is_some() {
            /* restart is hot — skip the resched check */
            let _ = self.path_traverse_all(paths);
            self.clear_notrace_relock_fail();
        } else if sched::need_resched() ||
                  time_after64(now, self.locking_wait.trans_start_time.get() +
                                    BTREE_TRANS_MAX_LOCK_HOLD_TIME_NS as u64) {
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
        ebug_assert!(new_bytes <= BTREE_TRANS_MEM_MAX);

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
            let old: BkeySC<'_> = BkeySC { k: &i.old_k, v: i.old_v, iter: PhantomData, val: PhantomData };

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

        self.journal_entries.get().with(self, |s| {
            for e in ValidatedEntries::of(s) {
                journal_entry_to_text(&mut out, Some(c), e.as_unvalidated());
                prt!(out, "\n");
            }
        });

        self.accounting.get().with(self, |s| {
            for k in ViewEntries::of(s) {
                bkey_val_to_text(&mut out, Some(c), BkeySC::from(k));
                prt!(out, "\n");
            }
        });
    }
}

// ── Restarts ────────────────────────────────────────────────────────────────

impl BtreeTrans<'_> {
    /// restart_ip(), less the backtrace: for a transaction this thread may not
    /// be running, whose restart this thread's backtrace wouldn't explain.
    pub(crate) fn restart_foreign_task(&self, err: Errcode, ip: usize) -> BchError {
        assert!(BchError::from(err).matches(Errcode::transaction_restart));

        self.set_restarted(Some(err.into()));
        self.last_restarted_ip.set(ip);
        err.into()
    }

    pub(crate) fn restart_ip(&self, err: Errcode, ip: usize) -> BchError {
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
    fn in_restart_error(&self, err: BchError) -> ! {
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
        if let Some(e) = self.restarted() {
            self.in_restart_error(e);
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

/// Each btree_trans!() call site's name, by fn_idx: the transaction's, and its
/// stats'. Slot 0 is never registered: it's "(unknown)".
static TRANSACTION_FNS: [SetOnce<&'static str>; TRANSACTIONS_NR as usize] =
    [const { SetOnce::new() }; TRANSACTIONS_NR as usize];

/// The name registered at @idx, if one is.
pub fn transaction_fn(idx: usize) -> Option<&'static str> {
    match idx {
        0 => Some("(unknown)"),
        _ => TRANSACTION_FNS.get(idx)?.as_ref().copied(),
    }
}

/// The name registered at @idx, for messages.
pub fn transaction_fn_name(idx: u8) -> &'static str {
    transaction_fn(idx.into()).unwrap_or("?")
}

/// The names registered, by index, up to the first slot that isn't.
pub fn transaction_fns() -> impl Iterator<Item = &'static str> {
    (0..TRANSACTION_FNS.len()).map_while(transaction_fn)
}

/// @name in the first free slot, claimed - so call sites registering at once
/// get one each: its index, or 0 if the table's full.
fn trans_get_fn_idx(name: &'static str) -> u32 {
    if let Some(i) = (1..TRANSACTION_FNS.len()).find(|&i| TRANSACTION_FNS[i].populate(name)) {
        return i as u32;
    }

    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        log_no_fs(LogLevel::WARN, crate::message!("TRANSACTIONS_NR not big enough!"));
    }
    0
}

// ── C's entry points ────────────────────────────────────────────────────────

pub fn __bch2_trans_get(c: &Fs, fn_idx: u32) -> *mut BtreeTrans<'static> {
    TransGuard::new_fn(c, fn_idx).into_raw().cast()
}

