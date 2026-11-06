// SPDX-License-Identifier: GPL-2.0

//! The transaction: btree_trans!(), the handle Rust gets one by, its own
//! methods, and the filesystem's list of them.

pub use super::types::BtreeTrans;
use super::iter::{CommitOpts, TransAttempt, UpdateTriggerFlags};
use crate::alloc::capacity::DiskReservation;
use crate::btree::bkey::{bkey_val_to_text, BkeySC};
use crate::journal::validate::journal_entry_to_text;
use crate::util::vstructs::{from_ffi, Head, SelfSized, Unvalidated};
use core::marker::PhantomData;
use crate::btree::locking::{bch2_trans_srcu_begin, bch2_trans_srcu_lock, bch2_trans_unlock_long};
use crate::btree::path::{bch2_trans_paths_exit, bch2_trans_paths_init, BtreePath, PathIdx, Paths};
use crate::c;
use crate::debug::trace::events;
use crate::errcode::{BchError, bch_errcode, err_ptr, ret_to_result_void as ret_to_result};
use crate::format::{Bpos, Btree};
use crate::fs::Fs;
use crate::locking::{Locked, MemallocFlags};
use crate::prt;
#[cfg(any(CONFIG_BCACHEFS_DEBUG, CONFIG_BCACHEFS_TRANS_KMALLOC_TRACE))]
use crate::util::alloc::darray_exit;
use crate::util::alloc::{kmalloc, krealloc};
#[cfg(CONFIG_BCACHEFS_DEBUG)]
use crate::util::backtrace::{prt_backtrace, save_backtrace};
use crate::util::backtrace::{prt_symbol, prt_task_backtrace};
use crate::util::closure::ClosureRef;
use crate::util::ffi::{c_char, Opaque};
use crate::util::kernel::{cond_resched, local_clock, prefetch, time_after64};
#[cfg(any(CONFIG_BCACHEFS_DEBUG, CONFIG_BCACHEFS_TRANS_KMALLOC_TRACE))]
use crate::util::log::LogMsg;
use crate::util::time_stats::__bch2_time_stats_update;
use crate::util::log::CFnName;
#[cfg(kernel)]
use crate::util::percpu::CpuGuard;
use crate::util::percpu::{bch2_task_pid, Counters};
use crate::util::rcu;
use crate::util::refcount::{RefGuard, TryRefcount};
use crate::util::sched;
use crate::util::{Printbuf, ToText};
use crate::data_structures::bitmap::Bitmap;
use crate::types::bits_to_longs;
use core::ffi::{c_int, c_uint, c_ulong, c_void};
use core::mem::{offset_of, size_of_val};
use core::ops::ControlFlow;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};
use cstruct_macros::{c_export, c_extern};

/// btree_trans!(fs): a transaction named for the function it's created in.
/// Transaction stats and lock contention are reported per function, under that
/// name; without it a transaction is "(unknown)".
///
/// XXX: C registers __func__ - a NUL-terminated string that lives forever - so
/// each call site carries a CFnName to copy its name into. Clean this up once
/// enough is converted to key the stats on Rust's own names.
#[macro_export]
macro_rules! btree_trans {
    ($fs:expr) => {{
        static FN: $crate::btree::trans::TransFnName = $crate::btree::trans::TransFnName::new();
        $crate::btree::trans::TransGuard::new_fn($fs, FN.idx($crate::function_name!()))
    }};
}

/// One btree_trans!() call site's registered name.
#[doc(hidden)]
pub struct TransFnName {
    idx:   AtomicU32,
    claim: AtomicBool,
    name:  CFnName,
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
            name:  CFnName::new(),
        }
    }

    /// This call site's index in TRANSACTION_FNS, registering
    /// @name on first use. 0 - "(unknown)" - while another thread is
    /// registering it, or if the table is full.
    pub fn idx(&self, name: &str) -> u32 {
        let idx = self.idx.load(Ordering::Acquire);
        if idx != 0 || self.claim.swap(true, Ordering::AcqRel) {
            return idx;
        }

        let idx = bch2_trans_get_fn_idx(self.name.get(name).as_ptr().cast());
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
        self.journal_res.get().seq
    }

    /// The function that got it, for messages: trans->fn.
    pub fn fn_name(&self) -> &str {
        // SAFETY: a string literal, set when it's got
        unsafe { core::ffi::CStr::from_ptr(self.fn_.cast()) }.to_str().unwrap_or("?")
    }

    /// Journal space for keys a trigger adds in the commit's atomic phase,
    /// through the write buffer, reserved with the transaction's own.
    pub fn reserve_extra_journal_u64s(&self, u64s: u32) {
        self.extra_journal_u64s.set(self.extra_journal_u64s.get() + u64s)
    }

    /// Delete [@start, @end) in @btree, committing as it goes. It drives its
    /// own begin and commit loop, so it's on the transaction, not an attempt:
    /// transaction_restart_nested if it restarted along the way, for a caller
    /// in an attempt to begin again.
    pub fn delete_range(
        &self,
        btree: Btree,
        start: Bpos,
        end:   Bpos,
        flags: UpdateTriggerFlags,
    ) -> Result<(), BchError> {
        crate::errcode::ret_to_result(unsafe {
            c::bch2_btree_delete_range_trans(self.raw(), btree, start, end,
                                             c::btree_iter_update_trigger_flags::from_bits_retain(flags.bits()))
        }).map(|_| ())
    }

    /// Flush the btree write buffer, through to the journal sequence number
    /// being written. It leaves the transaction unlocked - the next btree
    /// access has to begin it again.
    pub fn write_buffer_flush_sync(&self) -> Result<(), BchError> {
        crate::errcode::ret_to_result(unsafe {
            c::bch2_btree_write_buffer_flush_sync(self.raw())
        }).map(|_| ())
    }

    /// Flush what the btree write buffer holds, if anything - not waiting on
    /// a flush already running, and erofs_no_writes if the filesystem's
    /// going read-only. It may leave the transaction unlocked.
    pub fn write_buffer_tryflush(&self) -> Result<(), BchError> {
        crate::errcode::ret_to_result(unsafe {
            c::bch2_btree_write_buffer_tryflush(self.raw())
        }).map(|_| ())
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
    /// restart count.
    pub(crate) fn begin_raw(&self) -> u32 {
        bch2_trans_begin(self)
    }

    pub fn begin<'a>(&'a self) -> TransAttempt<'a, 'f> {
        TransAttempt::new(self, self.begin_raw())
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
    pub(crate) fn reset_updates(&self) {
        for i in 0..self.nr_updates.get() as usize {
            unsafe { c::bch2_path_put(self.raw(), (*self.updates.get().add(i)).path, true) };
        }

        let emptied = |b: c::btree_trans_subbuf| c::btree_trans_subbuf { u64s: 0, size: 0, ..b };
        self.nr_updates.set(0);
        self.journal_entries.set(emptied(self.journal_entries.get()));
        self.accounting.set(emptied(self.accounting.get()));
        self.hooks.set(core::ptr::null_mut());
        self.extra_disk_res.set(0);
        self.extra_disk_res_replicas.set(0);
        self.extra_journal_u64s.set(0);
        self.set_has_interior_updates(false);
    }

    pub fn verify_not_restarted(&self, restart_count: u32) {
        if self.restart_count.get() != restart_count {
            bch2_trans_restart_error(self, restart_count);
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

    fn paths_allocated(&self) -> (&Bitmap, usize) {
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

    /// TransAttempt::restart(), for what C calls in the middle of an attempt
    /// without one: the lock paths.
    pub(crate) fn restart(&self, error: bch_errcode) -> BchError {
        let ip = Self::restart as *const () as c_ulong;
        BchError::from_raw(-bch2_trans_restart_ip(self, error.0 as c_int, ip))
    }

    /// A panic, in debug builds, if the transaction's unlocked or in a
    /// restart: for what reads the nodes its paths hold.
    pub(crate) fn verify_not_unlocked_or_in_restart(&self) {
        if cfg!(CONFIG_BCACHEFS_DEBUG) {
            if self.restarted().0 != 0 || !self.locked() {
                bch2_trans_unlocked_or_in_restart_error(self)
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
            unsafe { c::__bch2_assert_pos_locked(self.raw(), btree, pos) }
        }
    }

    pub fn commit(
        &self,
        disk_res: Option<&DiskReservation<'_>>,
        flags: impl Into<CommitOpts>,
    ) -> Result<(), BchError> {
        self.commit_inner(disk_res, None, flags.into(), false)
    }

    /// The static inline bch2_trans_commit_flush(), with
    /// __bch2_trans_commit()'s @lazy. journal_seq and flush are set, as C's
    /// every commit does: a C commit before ours, in the same transaction, may
    /// have left a pointer to its stack there.
    pub(super) fn commit_inner(
        &self,
        disk_res: Option<&DiskReservation<'_>>,
        flush:    Option<&Opaque<c::closure>>,
        flags:    CommitOpts,
        lazy:     bool,
    ) -> Result<(), BchError> {
        self.disk_res.set(disk_res.map_or(core::ptr::null_mut(), |r| r.as_mut_ptr()));
        self.journal_seq.set(core::ptr::null_mut());
        self.flush.set(flush.map_or(core::ptr::null_mut(), Opaque::as_ptr));
        let ret = unsafe { c::__bch2_trans_commit(self.raw(), flags.to_c(), lazy) };
        ret_to_result(ret)
    }
}

// ── The filesystem's transactions: c->btree.trans.list ──────────────────────

type TransList = crate::list!(BtreeTrans<'static>, list);

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
    /// Each transaction above @cursor (an address) that @pred takes, in
    /// address order, given to @f with the list unlocked - @f's to drop before
    /// anything that might wait. Printing sleeps, so the list's seqmutex is
    /// dropped for @f and relocked only if no one took it, else the walk
    /// starts over: sorted by address, it resumes from @cursor.
    pub fn for_each_trans_from<E>(
        &self,
        cursor: &mut usize,
        pred:   impl Fn(&ListedTrans) -> bool,
        mut f:  impl FnMut(ListedTrans) -> Result<ControlFlow<()>, E>,
    ) -> Result<(), E> {
        let lock = Locked::from_c(&self.btree.trans.lock);
        let head = (&raw const self.btree.trans.list).cast_mut();

        // SAFETY, throughout: the list, under its lock
        'restart: loop {
            let mut guard = lock.lock();
            unsafe { TransList::from_c(head) }
                .sort(|a, b| core::ptr::from_ref(a).cmp(&core::ptr::from_ref(b)));

            let mut pos = unsafe { (*head).next };
            while pos != head {
                let trans: *mut BtreeTrans<'_> =
                    unsafe { pos.byte_sub(core::mem::offset_of!(BtreeTrans<'_>, list)) }.cast();

                if trans as usize > *cursor {
                    *cursor = trans as usize;

                    let t = unsafe { &(*trans).ref_ }.tryget()
                        .map(|r| ListedTrans { trans, _ref: r })
                        .filter(|t| pred(t));

                    if let Some(t) = t {
                        let seq = guard.unlock();
                        if f(t)?.is_break() {
                            return Ok(());
                        }
                        match lock.relock(seq) {
                            Some(g) => guard = g,
                            None    => continue 'restart,
                        }
                    }
                }
                pos = unsafe { (*pos).next };
            }
            return Ok(());
        }
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

    let trans = c.btree.trans.pool.alloc_raw(c::GFP_NOIO).cast::<BtreeTrans<'_>>();
    let p = trans.as_ptr();
    unsafe {
        p.write_bytes(0, 1);
        (&raw mut (*p).c).write(c);
    }

    let lock = Locked::from_c(&c.btree.trans.lock);
    let _guard = lock.lock();
    let list = unsafe { TransList::from_c((&raw const c.btree.trans.list).cast_mut()) };

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
        // SAFETY: ours until dropped
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
}

#[c_export]
pub fn __bch2_trans_get(c: &Fs, fn_idx: c_uint) -> *mut BtreeTrans<'static> {
    TransGuard::new_fn(c, fn_idx).into_raw().cast()
}

/// # Safety
/// @trans is __bch2_trans_get()'s, and isn't used after.
#[c_export]
pub unsafe fn bch2_trans_put(trans: *mut BtreeTrans<'_>) {
    drop(unsafe { TransGuard::from_raw(trans) })
}

impl<'f> TransGuard<'f> {
    /// Use btree_trans!(), which names the transaction for its caller.
    #[doc(hidden)]
    pub fn new_fn(c: &'f Fs, fn_idx: u32) -> Self {
        /*
         * No multithreaded btree access until we've gone RW and are no longer
         * modifying the journal keys gap buffer
         */
        ebug_assert!(c.flags.get(c::bch_fs_flags::BCH_FS_may_go_rw) ||
                     c.flags.get(c::bch_fs_flags::BCH_FS_scrub_journal) ||
                     sched::current() == c.recovery_task.load(Ordering::Relaxed));

        let trans = bch2_trans_alloc(c);
        let p = trans.as_ptr();

        /*
         * Warm the known btrees' root pointer cache lines: every btree access in
         * this trans goes through c->btree.cache.roots_b[], and the cold-line miss
         * shows up as ~3-7% of cycles in bch2_btree_path_traverse_one's
         * lock_root path. Issuing the prefetches here lets them complete
         * during the rest of trans_get's setup work.
         */
        let roots_b = &c.btree.cache.roots.roots_b;
        let roots = roots_b.as_ptr().cast::<u8>();
        for i in (0..c::BTREE_ID_NR as usize * size_of_val(&roots_b[0])).step_by(64) {
            prefetch(roots.wrapping_add(i));
        }

        let stats = (fn_idx < c::BCH_TRANSACTIONS_NR).then(|| &c.btree.trans.stats[fn_idx as usize]);

        unsafe {
            (*p).fn_idx = fn_idx as u8;
            if let Some(s) = stats {
                (*p).fn_ = TRANSACTION_FNS[fn_idx as usize].load(Ordering::Relaxed);
                (*p).nr_paths_max = s.nr_max_paths as _;
            }
            #[cfg(CONFIG_DEBUG_LOCK_ALLOC)]
            let _ = ::pin_init::PinInit::__pinned_init(crate::new_lockdep_map!("bcachefs_btree"), &raw mut (*p).dep_map);
        }

        let t = unsafe { trans.as_ref() };

        t.shard_cpu.set(if c.opts().shard_inode_numbers_bits.get() != 0 {
            c.inode_shard_cpu[c.inode_shard_idx() as usize] as i16
        } else {
            -1
        });
        t.locking_wait.task.store(sched::current(), Ordering::Relaxed);
        t.set_journal_replay_not_finished(
            !c.journal.flags.get(c::journal_flags::JOURNAL_replay_done) &&
            c.journal_keys_get().map(RefGuard::leak).is_some());
        bch2_trans_paths_init(t);

        if let Some(s) = stats {
            if s.max_mem != 0 {
                let expected_mem_bytes = s.max_mem.next_power_of_two();

                let _nowarn = MemallocFlags::nowarn();
                if let Ok(mem) = kmalloc(expected_mem_bytes as usize) {
                    t.mem.set(mem.as_ptr().cast());
                    t.mem_bytes.set(expected_mem_bytes);
                }
            }
        }

        bch2_trans_srcu_lock(t);
        t.mark_locked(false);

        let now = local_clock();
        t.locking_wait.trans_start_time.set(now);
        t.last_begin_time.set(now);

        unsafe { c::closure_init_stack_release(t.ref_.as_ptr()) };
        TransGuard { trans }
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
            bch2_trans_in_restart_error(t);
        }

        {
            let mut paths = t.paths();

            bch2_trans_unlock_long(t, &mut paths);

            let updates = unsafe { core::slice::from_raw_parts(t.updates.get(), t.nr_updates.get() as usize) };
            for i in updates {
                paths.put_ll(i.path, true);
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
        unsafe { darray_exit(&mut *t.last_restarted_trace.as_ptr()) };
        #[cfg(CONFIG_BCACHEFS_TRANS_KMALLOC_TRACE)]
        unsafe { darray_exit(&mut *t.trans_kmalloc_trace.as_ptr()) };

        bch2_trans_paths_exit(t);

        if let Some(mem) = NonNull::new(t.mem.get()) {
            if t.used_mempool() {
                unsafe { c.btree.trans.malloc_pool.free_raw(mem.cast()) };
            } else {
                unsafe { c::rust_kfree(mem.as_ptr()) };
            }
        }

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
            let lock = Locked::from_c(&c.btree.trans.lock);
            let guard = lock.lock();
            unsafe { TransList::from_c((&raw const c.btree.trans.list).cast_mut()).remove(&*trans.cast()) };
            drop(guard);

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

c_extern! {
    pub fn __bch2_path_free(trans: *mut BtreeTrans<'_>, path: PathIdx);
    pub fn bch2_btree_path_traverse_all(trans: *mut BtreeTrans<'_>) -> c_int;
}

/// @alloc in a NORECLAIM scope, and if it fails, again with the transaction
/// unlocked - and left unlocked: allocate_dropping_locks_norelock().
fn allocate_dropping_locks_norelock<T>(trans: &BtreeTrans<'_>, mut alloc: impl FnMut() -> Option<T>) -> Option<T> {
    let first = {
        let _noreclaim = MemallocFlags::noreclaim();
        alloc()
    };
    first.or_else(|| {
        trans.unlock();
        alloc()
    })
}

fn trans_maybe_inject_restart(trans: &BtreeTrans<'_>, ip: c_ulong) -> Result<(), BchError> {
    #[cfg(CONFIG_BCACHEFS_INJECT_TRANSACTION_RESTARTS)]
    if crate::util::kernel::ktime_get_ns() & !(!0u64 << (10 + trans.restart_count_this_trans.get()).min(63)) == 0 {
        trans.fs().event_inc_trace(events::trans_restart_injected, |out| prt!(out, "{}", trans.fn_name()));
        return Err(BchError::from_raw(-bch2_trans_restart_ip(
            trans, bch_errcode::BCH_ERR_transaction_restart_fault_inject.0 as c_int, ip)));
    }
    #[cfg(not(CONFIG_BCACHEFS_INJECT_TRANSACTION_RESTARTS))]
    let _ = (trans, ip);
    Ok(())
}

#[cfg(CONFIG_BCACHEFS_TRANS_KMALLOC_TRACE)]
#[cold]
pub(crate) fn bch2_trans_kmalloc_trace_to_text(out: &mut Printbuf, trace: &c::darray_trans_kmalloc_trace) {
    out.tabstops_reset();
    out.tabstop_push(60);

    for i in trace.as_slice() {
        prt_symbol(out, i.ip);
        prt!(out, "\t{}\n", i.bytes);
    }
}

#[c_export]
pub fn __bch2_trans_kmalloc(trans: &BtreeTrans<'_>, size: usize, ip: c_ulong) -> *mut c_void {
    let _ = ip;
    trans_kmalloc_grow(trans, size, crate::ret_ip!()).unwrap_or_else(err_ptr)
}

/// @size bytes of the transaction's memory, when they don't fit in what it
/// has: grown, or the transaction restarted to grow it.
pub(crate) fn trans_kmalloc_grow(trans: &BtreeTrans<'_>, size: usize, ret_ip: c_ulong) -> Result<*mut c_void, BchError> {
    let c = trans.fs();
    let new_top = trans.mem_top.get() + size as c_uint;
    let old_bytes = trans.mem_bytes.get();
    let mut new_bytes = new_top.next_power_of_two();

    if crate::warn_on_once!(new_bytes > c::BTREE_TRANS_MEM_MAX) {
        #[cfg(CONFIG_BCACHEFS_TRANS_KMALLOC_TRACE)]
        {
            let mut msg = LogMsg::new(c);
            prt!(&mut *msg, "bump allocator exceeded BTREE_TRANS_MEM_MAX ({})\n", c::BTREE_TRANS_MEM_MAX);

            bch2_trans_kmalloc_trace_to_text(&mut msg, unsafe { &*trans.trans_kmalloc_trace.as_ptr() });
        }
    }

    trans_maybe_inject_restart(trans, ret_ip)?;

    let s = &c.btree.trans.stats[trans.fn_idx as usize];
    if new_bytes > s.max_mem {
        let _lock = Locked::from_c(&s.lock).lock();
        // these are the stats' lock's
        let s = ptr::from_ref(s).cast_mut();
        #[cfg(CONFIG_BCACHEFS_TRANS_KMALLOC_TRACE)]
        {
            let trace = unsafe { &*trans.trans_kmalloc_trace.as_ptr() }.as_slice();
            let mut v = unsafe { crate::util::alloc::DArrayLoan::new(&mut (*s).trans_kmalloc_trace) };
            v.clear();
            let _ = v.reserve(trace.len());
            let nr = v.capacity().min(trace.len());
            let _ = v.extend_from_slice(&trace[..nr]);
        }
        unsafe { (*s).max_mem = new_bytes };
    }

    if trans.used_mempool() || new_bytes > c::BTREE_TRANS_MEM_MAX {
        ebug_assert!(trans.mem_bytes.get() < new_bytes);
        return Err(bch_errcode::BCH_ERR_ENOMEM_trans_kmalloc.into());
    }

    if old_bytes != 0 {
        trans.realloc_bytes_required.set(new_bytes);
        c.event_inc_trace(events::trans_restart_mem_realloced, |out| {
            prt!(out, "{}\nbytes {}\n", trans.fn_name(), new_bytes);
        });
        return Err(BchError::from_raw(-bch2_trans_restart_ip(
            trans, bch_errcode::BCH_ERR_transaction_restart_mem_realloced.0 as c_int, ret_ip)));
    }

    ebug_assert!(trans.mem.get().is_null());
    ebug_assert!(trans.mem_bytes.get() == 0);
    ebug_assert!(trans.mem_top.get() == 0);
    ebug_assert!(new_bytes <= c::BTREE_TRANS_MEM_MAX);

    let new_mem = allocate_dropping_locks_norelock(trans, || {
        let _nowarn = MemallocFlags::nowarn();
        kmalloc(new_bytes as usize).ok()
    }).unwrap_or_else(|| {
        new_bytes = c::BTREE_TRANS_MEM_MAX;
        trans.set_used_mempool(true);
        c.btree.trans.malloc_pool.alloc_raw(c::GFP_KERNEL)
    });

    trans.mem.set(new_mem.as_ptr().cast());
    trans.mem_bytes.set(new_bytes);

    trans.relock()?;

    let p = trans.mem.get();
    trans.mem_top.set(trans.mem_top.get() + size as c_uint);
    unsafe { p.cast::<u8>().write_bytes(0, size) };
    Ok(p)
}

/**
 * bch2_trans_begin() - reset a transaction after a interrupted attempt
 * @trans: transaction to reset
 *
 * Returns:	current restart counter, to be used with trans_was_restarted()
 *
 * While iterating over nodes or updating nodes a attempt to lock a btree node
 * may return BCH_ERR_transaction_restart when the trylock fails. When this
 * occurs bch2_trans_begin() should be called and the transaction retried.
 */
#[c_export]
pub fn bch2_trans_begin(trans: &BtreeTrans<'_>) -> u32 {
    let ret_ip = crate::ret_ip!();
    let c = trans.fs();

    c.event_inc_trace(events::transaction_begin, |out| {
        let (allocated, nr) = trans.paths_allocated();
        prt!(out, "{}\nnr_paths {}/{}\n", trans.fn_name(), allocated.weight(), nr);
    });

    /*
     * Discarding queued updates is correct after a transaction restart
     * (the restart loop re-queues them) and on error unwind — but a new
     * iteration starting with updates queued and no restart in flight
     * means the previous iteration forgot to commit them, and they're
     * about to be lost silently:
     */
    crate::warn_on_once!(cfg!(CONFIG_BCACHEFS_DEBUG) &&
                         trans.has_updates() &&
                         trans.restarted().0 == 0 && !trans.in_traverse_all() &&
                         !trans.begin_may_drop_updates());

    trans.reset_updates();

    trans.restart_count.set(trans.restart_count.get() + 1);
    trans.mem_top.set(0);

    /*
     * Long-lived trans (e.g. copygc) initialized before journal replay
     * finished hold a journal_keys ref and stay on the journal-overlay
     * iter path forever, reading stale post-replay journal_keys data.
     * Drop the ref + clear the flag once replay completes; subsequent
     * trans_begin calls see the flag already clear and skip cheaply.
     */
    if trans.journal_replay_not_finished() &&
       c.journal.flags.get(c::journal_flags::JOURNAL_replay_done) {
        drop(unsafe { RefGuard::adopt(&c.journal_keys) });
        trans.set_journal_replay_not_finished(false);
    }

    if trans.restarted() == bch_errcode::BCH_ERR_transaction_restart_mem_realloced {
        let mut new_bytes = trans.realloc_bytes_required.get();
        ebug_assert!(new_bytes <= c::BTREE_TRANS_MEM_MAX);
        ebug_assert!(!trans.mem.get().is_null());
        ebug_assert!(trans.mem_bytes.get() != 0);

        let new_mem = allocate_dropping_locks_norelock(trans, || {
            unsafe { krealloc(trans.mem.get().cast(), new_bytes as usize) }.ok()
        }).unwrap_or_else(|| {
            new_bytes = c::BTREE_TRANS_MEM_MAX;
            trans.set_used_mempool(true);
            let p = c.btree.trans.malloc_pool.alloc_raw(c::GFP_KERNEL);
            unsafe { c::rust_kfree(trans.mem.get()) };
            p
        });

        trans.mem.set(new_mem.as_ptr().cast());
        trans.mem_bytes.set(new_bytes);
    }

    for i in trans.path_idxs() {
        let free = {
            let mut paths = trans.paths();
            let path = &mut paths[i];

            path.set_should_be_locked(false);

            /*
             * If the transaction wasn't restarted, we're presuming to be
             * doing something new: dont keep iterators excpt the ones that
             * are in use - except for the subvolumes btree:
             */
            if trans.restarted().0 == 0 && path.btree_id() != Btree::subvolumes {
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
            unsafe { __bch2_path_free(trans.raw(), i) };
        }
    }

    let now = local_clock();

    if !cfg!(CONFIG_BCACHEFS_NO_LATENCY_ACCT) &&
       time_after64(now, trans.last_begin_time.get() + 10) {
        __bch2_time_stats_update(&c.btree.trans.stats[trans.fn_idx as usize].duration,
                                 trans.last_begin_time.get(), now);
    }

    bch2_trans_srcu_begin(trans, &mut trans.paths());

    if trans.restarted().0 == 0 {
        trans.locking_wait.trans_start_time.set(now);
    }
    trans.last_begin_time.set(now);

    trans.last_begin_ip.set(ret_ip);

    #[cfg(CONFIG_BCACHEFS_INJECT_TRANSACTION_RESTARTS)]
    if trans.restarted().0 != 0 {
        trans.restart_count_this_trans.set(trans.restart_count_this_trans.get() + 1);
    } else {
        trans.restart_count_this_trans.set(0);
    }

    #[cfg(CONFIG_BCACHEFS_TRANS_KMALLOC_TRACE)]
    unsafe { (*trans.trans_kmalloc_trace.as_ptr()).nr = 0 };

    if trans.restarted().0 != 0 {
        /* restart is hot — skip the resched check */
        unsafe { bch2_btree_path_traverse_all(trans.raw()) };
        trans.clear_notrace_relock_fail();
    } else if sched::need_resched() ||
              time_after64(now, trans.locking_wait.trans_start_time.get() +
                                c::BTREE_TRANS_MAX_LOCK_HOLD_TIME_NS as u64) {
        trans.unlock();
        cond_resched();
    }

    trans.mark_locked(false);

    trans.verify_not_unlocked_or_in_restart();
    trans.restart_count.get()
}

#[c_export]
#[cold]
#[inline(never)]
pub fn bch2_trans_updates_to_text(out: &mut Printbuf, trans: &BtreeTrans<'_>) {
    let c = trans.fs();

    prt!(out, "{} transaction updates for {} journal seq {}\n",
         trans.nr_updates.get(), trans.fn_name(), trans.journal_res.get().seq);
    let mut out = out.indent(2);

    let updates = unsafe { core::slice::from_raw_parts(trans.updates.get(), trans.nr_updates.get() as usize) };
    for i in updates {
        let old = BkeySC { k: &i.old_k, v: i.old_v, iter: PhantomData, val: PhantomData };
        let new = unsafe { BkeySC { k: &(*i.k).k, v: &(*i.k).v, iter: PhantomData, val: PhantomData } };

        prt!(out, "update: btree={} cached={} ", i.bits.btree_id(), u32::from(i.bits.cached()));
        prt_symbol(&mut out, i.ip_allocated);
        prt!(out, "\n");

        let mut out = out.indent(2);

        prt!(out, "old ");
        bkey_val_to_text(&mut out, Some(c), old);
        prt!(out, "\n");

        prt!(out, "new ");
        bkey_val_to_text(&mut out, Some(c), new);
        prt!(out, "\n");
    }

    let subbuf = |buf: c::btree_trans_subbuf| unsafe {
        let base = trans.mem.get().cast::<u64>().add(usize::from(buf.base));
        (base, base.add(usize::from(buf.u64s)))
    };

    let (mut e, top) = subbuf(trans.journal_entries.get());
    while e != top {
        let entry = e.cast::<Head<c::jset_entry>>();
        journal_entry_to_text(&mut out, Some(c), unsafe { from_ffi::<c::jset_entry<Unvalidated>>(entry) });
        prt!(out, "\n");
        e = e.wrapping_byte_add(unsafe { (*entry).bytes() });
    }

    let (mut k, top) = subbuf(trans.accounting.get());
    while k != top {
        let key = unsafe { &*k.cast::<c::bkey_i>() };
        bkey_val_to_text(&mut out, Some(c), BkeySC { k: &key.k, v: &key.v, iter: PhantomData, val: PhantomData });
        prt!(out, "\n");
        k = k.wrapping_add(usize::from(key.k.u64s));
    }
}

#[c_export]
pub fn bch2_current_has_btree_trans(c: &Fs) -> bool {
    let lock = Locked::from_c(&c.btree.trans.lock);
    let _guard = lock.lock();
    unsafe { TransList::from_c((&raw const c.btree.trans.list).cast_mut()) }
        .iter()
        .any(|trans| trans.locking_wait.task.load(Ordering::Relaxed) == sched::current() &&
                     foreign_bits(trans).locked())
}

// ── Restarts ────────────────────────────────────────────────────────────────

#[c_export]
pub fn bch2_trans_restart_foreign_task(trans: &BtreeTrans<'_>, err: c_int, ip: c_ulong) -> c_int {
    assert!(err > 0);
    assert!(BchError::from_raw(err).matches(bch_errcode::BCH_ERR_transaction_restart));

    trans.set_restarted(bch_errcode::from_bits(err as _));
    trans.last_restarted_ip.set(ip);
    -err
}

#[c_export]
pub fn bch2_trans_restart_ip(trans: &BtreeTrans<'_>, err: c_int, ip: c_ulong) -> c_int {
    bch2_trans_restart_foreign_task(trans, err, ip);
    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    {
        let _noreclaim = MemallocFlags::noreclaim();
        unsafe {
            let trace = &mut *trans.last_restarted_trace.as_ptr();
            darray_exit(trace);
            let _ = save_backtrace(trace, sched::current(), 0);
        }
    }
    -err
}

#[c_export]
#[cold]
#[inline(never)]
pub fn bch2_trans_restart_error(trans: &BtreeTrans<'_>, restart_count: u32) -> ! {
    let mut ip = Printbuf::new();
    prt_symbol(&mut ip, trans.last_begin_ip.get());
    panic!("trans->restart_count {}, should be {}, last restarted by {ip}", trans.restart_count.get(), restart_count);
}

#[cold]
#[inline(never)]
fn bch2_trans_in_restart_error(trans: &BtreeTrans<'_>) -> ! {
    let err = BchError::from_raw(trans.restarted().0 as c_int);
    let mut by = Printbuf::new();
    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    {
        prt_backtrace(&mut by, unsafe { &*trans.last_restarted_trace.as_ptr() }.as_slice());
        panic!("in transaction restart: {}, last restarted by\n{by}", err.msg());
    }
    #[cfg(not(CONFIG_BCACHEFS_DEBUG))]
    {
        prt_symbol(&mut by, trans.last_restarted_ip.get());
        panic!("in transaction restart: {}, last restarted by {by}", err.msg());
    }
}

#[c_export]
#[cold]
#[inline(never)]
pub fn bch2_trans_unlocked_or_in_restart_error(trans: &BtreeTrans<'_>) -> ! {
    if trans.restarted().0 != 0 {
        bch2_trans_in_restart_error(trans);
    }

    if !trans.locked() {
        let mut by = Printbuf::new();
        prt_symbol(&mut by, trans.last_unlock_ip.get());
        panic!("trans should be locked, unlocked by {by}");
    }

    panic!("BUG");
}

// ── Transaction names ───────────────────────────────────────────────────────

/// Each btree_trans!() call site's and C caller's name, by fn_idx: the
/// transaction's, and its stats'. 0 is "(unknown)".
pub static TRANSACTION_FNS: [AtomicPtr<c_char>; c::BCH_TRANSACTIONS_NR as usize] = {
    let mut fns = [const { AtomicPtr::new(ptr::null_mut()) }; c::BCH_TRANSACTIONS_NR as usize];
    fns[0] = AtomicPtr::new(c"(unknown)".as_ptr().cast_mut().cast());
    fns
};

#[c_export]
pub fn bch2_trans_get_fn_idx(fn_: *const c_char) -> c_uint {
    for (i, slot) in TRANSACTION_FNS.iter().enumerate() {
        let cur = slot.load(Ordering::Relaxed);
        if cur.is_null() || ptr::eq(cur, fn_) {
            slot.store(fn_.cast_mut(), Ordering::Relaxed);
            return i as c_uint;
        }
    }

    crate::warn_once!(true, no_fs, "BCH_TRANSACTIONS_NR not big enough!");
    0
}
