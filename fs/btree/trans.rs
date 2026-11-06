// SPDX-License-Identifier: GPL-2.0

//! The transaction: btree_trans!(), the handle Rust gets one by, its own
//! methods, and the filesystem's list of them.

pub use super::types::BtreeTrans;
use super::iter::{CommitOpts, TransAttempt, UpdateTriggerFlags};
use crate::alloc::capacity::DiskReservation;
use crate::btree::path::{BtreePath, PathIdx};
use crate::c;
use crate::errcode::{BchError, bch_errcode, ret_to_result_void as ret_to_result};
use crate::format::{Bpos, Btree};
use crate::fs::Fs;
use crate::locking::Locked;
use crate::util::backtrace::prt_task_backtrace;
use crate::util::closure::ClosureRef;
use crate::util::ffi::Opaque;
use crate::util::log::CFnName;
use crate::util::percpu::Counters;
use crate::util::refcount::TryRefcount;
use crate::util::{Printbuf, ToText};
use crate::data_structures::bitmap::Bitmap;
use crate::types::bits_to_longs;
use core::ffi::c_ulong;
use core::ops::ControlFlow;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

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

    /// This call site's index in bch2_btree_transaction_fns, registering
    /// @name on first use. 0 - "(unknown)" - while another thread is
    /// registering it, or if the table is full.
    pub fn idx(&self, name: &str) -> u32 {
        let idx = self.idx.load(Ordering::Acquire);
        if idx != 0 || self.claim.swap(true, Ordering::AcqRel) {
            return idx;
        }

        let idx = unsafe { c::bch2_trans_get_fn_idx(self.name.get(name).as_ptr().cast()) };
        self.idx.store(idx, Ordering::Release);
        idx
    }
}

/// A transaction Rust got, and puts when dropped: btree_trans!()'s, used as
/// the &BtreeTrans it derefs to - nothing else names it.
#[doc(hidden)]
pub struct TransGuard<'f> {
    trans: NonNull<BtreeTrans<'f>>,
}

impl<'f> TransGuard<'f> {
    /// Use btree_trans!(), which names the transaction for its caller.
    #[doc(hidden)]
    pub fn new_fn(fs: &'f Fs, fn_idx: u32) -> TransGuard<'f> {
        let raw = unsafe { c::__bch2_trans_get(fs.as_ptr(), fn_idx) };
        TransGuard { trans: NonNull::new(raw.cast()).expect("bch2_trans_get() never fails") }
    }
}

impl<'f> core::ops::Deref for TransGuard<'f> {
    type Target = BtreeTrans<'f>;

    fn deref(&self) -> &BtreeTrans<'f> {
        // SAFETY: ours until dropped
        unsafe { self.trans.as_ref() }
    }
}

impl Drop for TransGuard<'_> {
    fn drop(&mut self) {
        unsafe { c::bch2_trans_put(self.raw()) }
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
        unsafe { c::bch2_trans_begin(self.raw()) }
    }

    pub fn begin<'a>(&'a self) -> TransAttempt<'a, 'f> {
        TransAttempt::new(self, self.begin_raw())
    }

    pub fn verify_not_restarted(&self, restart_count: u32) {
        if self.restart_count.get() != restart_count {
            unsafe { c::bch2_trans_restart_error(self.raw(), restart_count) };
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

    fn next_path_idx(&self, idx: PathIdx) -> Option<PathIdx> {
        let nr = self.nr_paths.get() as usize;
        let words = unsafe { core::slice::from_raw_parts(self.paths_allocated.get().cast::<usize>(), bits_to_longs(nr)) };
        Bitmap::from_words(words).next(idx as usize, nr).map(|i| i as PathIdx)
    }

    /// TransAttempt::restart(), for what C calls in the middle of an attempt
    /// without one: the lock paths.
    pub(crate) fn restart(&self, error: bch_errcode) -> BchError {
        let ip = Self::restart as *const () as c_ulong;
        let ret = unsafe { c::bch2_trans_restart_ip(self.raw(), error.0 as i32, ip) };
        BchError::from_raw(-ret)
    }

    /// A panic, in debug builds, if the transaction's unlocked or in a
    /// restart: for what reads the nodes its paths hold.
    pub(crate) fn verify_not_unlocked_or_in_restart(&self) {
        if cfg!(CONFIG_BCACHEFS_DEBUG) {
            if self.restarted().0 != 0 || !self.locked() {
                unsafe { c::bch2_trans_unlocked_or_in_restart_error(self.raw()) }
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

/// A transaction off the list, kept live - its paths, its task - by the ref.
pub struct ListedTrans {
    trans: *mut BtreeTrans<'static>,
    _ref:  ClosureRef,
}

impl ListedTrans {
    /// Whether it holds the SRCU lock btree nodes are freed under.
    pub fn srcu_held(&self) -> bool {
        // its own thread changes it: READ_ONCE()
        unsafe { (&raw const (*self.trans).bits).cast::<c::btree_trans_bits>().read_volatile() }.srcu_held()
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
        unsafe { c::bch2_btree_trans_to_text(out.as_raw(), self.trans) }
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
            unsafe { <crate::list!(BtreeTrans<'static>, list)>::from_c(head) }
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
