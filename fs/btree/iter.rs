use super::bkey::*;
pub use super::types::BtreeIter;
use crate::alloc::buckets::DiskReservation;
use crate::c;
use crate::errcode::{
    BchError,
    bch_errcode,
    errptr_or_null,
    errptr_to_result,
    ret_to_result_void as ret_to_result,
};
use crate::fs::Fs;
use crate::locking::Lock;
use crate::util::closure::{closure_get_not_zero, ClosureRef};
use crate::util::ffi::Opaque;
use crate::util::log::CFnName;
use crate::util::{Printbuf, ToText};
use crate::SPOS_MAX;
use bitflags::bitflags;
use core::fmt;
use core::marker::PhantomData;
use core::mem::{size_of, MaybeUninit};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use core::ptr::NonNull;
use core::slice;
use core::ops::ControlFlow;

use c::bpos;
use crate::util::vstructs::{State, Validated};

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
        static FN: $crate::btree::iter::TransFnName = $crate::btree::iter::TransFnName::new();
        $crate::btree::iter::TransGuard::new_fn($fs, FN.idx($crate::function_name!()))
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

/// A btree transaction, on filesystem 'f: what C's struct btree_trans *
/// points at, its fields C's. Rust has it borrowed - &'t BtreeTrans, from the
/// TransGuard that got it or from C, at an entry point - and an iterator's
/// lifetime is that borrow.
#[repr(transparent)]
pub struct BtreeTrans<'f> {
    inner: Opaque<c::btree_trans>,
    fs:    PhantomData<&'f Fs>,
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
    pub unsafe fn borrow_raw<'a>(raw: *mut c::btree_trans) -> &'a BtreeTrans<'f> {
        unsafe { &*raw.cast() }
    }

    /// How many times this transaction has committed: what's cached from the
    /// btree is stale once it moves on.
    pub fn commit_count(&self) -> u32 {
        unsafe { (*self.raw()).commit_count }
    }

    /// The journal sequence number the commit in progress is going into: for
    /// atomic triggers, which run with the journal reservation held.
    pub fn journal_res_seq(&self) -> u64 {
        unsafe { (*self.raw()).journal_res.seq }
    }

    /// Delete [@start, @end) in @btree, committing as it goes. It drives its
    /// own begin and commit loop, so it's on the transaction, not an attempt:
    /// transaction_restart_nested if it restarted along the way, for a caller
    /// in an attempt to begin again.
    pub fn delete_range(
        &self,
        btree: c::btree_id,
        start: bpos,
        end:   bpos,
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

    /// The attempt C began, for Rust that C calls partway through one.
    ///
    /// # Safety
    /// For C entry points only: C has an attempt in progress on this
    /// transaction, for as long as the TransAttempt lives. Nothing else can
    /// say that - Rust gets its attempts from begin() and the loops.
    pub(crate) unsafe fn attempt_in_progress<'a>(&'a self) -> TransAttempt<'a, 'f> {
        TransAttempt {
            trans:         self,
            restart_count: unsafe { (*self.raw()).restart_count },
            t:             PhantomData,
        }
    }

    /// bch2_trans_begin(), for a loop that tracks restarts itself: the new
    /// restart count.
    pub(crate) fn begin_raw(&self) -> u32 {
        unsafe { c::bch2_trans_begin(self.raw()) }
    }

    /// Take back the locks unlock() dropped: a transaction restart if they've
    /// changed hands meanwhile.
    pub fn relock(&self) -> Result<(), BchError> {
        let trans = unsafe { &*self.raw() };
        if trans.locked() && trans.restarted().0 == 0 {
            return Ok(());
        }
        crate::errcode::ret_to_result(unsafe { c::__bch2_trans_relock(self.raw(), true) }).map(|_| ())
    }

    /// relock(), always attempted and without the tracepoint: as
    /// bch2_trans_relock_notrace().
    pub fn relock_notrace(&self) -> Result<(), BchError> {
        crate::errcode::ret_to_result(unsafe { c::bch2_trans_relock_notrace(self.raw()) }).map(|_| ())
    }

    pub fn begin<'a>(&'a self) -> TransAttempt<'a, 'f> {
        TransAttempt {
            trans:         self,
            restart_count: self.begin_raw(),
            t:             PhantomData,
        }
    }

    pub fn verify_not_restarted(&self, restart_count: u32) {
        unsafe {
            if (*self.raw()).restart_count != restart_count {
                c::bch2_trans_restart_error(self.raw(), restart_count);
            }
        }
    }

    pub fn restart_count(&self) -> u32 {
        unsafe { (*self.raw()).restart_count }
    }

    pub fn raw(&self) -> *mut c::btree_trans {
        self.inner.as_ptr()
    }

    pub(crate) fn fs(&self) -> &'f Fs {
        // SAFETY: trans->c, as the type says, outlives it
        unsafe { Fs::borrow_raw((*self.raw()).c) }
    }

    pub fn unlock(&self) {
        unsafe { c::bch2_trans_unlock(self.raw()) };
    }

    pub fn unlock_long(&self) {
        unsafe { c::bch2_trans_unlock_long(self.raw()) };
    }

    pub fn commit(
        &self,
        disk_res: Option<&DiskReservation<'_>>,
        flags: impl Into<CommitOpts>,
    ) -> Result<(), BchError> {
        self.commit_inner(disk_res, flags.into(), false)
    }

    /// The static inline bch2_trans_commit(), with __bch2_trans_commit()'s
    /// @lazy. journal_seq and flush are reset, as C's every commit does: a C
    /// commit before ours, in the same transaction, may have left a pointer to
    /// its stack there.
    fn commit_inner(
        &self,
        disk_res: Option<&DiskReservation<'_>>,
        flags:    CommitOpts,
        lazy:     bool,
    ) -> Result<(), BchError> {
        let ret = unsafe {
            let trans = self.raw();
            (*trans).disk_res    = disk_res.map_or(core::ptr::null_mut(), |r| r.as_mut_ptr());
            (*trans).journal_seq = core::ptr::null_mut();
            (*trans).flush       = core::ptr::null_mut();
            c::__bch2_trans_commit(trans, flags.to_c(), lazy)
        };
        ret_to_result(ret)
    }
}

pub struct TransAttempt<'a, 't> {
    trans:         &'a BtreeTrans<'t>,
    restart_count: u32,
    t:             PhantomData<&'a mut ()>,
}

/// What an iterator is made from: the transaction, or an attempt at it. An
/// iterator belongs to the transaction, and outlives any one attempt: @'t is
/// the borrow of the transaction - the attempt's, for an attempt - that it's
/// bound by, so it can't outlive the transaction its C iterator points into.
///
/// XXX: making an iterator needs the transaction locked and not in a restart,
/// and nothing here says so. bch2_path_get() reuses the transaction's paths
/// and set_pos()es them, reading the bsets of the nodes they hold: unlocked,
/// those nodes can be rewritten or freed. C checks, and panics
/// (bch2_trans_unlocked_or_in_restart_error()); C's loops get it right by
/// bch2_trans_begin() before the iterator's init. A &BtreeTrans or
/// &TransGuard vouches for nothing - an unlock() before, with no relock, and
/// it's that panic, at runtime. An attempt vouches for it only until its
/// first unlock() or error.
///
/// Next steps, in order:
/// - Iterators made only from a &TransAttempt: drop the &BtreeTrans and
///   &TransGuard impls, and the compiler lists the callers to convert.
///   TransAttempt::unlock() consumes the attempt; drop_locks_do() for
///   unlock, f, relock.
/// - That's lifetimes standing in for the state, and approximately. What we
///   want is the verifier tracking it: locked, unlocked or in restart, a Cell
///   in BtreeTrans, changed only by begin(), relock(), unlock() and anything
///   that returns an error - each proven to have its effect, construction
///   proven to need locked, and runtime asserts where a proof can't reach,
///   which later proofs can then assume. Verus can express this now (a
///   PCell and its permission); Aeneas, which we're leaning towards, first
///   needs UnsafeCell support - shared borrows of !Freeze types as heap
///   locations, reads and writes through them as effects.
pub trait TransRef<'t> {
    fn trans_raw(&self) -> *mut c::btree_trans;
}

impl<'i, 'f> TransRef<'i> for &'i BtreeTrans<'f> {
    fn trans_raw(&self) -> *mut c::btree_trans { self.raw() }
}

impl<'i, 'f> TransRef<'i> for &'i TransGuard<'f> {
    fn trans_raw(&self) -> *mut c::btree_trans { self.raw() }
}

impl<'a, 't> TransRef<'a> for &TransAttempt<'a, 't> {
    fn trans_raw(&self) -> *mut c::btree_trans { self.trans.raw() }
}

/// Exempts bch2_trans_begin() from the dropped-updates warning while it lives
/// (see begin_may_drop_updates), and restores the previous value when dropped,
/// so exemptions nest - as C's CLASS(trans_may_drop_updates).
struct TransMayDropUpdates<'a, 't> {
    trans: &'a BtreeTrans<'t>,
    old:   bool,
}

impl<'a, 't> TransMayDropUpdates<'a, 't> {
    fn new(trans: &'a BtreeTrans<'t>) -> Self {
        let raw = unsafe { &mut *trans.raw() };
        let old = raw.begin_may_drop_updates();
        raw.set_begin_may_drop_updates(true);
        TransMayDropUpdates { trans, old }
    }
}

impl Drop for TransMayDropUpdates<'_, '_> {
    fn drop(&mut self) {
        unsafe { (*self.trans.raw()).set_begin_may_drop_updates(self.old) };
    }
}

/// Whether @e is a transaction restart: the attempt is over, and the loop
/// it's in begins again.
pub fn is_restart(e: &BchError) -> bool {
    e.matches(bch_errcode::BCH_ERR_transaction_restart)
}

/// What a for_each body returns on success: whether the loop goes on - to the
/// next key, or from a position of the body's - and if not, what it stopped
/// with. `()` for a body that always goes on to the next key - most of them,
/// as a C loop body returns 0 - `ControlFlow` for one that can stop early,
/// breaking with what the loop returns, and `LoopFlow` for one that can also
/// move the loop on.
///
/// A loop that runs to the end returns Break's default: `()`, `None`,
/// `false` - nothing found.
pub trait LoopControl {
    type Break: Default;

    fn into_step(self) -> Step<Self::Break>;
}

/// Where a for_each loop goes after a key.
pub enum Step<B> {
    /// The next key.
    Next,
    /// The keys from @pos, in place of the next. A C loop body's
    /// bch2_btree_iter_set_pos(p) is followed by the loop's advance, from p:
    /// its From is where that lands - p's successor, or for extents p.
    From(bpos),
    Break(B),
}

impl LoopControl for () {
    type Break = ();

    fn into_step(self) -> Step<()> { Step::Next }
}

impl<B: Default> LoopControl for ControlFlow<B> {
    type Break = B;

    fn into_step(self) -> Step<B> {
        match self {
            ControlFlow::Continue(()) => Step::Next,
            ControlFlow::Break(b)     => Step::Break(b),
        }
    }
}

/// ControlFlow, and continuing from a position: for a body that moves the
/// loop on - past the rest of an inode's snapshot versions, say.
pub enum LoopFlow<B = ()> {
    Continue,
    ContinueFrom(bpos),
    Break(B),
}

impl<B: Default> LoopControl for LoopFlow<B> {
    type Break = B;

    fn into_step(self) -> Step<B> {
        match self {
            LoopFlow::Continue          => Step::Next,
            LoopFlow::ContinueFrom(pos) => Step::From(pos),
            LoopFlow::Break(b)          => Step::Break(b),
        }
    }
}

/// A key to update with, valid and ours to edit while the attempt runs - in
/// the transaction's memory, or a buffer C hands over (from_raw_room()): of
/// key type @V, its value whole - as the typed constructors make one - or,
/// untyped, any key. It's edited in place, in its buffer, never moved: what
/// won't fit is NoRoom.
pub struct TransBkey<'a, 't, V: ?Sized = FamilyVal<AllTypes>> {
    ptr:      NonNull<c::bkey_i>,
    buf_u64s: u32,
    t:        PhantomData<&'a mut TransAttempt<'a, 't>>,
    v:        PhantomData<*const V>,
}

/// A key that won't fit where it is: @need_u64s, header and all, where its
/// buffer has @have_u64s - or a key can, which is at most a u8's worth.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoRoom {
    pub need_u64s: usize,
    pub have_u64s: usize,
}

impl fmt::Display for NoRoom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "key needs {} u64s, buffer has {}", self.need_u64s, self.have_u64s)
    }
}

pub(crate) struct BtreeNodeRef {
    raw: NonNull<c::btree>,
}

impl BtreeNodeRef {
    pub(crate) fn level(&self) -> u32 {
        unsafe { self.raw.as_ref().c.level.into() }
    }

    pub(crate) fn key(&self) -> &BkeyI {
        // SAFETY: a node we hold
        unsafe { self.raw.as_ref() }.key_i()
    }

    pub(crate) fn key_sc(&self) -> BkeySC<'_> {
        self.key().into()
    }

    pub(crate) fn btree_id(&self) -> c::btree_id {
        c::btree_id(unsafe { self.raw.as_ref().c.btree_id }.into())
    }

    /// The node's identity, for telling whether it's the same node as
    /// before - not for getting back to it.
    pub(crate) fn addr(&self) -> usize {
        self.raw.as_ptr() as usize
    }

    pub(crate) fn as_ptr(&self) -> *mut c::btree {
        self.raw.as_ptr()
    }
}

impl<'a, 't> TransAttempt<'a, 't> {
    pub fn raw(&self) -> *mut c::btree_trans {
        self.trans.raw()
    }

    pub(crate) fn fs(&self) -> &'t Fs {
        self.trans.fs()
    }

    pub fn verify_not_restarted(&self) {
        self.trans.verify_not_restarted(self.restart_count);
    }

    /// For what a step does to the transaction itself - a write buffer flush, a
    /// nested loop: those end this attempt, leaving a restart for the step to
    /// return.
    pub fn trans(&self) -> &'a BtreeTrans<'t> {
        self.trans
    }

    pub fn unlock(&self) {
        self.trans.unlock()
    }

    pub fn relock(&self) -> Result<(), BchError> {
        self.trans.relock()
    }

    pub fn commit_count(&self) -> u32 {
        self.trans.commit_count()
    }

    pub fn journal_res_seq(&self) -> u64 {
        self.trans.journal_res_seq()
    }

    /// Whether anything is queued for the next commit: key updates, journal
    /// entries or accounting.
    pub fn has_updates(&self) -> bool {
        let trans = unsafe { &*self.raw() };
        trans.nr_updates != 0 ||
            trans.journal_entries.u64s != 0 ||
            trans.accounting.u64s != 0
    }

    /// Drop everything queued for the next commit: as C's
    /// bch2_trans_reset_updates(), which every commit and begin runs.
    fn reset_updates(&self) {
        let raw = self.raw();
        unsafe {
            // bch2_path_put() changes the transaction: no reference to it
            // is held across the calls
            for i in 0..(*raw).nr_updates as usize {
                c::bch2_path_put(raw, (*(*raw).updates.add(i)).path, true);
            }

            let trans = &mut *raw;
            trans.nr_updates               = 0;
            trans.journal_entries.u64s     = 0;
            trans.journal_entries.size     = 0;
            trans.accounting.u64s          = 0;
            trans.accounting.size          = 0;
            trans.hooks                    = core::ptr::null_mut();
            trans.extra_disk_res           = 0;
            trans.extra_disk_res_replicas  = 0;
            trans.extra_journal_u64s       = 0;
            trans.set_has_interior_updates(false);
        }
    }

    // ── The attempt's operations ─────────────────────────────────────────
    //
    // All take the attempt shared, and a restart is an error like any other:
    // the caller returns it, and the loop driving the attempt begins again.
    // That nothing is done in an attempt after it has restarted, or with a
    // key peeked before a commit, isn't for the types to say: both are rules
    // about order within the attempt, left for now to C's runtime checks
    // (trans->restarted) and to review.

    pub fn commit(
        &self,
        disk_res: Option<&DiskReservation<'_>>,
        flags:    impl Into<CommitOpts>,
    ) -> Result<(), BchError> {
        self.trans.commit(disk_res, flags)
    }

    /// @msg into the journal, with this transaction's commit: as
    /// bch2_trans_log_msg(). ENOMEM_trans_log_msg if building @msg ran out of
    /// memory.
    pub fn log_msg(&self, msg: &mut crate::util::Printbuf) -> Result<(), BchError> {
        let ret = unsafe { c::bch2_trans_log_msg(self.raw(), msg.as_raw()) };
        self.result(ret)
    }

    /// Commit what's queued, if anything - and having committed, return
    /// transaction_restart_commit, so the caller's loop re-runs against the
    /// committed state.
    pub fn commit_lazy(&self, flags: impl Into<CommitOpts>) -> Result<(), BchError> {
        if !self.has_updates() {
            return Ok(());
        }

        self.trans.commit_inner(None, flags.into(), true)
    }

    /// Close this attempt, for work at the transaction level - work that runs
    /// loops of its own, beginning attempts as it goes: commit what's queued,
    /// so its first begin drops nothing (a repair's fsck log entry, say), and
    /// run @f on the transaction. Then begin the next attempt, for the restart
    /// the caller returns: its loop retries against what @f did. As C's
    /// helpers that commit their caller's updates, run their own loop and
    /// return transaction_restart_nested - bch2_fpunch_snapshot().
    ///
    /// The restart is the caller's only way on: `return Err(t.close(..))`.
    /// An error from the commit or from @f is returned instead.
    ///
    /// Keys peeked in this attempt aren't to be used after - as across a
    /// commit, which the types don't yet enforce either.
    #[must_use]
    pub fn close<F>(&self, flags: impl Into<CommitOpts>, f: F) -> BchError
    where
        F: FnOnce(&'a BtreeTrans<'t>) -> Result<(), BchError>,
    {
        if self.has_updates() {
            if let Err(e) = self.commit(None, flags) {
                return e;
            }
        }
        if let Err(e) = f(self.trans) {
            return e;
        }
        self.trans.begin().restart(bch_errcode::BCH_ERR_transaction_restart_nested)
    }

    /// Commit only once the transaction's memory is getting full: for a key
    /// whose repairs are bounded only by something like snapshot count. A
    /// commit here restarts, and the re-drive has less to queue, so it
    /// converges.
    pub fn commit_lazy_if_full(&self, flags: impl Into<CommitOpts>) -> Result<(), BchError> {
        // disk_accounting_mod allocations grow by powers of 2; max / 2 is too
        // small of a limit to avoid hitting ENOMEMs
        if unsafe { (*self.raw()).mem_top } < c::BTREE_TRANS_MEM_MAX / 4 {
            return Ok(());
        }
        self.commit_lazy(flags)
    }

    pub fn result(&self, ret: i32) -> Result<(), BchError> {
        ret_to_result(ret).map(|_| ())
    }

    /// The transaction has to know: it's marked restarted and its restart_count
    /// bumped, which bch2_trans_begin() and verify_not_restarted() go by.
    pub fn restart(&self, error: bch_errcode) -> BchError {
        let ip = Self::restart as *const () as core::ffi::c_ulong;
        let ret = unsafe { c::bch2_trans_restart_ip(self.raw(), error.0 as i32, ip) };
        BchError::from_raw(-ret)
    }

    /// What @alloc allocates, tried first without blocking - GFP_NOWAIT, the
    /// locks held - and if that fails, again with them dropped, GFP_KERNEL,
    /// then retaken: as C's allocate_dropping_locks(). None if it fails both
    /// ways. A restart if the locks changed hands while they were dropped -
    /// and what @alloc made then is dropped with it, where C has the caller
    /// free it.
    pub fn allocate_dropping_locks<T>(
        &self,
        mut alloc: impl FnMut(c::gfp_t) -> Option<T>,
    ) -> Result<Option<T>, BchError> {
        if let Some(p) = alloc(c::GFP_NOWAIT) {
            return Ok(Some(p));
        }
        self.unlock();
        let p = alloc(c::GFP_KERNEL);
        self.relock()?;
        Ok(p)
    }

    /// allocate_dropping_locks(), leaving the locks dropped: for the end of an
    /// attempt, as C's allocate_dropping_locks_norelock().
    pub fn allocate_dropping_locks_norelock<T>(&self, mut alloc: impl FnMut(c::gfp_t) -> Option<T>)
        -> Option<T>
    {
        if let Some(p) = alloc(c::GFP_NOWAIT) {
            return Some(p);
        }
        self.unlock();
        alloc(c::GFP_KERNEL)
    }

    /// allocate_dropping_locks(), for an @alloc that says it couldn't by
    /// ENOMEM: as C's allocate_dropping_locks_errcode(). Another error from
    /// the first try is returned as is; one from the second, with the locks
    /// still dropped, as C's drop_locks_do() does.
    pub fn allocate_dropping_locks_errcode<T>(
        &self,
        mut alloc: impl FnMut(c::gfp_t) -> Result<T, BchError>,
    ) -> Result<T, BchError> {
        match alloc(c::GFP_NOWAIT) {
            Err(e) if e.matches(c::ENOMEM) => {}
            r => return r,
        }
        self.unlock();
        let p = alloc(c::GFP_KERNEL)?;
        self.relock()?;
        Ok(p)
    }

    /// Run @f as a nested transaction, as C's nested_lockrestart_do(): @f
    /// carries on this attempt - no bch2_trans_begin() - and is retried on
    /// restart.
    ///
    /// If anything restarted, the outer attempt is spent - its locks and
    /// whatever it had queued are gone - and the caller is told with
    /// transaction_restart_nested, after @f succeeds: the outer loop retries
    /// from the top. Nested commits drop the outer attempt's queued updates
    /// on purpose, so the dropped-updates warning is off while @f runs.
    ///
    /// XXX: the attempts @f is retried in begin within this one, so a key
    /// the caller peeked before is stale after a retry - which nothing here
    /// stops it using.
    pub fn nested<T, F>(&self, mut f: F) -> Result<T, BchError>
    where
        F: for<'b> FnMut(&TransAttempt<'b, 't>) -> Result<T, BchError>,
    {
        let trans = self.trans;
        let orig_restart_count = unsafe { (*trans.raw()).restart_count };
        let _may_drop = TransMayDropUpdates::new(trans);

        let mut retry;
        let mut t = self;
        loop {
            match f(t) {
                Ok(v) => {
                    t.verify_not_restarted();

                    if unsafe { (*trans.raw()).restart_count } != orig_restart_count {
                        return Err(BchError::from_raw(
                            bch_errcode::BCH_ERR_transaction_restart_nested.0 as i32));
                    }
                    return Ok(v);
                }
                Err(e) if is_restart(&e) => {
                    retry = trans.begin();
                    t = &retry;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Freed when the transaction next begins.
    pub fn kmalloc(&self, bytes: usize) -> Result<&'a mut [u8], BchError> {
        let ptr = self.kmalloc_raw(bytes)?;
        Ok(unsafe { core::slice::from_raw_parts_mut(ptr, bytes) })
    }

    /// @bytes of zeroed transaction memory, rounded up to u64s: off the top of
    /// the transaction's buffer, or __bch2_trans_kmalloc() grows it - which
    /// restarts the transaction if it had a buffer already. As
    /// bch2_trans_kmalloc(), less CONFIG_BCACHEFS_TRANS_KMALLOC_TRACE's
    /// record of the allocation.
    fn kmalloc_raw(&self, bytes: usize) -> Result<*mut u8, BchError> {
        let size = bytes.next_multiple_of(size_of::<u64>());
        let trans = self.raw();

        unsafe {
            let top = (*trans).mem_top as usize;
            if top + size > (*trans).mem_bytes as usize {
                let p = NonNull::new(c::__bch2_trans_kmalloc(trans, size, 0)).expect("memory, or an error");
                return Ok(errptr_to_result(p)?.as_ptr().cast());
            }

            let p = ((*trans).mem as *mut u8).add(top);
            (*trans).mem_top += size as u32;
            p.write_bytes(0, size);
            Ok(p)
        }
    }

    /// Run @f in this attempt's commit, under its btree write locks. It runs
    /// before the commit is certain - what follows can still fail it - and a
    /// restart drops it, with the rest of the attempt's memory.
    ///
    /// That memory is freed without running destructors, so @f is Copy; and
    /// it outlives this call, so 'static.
    pub fn commit_hook<F>(&self, f: F) -> Result<(), BchError>
    where
        F: Fn(&TransAttempt<'_, '_>) -> Result<(), BchError> + Copy + 'static,
    {
        #[repr(C)]
        struct Hook<F> {
            h: c::btree_trans_commit_hook,
            f: F,
        }

        unsafe extern "C" fn run<F>(
            trans: *mut c::btree_trans,
            h:     *mut c::btree_trans_commit_hook,
        ) -> core::ffi::c_int
        where
            F: Fn(&TransAttempt<'_, '_>) -> Result<(), BchError>,
        {
            // SAFETY: @h is the first field of the Hook<F> commit_hook() queued.
            let hook = unsafe { &*(h as *const Hook<F>) };
            let trans = unsafe { BtreeTrans::borrow_raw(trans) };
            // SAFETY: C runs hooks in the commit, in the attempt being committed
            let t = unsafe { trans.attempt_in_progress() };
            crate::errcode::ret_to_c((hook.f)(&t))
        }

        // bch2_trans_kmalloc() gives 8 byte alignment:
        const { assert!(core::mem::align_of::<Hook<F>>() <= 8) };

        let hook = self.kmalloc(size_of::<Hook<F>>())?.as_mut_ptr() as *mut Hook<F>;
        unsafe {
            hook.write(Hook {
                h: c::btree_trans_commit_hook { fn_: Some(run::<F>), next: core::ptr::null_mut() },
                f,
            });
            c::bch2_trans_commit_hook(self.raw(), &raw mut (*hook).h);
        }
        Ok(())
    }

    pub fn bkey_alloc(&self, u64s: u32) -> Result<TransBkey<'a, 't>, BchError> {
        let bytes = u64s as usize * size_of::<u64>();
        let ptr = self.kmalloc_raw(bytes)? as *mut c::bkey_i;

        Ok(TransBkey {
            ptr:      NonNull::new(ptr).expect("transaction memory at NULL"),
            buf_u64s: u64s,
            t:        PhantomData,
            v:        PhantomData,
        })
    }

    pub fn bkey_alloc_typed<K: BkeyWhole>(&self) -> Result<TransBkey<'a, 't, K>, BchError> {
        debug_assert_eq!(size_of::<BkeyI<K>>() % size_of::<u64>(), 0);

        let u64s = (size_of::<BkeyI<K>>() / size_of::<u64>()) as u32;
        let k = self.bkey_alloc(u64s)?;

        unsafe {
            k.as_ptr().cast::<BkeyI<K>>().write(BkeyI::new());
        }

        // SAFETY: a @K's key, all of it
        Ok(unsafe { k.whole_unchecked() })
    }

    pub fn bkey_copy(&self, k: &BkeyI) -> Result<TransBkey<'a, 't>, BchError> {
        let dst = self.bkey_alloc(k.k.u64s as u32)?;
        // SAFETY: all of @k, into a buffer as long
        unsafe {
            core::ptr::copy_nonoverlapping(k.as_ffi().cast::<u8>(), dst.as_ptr().cast::<u8>(),
                                           size_of_val(k));
        }
        Ok(dst)
    }

    /// @k, copied as it is, of the type it's read as: any key, or a type an
    /// exact copy of is whole - a plain type, not a grown one, whose older
    /// values bkey_make_mut_noupdate() grows.
    pub fn bkey_reassemble<V: MutBkey<Mut = V> + ?Sized>(&self, k: BkeySC<'_, V>)
        -> Result<TransBkey<'a, 't, V>, BchError>
    {
        let mut dst = self.bkey_alloc(k.k.u64s as u32)?;

        unsafe {
            core::ptr::copy_nonoverlapping(k.k, &raw mut (*dst.as_ptr()).k, 1);
            core::ptr::copy_nonoverlapping(
                k.v as *const c::bch_val as *const u64,
                dst.as_mut_u64s().as_mut_ptr().add(BKEY_U64S),
                k.k.u64s as usize - BKEY_U64S,
            );
        }

        debug_assert!(dst.buf_u64s as usize * size_of::<u64>() >= V::MIN_BYTES);
        // SAFETY: @k's, which as a valid @V's is MIN_BYTES: MutBkey's
        Ok(unsafe { dst.cast() })
    }

    /// bkey_reassemble() into a buffer with `val_u64s` of value space:
    /// the copied value is zero-extended (or truncated) to the new size.
    pub fn bkey_reassemble_resized(&self, k: BkeySC<'_>, val_u64s: usize)
        -> Result<TransBkey<'a, 't>, BchError>
    {
        let mut dst = self.bkey_alloc((BKEY_U64S + val_u64s) as u32)?;
        dst.as_mut_u64s().fill(0);

        let copy_val = (k.k.u64s as usize - BKEY_U64S).min(val_u64s);
        unsafe {
            core::ptr::copy_nonoverlapping(k.k, &mut dst.as_mut().k, 1);
            core::ptr::copy_nonoverlapping(
                k.v as *const c::bch_val as *const u64,
                dst.as_mut_u64s().as_mut_ptr().add(BKEY_U64S),
                copy_val,
            );
        }
        dst.k_mut().u64s = (BKEY_U64S + val_u64s) as u8;

        Ok(dst)
    }

    /// A fresh @K: bkey_init()ed header at `pos`, and `val_u64s` of zeroed
    /// value space - at least a @K's min_val_size.
    pub fn bkey_alloc_init<K: BkeyTag + BkeyFamily>(&self, val_u64s: usize, pos: c::bpos)
        -> Result<TransBkey<'a, 't, K::Validated>, BchError>
    {
        assert!(val_u64s * size_of::<u64>() >= K::MIN_VAL_SIZE,
                "{val_u64s} u64s of value, shorter than a {:?}'s {}", K::TYPE, K::MIN_VAL_SIZE);
        let mut k = self.bkey_alloc((BKEY_U64S + val_u64s) as u32)?;
        k.as_mut_u64s().fill(0);
        *k.k_mut() = c::bkey::new();
        k.k_mut().u64s = (BKEY_U64S + val_u64s) as u8;
        k.k_mut().type_ = K::TYPE.0 as u8;
        k.k_mut().p = pos;
        // SAFETY: a @K by its type, as long as one
        Ok(unsafe { k.arm_unchecked::<K>() })
    }

    /// A copy of @k in a buffer of at least @min_bytes - as C's
    /// bkey_reassemble() into one it allocated: the key as long as it was,
    /// with room for a callee to grow it, of the type it's read as, as
    /// bkey_reassemble()'s copy is. A u64 more is allocated past the end, for
    /// varint_decode_fast().
    pub fn bkey_reassemble_room<V: MutBkey<Mut = V> + ?Sized>(&self, k: BkeySC<'_, V>, min_bytes: usize)
        -> Result<TransBkey<'a, 't, V>, BchError>
    {
        self.reassemble_room(k, min_bytes)
    }

    /// bkey_reassemble_room(), of any V: the copy as long as @k, read as @k
    /// is - the caller makes it what it's read as after.
    fn reassemble_room<V: ?Sized + BkeyVal>(&self, k: BkeySC<'_, V>, min_bytes: usize)
        -> Result<TransBkey<'a, 't, V>, BchError>
    {
        let k_u64s = k.k.u64s as usize;
        let u64s   = k_u64s.max(min_bytes.div_ceil(size_of::<u64>()));

        let mut dst = self.bkey_alloc(u64s as u32 + 1)?;
        dst.buf_u64s = u64s as u32;

        unsafe {
            core::ptr::copy_nonoverlapping(k.k, dst.k_mut(), 1);
            core::ptr::copy_nonoverlapping(
                k.v as *const c::bch_val as *const u64,
                dst.as_mut_u64s().as_mut_ptr().add(BKEY_U64S),
                k_u64s - BKEY_U64S,
            );
        }
        // SAFETY: @k's, as long as it was: a @V, as bkey_reassemble()'s copy is
        Ok(unsafe { dst.cast() })
    }

    /// @k as a mutable copy, for an update, with at least @min_bytes: past
    /// @k's own value, zeroes, which is what those fields read as, and the key
    /// covers them, so writes there go in with the commit.
    fn make_mut_noupdate<V: ?Sized + BkeyVal>(&self, k: BkeySC<'_, V>, min_bytes: usize)
        -> Result<TransBkey<'a, 't, V>, BchError>
    {
        let mut dst = self.reassemble_room(k, min_bytes)?;
        dst.k_mut().u64s = dst.buf_u64s as u8;
        Ok(dst)
    }

    /// @k as a mutable copy, not yet an update: of the type @k is read as -
    /// any key's, as it is, or a typed key's, grown to all of its value
    /// (MutBkey).
    pub fn bkey_make_mut_noupdate<V: MutBkey + ?Sized>(&self, k: BkeySC<'_, V>)
        -> Result<TransBkey<'a, 't, V::Mut>, BchError>
    {
        let k = self.make_mut_noupdate(k, V::MIN_BYTES)?;
        // SAFETY: @k's, grown to MIN_BYTES: MutBkey's
        Ok(unsafe { k.cast() })
    }

    /// The value of the @K at @pos in @btree, zero padded:
    /// ENOENT_bkey_type_mismatch if it isn't one.
    pub fn bkey_get_val<K: BkeyWhole>(&self, btree: c::btree_id, pos: bpos, flags: BtreeIterFlags)
        -> Result<K, BchError>
    {
        let mut iter = BtreeIter::new(self.trans, btree, pos, flags);
        let k = iter.peek_slot_typed::<K>(self)?.untyped();
        Ok(K::val_copy_pad(k).expect("peek_slot_typed() checked the type"))
    }

    /// bkey_get_val(), the whole key.
    pub fn bkey_get_key<K: BkeyWhole>(&self, btree: c::btree_id, pos: bpos, flags: BtreeIterFlags)
        -> Result<BkeyI<K>, BchError>
    {
        let mut iter = BtreeIter::new(self.trans, btree, pos, flags);
        let k = iter.peek_slot_typed::<K>(self)?.untyped();
        Ok(BkeyI::from_key(k).expect("peek_slot_typed() checked the type"))
    }

    /// The @K at @pos in @btree, as a mutable copy already queued as its
    /// update, grown to all of its value - ENOENT_bkey_type_mismatch if it
    /// isn't a @K. @iter_flags for the lookup - BtreeIterFlags::CACHED, for a
    /// btree read through the key cache - and @flags for the update: C passes
    /// both in one enum.
    pub fn bkey_get_mut<K: BkeyWhole>(
        &self,
        btree:      c::btree_id,
        pos:        bpos,
        iter_flags: BtreeIterFlags,
        flags:      UpdateTriggerFlags,
    ) -> Result<TransBkey<'a, 't, K>, BchError> {
        let mut iter = BtreeIter::new(self.trans, btree, pos, iter_flags | BtreeIterFlags::INTENT);
        let k = iter.peek_slot_typed::<K>(self)?.untyped();
        let k = self.make_mut_noupdate(k, size_of::<BkeyI<K>>())?;
        self.update(&iter, &k, flags)?;
        // SAFETY: a @K's, grown to all of it
        Ok(unsafe { k.whole_unchecked() })
    }

    /// @k, the key at @iter, as bkey_make_mut_noupdate() makes it, already
    /// queued as its update: edits to it go in with the commit.
    pub fn bkey_make_mut<V: MutBkey + ?Sized>(
        &self,
        iter:  &BtreeIter<'a>,
        k:     BkeySC<'_, V>,
        flags: UpdateTriggerFlags,
    ) -> Result<TransBkey<'a, 't, V::Mut>, BchError> {
        let k = self.bkey_make_mut_noupdate(k)?;
        self.update(iter, &k, flags)?;
        Ok(k)
    }

    /// Overwrite @old, the extent at @iter, with @new where they overlap.
    pub fn update_extent_overwrite(
        &self,
        iter:  &BtreeIter<'a>,
        flags: UpdateTriggerFlags,
        old:   BkeySC<'_>,
        new:   BkeySC<'_>,
    ) -> Result<(), BchError> {
        let ret = unsafe {
            c::bch2_trans_update_extent_overwrite(self.raw(), iter.raw(),
                                                  c::btree_iter_update_trigger_flags::from_bits_retain(flags.bits()),
                                                  old.into(), new.into())
        };
        self.result(ret)
    }

    /// Have the commit reserve @sectors more, at @nr_replicas, for data an
    /// update rewrites.
    pub fn extra_disk_res_add(&self, sectors: u64, nr_replicas: u32) {
        let trans = unsafe { &mut *self.raw() };
        trans.extra_disk_res += sectors;
        trans.extra_disk_res_replicas = trans.extra_disk_res_replicas.max(nr_replicas as u8);
    }

    /// Queue @key as an update at @iter's position. Takes the iterator
    /// shared: it doesn't move it - what it sets up there, the key cache
    /// path, is the iterator's own business. @key is borrowed too: it's in
    /// transaction memory, where the update points, and the caller can go on
    /// reading it - where it went, say.
    pub fn update<V: ?Sized>(
        &self,
        iter:  &BtreeIter<'a>,
        key:   &TransBkey<'_, 't, V>,
        flags: UpdateTriggerFlags,
    ) -> Result<(), BchError> {
        // No caller ip: C's is the call site's, _THIS_IP_, which Rust has
        // no way to name.
        let ret = unsafe {
            c::bch2_trans_update_ip(
                self.raw(),
                iter.raw(),
                key.as_ptr(),
                key.buf_u64s,
                c::btree_iter_update_trigger_flags::from_bits_retain(flags.bits()),
                0,
            )
        };
        self.result(ret)
    }

    /// update(), with a key that isn't in transaction memory - a C caller's,
    /// as bch2_trans_update() takes. Mutably: the update may rewrite it, a
    /// deletion that needs a whiteout becoming one.
    ///
    /// # Safety
    /// @key stays where it is, unchanged, until the commit - or until the
    /// attempt is over, if it doesn't commit. The update points at it.
    pub unsafe fn update_key(
        &self,
        iter:  &BtreeIter<'a>,
        key:   &mut BkeyI,
        flags: UpdateTriggerFlags,
    ) -> Result<(), BchError> {
        let ret = unsafe {
            c::bch2_trans_update_ip(
                self.raw(),
                iter.raw(),
                key.as_ffi_mut(),
                key.k.u64s as u32,
                c::btree_iter_update_trigger_flags::from_bits_retain(flags.bits()),
                0,
            )
        };
        self.result(ret)
    }

    /// A key moved from @old_pos to @new_pos, in the same snapshot: in every
    /// descendant snapshot where it was overwritten at @old_pos, whiteout
    /// @new_pos too, so it stays hidden there.
    pub fn insert_snapshot_whiteouts(&self, btree: c::btree_id, old_pos: bpos, new_pos: bpos)
        -> Result<(), BchError>
    {
        assert_eq!(old_pos.snapshot, new_pos.snapshot);

        if crate::BTREE_HAS_SNAPSHOTS_MASK & (1 << btree.0) == 0 || bkey_eq(old_pos, new_pos) {
            return Ok(());
        }

        let mut ids = crate::snapshots::snapshot::overwrites(self, btree, old_pos)?;
        if ids.is_empty() {
            return Ok(());
        }

        // Lent to C, which only reads it:
        let mut list = c::snapshot_id_list {
            nr:           ids.len(),
            size:         ids.len(),
            data:         ids.as_mut_ptr(),
            preallocated: [],
        };
        let ret = unsafe { c::__bch2_insert_snapshot_whiteouts(self.raw(), btree, new_pos, &mut list) };
        self.result(ret)
    }

    /// @btree is a bitset btree.
    pub fn bit_mod(&self, btree: c::btree_id, pos: bpos, set: bool) -> Result<(), BchError> {
        let ret = unsafe { c::bch2_btree_bit_mod(self.raw(), btree, pos, set) };
        self.result(ret)
    }

    /// Point @iter, intent locked, at the first empty slot in @btree between
    /// @start and @end, after the last key: ENOSPC_btree_slot if there's none.
    pub fn bkey_get_empty_slot(
        &self,
        iter:  &mut BtreeIter<'a>,
        btree: c::btree_id,
        start: bpos,
        end:   bpos,
    ) -> Result<(), BchError> {
        let ret = unsafe { c::bch2_bkey_get_empty_slot(self.raw(), iter.raw(), btree, start, end) };
        self.result(ret)
    }

    /// bit_mod(), through the write buffer.
    pub fn bit_mod_buffered(&self, btree: c::btree_id, pos: bpos, set: bool) -> Result<(), BchError> {
        let ret = unsafe { c::bch2_btree_bit_mod_buffered(self.raw(), btree, pos, set) };
        self.result(ret)
    }

    /// bit_mod(), at @iter's position: bch2_btree_bit_mod_iter().
    pub fn bit_mod_iter(&self, iter: &BtreeIter<'a>, set: bool) -> Result<(), BchError> {
        let ret = unsafe { c::bch2_btree_bit_mod_iter(self.raw(), iter.raw(), set) };
        self.result(ret)
    }

    /// @key, for write buffered @btree, through the write buffer: as
    /// bch2_trans_update_buffered(). Copied - into the transaction's journal
    /// entries, or before journal replay's done, an update of its own.
    pub fn update_buffered<V: ?Sized>(&self, btree: c::btree_id, key: &TransBkey<'_, 't, V>)
        -> Result<(), BchError>
    {
        // SAFETY: a key, which is only read
        self.result(unsafe { c::rust_bch2_trans_update_buffered(self.raw(), btree, key.as_ptr()) })
    }

    pub fn insert<V: ?Sized>(
        &self,
        btree: impl Into<u32>,
        key:   TransBkey<'_, 't, V>,
        flags: UpdateTriggerFlags,
    ) -> Result<(), BchError> {
        self.insert_with(btree, key, BtreeIterFlags::empty(), flags)
    }

    /// As insert(), with flags for the iterator the insert goes through, too.
    pub fn insert_with<V: ?Sized>(
        &self,
        btree:      impl Into<u32>,
        key:        TransBkey<'_, 't, V>,
        iter_flags: BtreeIterFlags,
        flags:      UpdateTriggerFlags,
    ) -> Result<(), BchError> {
        let ret = unsafe {
            c::bch2_btree_insert_trans(
                self.raw(),
                c::btree_id::from_raw(btree.into()).expect("invalid btree id"),
                key.as_ptr(),
                c::btree_iter_update_trigger_flags::from_bits_retain(iter_flags.bits() | flags.bits()),
            )
        };
        self.result(ret)
    }

    pub fn insert_nonextent<V: ?Sized>(
        &self,
        btree: impl Into<u32>,
        key:   TransBkey<'_, 't, V>,
        flags: UpdateTriggerFlags,
    ) -> Result<(), BchError> {
        let ret = unsafe {
            c::bch2_btree_insert_nonextent(
                self.raw(),
                c::btree_id::from_raw(btree.into()).expect("invalid btree id"),
                key.as_ptr(),
                key.k().u64s as u32,
                c::btree_iter_update_trigger_flags::from_bits_retain(flags.bits()),
            )
        };
        self.result(ret)
    }

    pub fn delete_at(
        &self,
        iter:  &BtreeIter<'a>,
        flags: UpdateTriggerFlags,
    ) -> Result<(), BchError> {
        let ret = unsafe {
            c::bch2_btree_delete_at(
                self.raw(),
                iter.raw(),
                c::btree_iter_update_trigger_flags::from_bits_retain(flags.bits()),
            )
        };
        self.result(ret)
    }

    /// The pointer to the node at @level of @btree, at @key's position, made
    /// @key, committed: bch2_btree_node_update_key_at_pos().
    pub fn btree_node_update_key_at_pos<V: ?Sized>(&self, btree: c::btree_id, level: u32,
                                                   key: TransBkey<'_, 't, V>) -> Result<(), BchError> {
        // SAFETY: our transaction, @key in memory its attempt may edit
        self.result(unsafe { c::bch2_btree_node_update_key_at_pos(self.raw(), btree, level, key.as_ptr()) })
    }

    pub(crate) fn btree_node_update_key<V: ?Sized>(
        &self,
        iter:          &mut BtreeIter<'a>,
        node:          BtreeNodeRef,
        key:           TransBkey<'_, 't, V>,
        flags:         impl Into<CommitOpts>,
        skip_triggers: bool,
    ) -> Result<(), BchError> {
        let ret = unsafe {
            c::bch2_btree_node_update_key(
                self.raw(),
                iter.raw_mut(),
                node.as_ptr(),
                key.as_ptr(),
                flags.into().bits(),
                skip_triggers,
            )
        };
        self.result(ret)
    }

    /// The pointer at @level of @btree, at @key's position, made @key and
    /// committed, its triggers not run - btree_node_update_key_at_pos(), for
    /// a repair or an injection that does the accounting itself, or none.
    pub fn btree_node_update_key_at_pos_raw<V: ?Sized>(&self, btree: c::btree_id, level: u32,
                                                       key: TransBkey<'_, 't, V>) -> Result<(), BchError> {
        let mut node_iter = BtreeNodeIter::new(self, btree, key.k().p, 0, level - 1, BtreeIterFlags::INTENT);
        let b = node_iter.peek_ref()?.expect("the node a btree node pointer points to");
        self.btree_node_update_key(node_iter.iter(), b, key, CommitFlags::NO_ENOSPC, true)
    }

    /// @node, which @iter is at, rewritten: bch2_btree_node_rewrite().
    pub(crate) fn btree_node_rewrite(
        &self,
        iter:        &mut BtreeIter<'a>,
        node:        BtreeNodeRef,
        target:      u32,
        flags:       impl Into<CommitOpts>,
        write_flags: c::bch_write_flags,
    ) -> Result<(), BchError> {
        // SAFETY: our transaction; @node, held by @iter's path
        self.result(unsafe {
            c::bch2_btree_node_rewrite(self.raw(), iter.raw_mut(), node.as_ptr(), target,
                                       flags.into().to_c(), write_flags)
        })
    }

    pub fn delete(
        &self,
        btree: impl Into<u32>,
        pos:   c::bpos,
        flags: UpdateTriggerFlags,
    ) -> Result<(), BchError> {
        let ret = unsafe {
            c::bch2_btree_delete(
                self.raw(),
                c::btree_id::from_raw(btree.into()).expect("invalid btree id"),
                pos,
                c::btree_iter_update_trigger_flags::from_bits_retain(flags.bits()),
            )
        };
        self.result(ret)
    }

    pub fn iter_traverse(&self, iter: &mut BtreeIter<'a>) -> Result<(), BchError> {
        let ret = unsafe { c::bch2_btree_iter_traverse(iter.raw_mut()) };
        self.result(ret)
    }
}

impl<'a, 't, V: ?Sized> TransBkey<'a, 't, V> {
    pub fn as_ptr(&self) -> *mut c::bkey_i {
        self.ptr.as_ptr()
    }

    /// The key as a @W's, unchecked: whole_unchecked(), arm(), for a @W that
    /// may be any key's.
    ///
    /// # Safety
    /// As whole_unchecked()'s, for a @W that's a BkeyWhole.
    pub(crate) unsafe fn cast<W: ?Sized>(self) -> TransBkey<'a, 't, W> {
        TransBkey { ptr: self.ptr, buf_u64s: self.buf_u64s, t: PhantomData, v: PhantomData }
    }

    /// The key, borrowed from this handle: to arm, or edit, where the caller
    /// keeps the key.
    pub fn reborrow(&mut self) -> TransBkey<'_, 't, V> {
        TransBkey { ptr: self.ptr, buf_u64s: self.buf_u64s, t: PhantomData, v: PhantomData }
    }

    /// Its header - whatever its u64s say, which may be being set.
    pub fn k(&self) -> &c::bkey {
        // SAFETY: the header at the start of our buffer
        unsafe { &(*self.ptr.as_ptr()).k }
    }

    pub fn k_mut(&mut self) -> &mut c::bkey {
        // SAFETY: as k()'s, and we have the buffer mutably
        unsafe { &mut (*self.ptr.as_ptr()).k }
    }

    /// Make it a deletion - KEY_TYPE_deleted, no value - in place, wherever
    /// it's queued. It's no longer a @V, so it's consumed.
    pub fn set_deleted(mut self) {
        let k = self.k_mut();
        k.type_ = c::bch_bkey_type::KEY_TYPE_deleted.0 as u8;
        k.u64s  = c::BKEY_U64s as u8;
    }

    /// Its buffer, all buf_u64s of it - the room the key has, not its u64s.
    pub fn as_u64s(&self) -> &[u64] {
        // SAFETY: our buffer, buf_u64s long
        unsafe { slice::from_raw_parts(self.ptr.as_ptr() as *const u64, self.buf_u64s as usize) }
    }

    pub fn as_mut_u64s(&mut self) -> &mut [u64] {
        // SAFETY: as as_u64s()'s, and we have the buffer mutably
        unsafe { slice::from_raw_parts_mut(self.ptr.as_ptr() as *mut u64, self.buf_u64s as usize) }
    }
}

/// A key of a family - any key's, or a family's - is as long as its header
/// says, within its buffer. Not a typed whole one's: that's read as all of its
/// value, whatever its u64s.
impl<'a, 't, F: 'static, S: State> TransBkey<'a, 't, FamilyVal<F, S>> {
    /// Its value made @val_u64s long, within its buffer. Grown, the new u64s
    /// are zeroes - what fields past an older value read as; shrunk, the u64s
    /// past it are left as they were. NoRoom if the buffer, or a key's u8 of
    /// u64s, can't hold it.
    pub fn set_val_u64s(&mut self, val_u64s: usize) -> Result<(), NoRoom> {
        let need_u64s = BKEY_U64S + val_u64s;
        let have_u64s = (self.buf_u64s as usize).min(u8::MAX.into());
        if need_u64s > have_u64s {
            return Err(NoRoom { need_u64s, have_u64s });
        }

        let u64s = self.k().u64s as usize;
        if need_u64s > u64s {
            self.as_mut_u64s()[u64s..need_u64s].fill(0);
        }
        self.k_mut().u64s = need_u64s as u8;
        Ok(())
    }
}

impl<'a, 't, V: BkeyWhole> TransBkey<'a, 't, V> {
    /// The key, as a @V's: all of its value there, as the typed constructors
    /// make it.
    pub fn key(&self) -> &BkeyI<V> {
        // SAFETY: the buffer holds a whole BkeyI<@V> - whole_unchecked()'s -
        // whatever the header has been set to since, and any bytes are a @V:
        // BkeyWhole
        unsafe { &*self.ptr.as_ptr().cast() }
    }

    /// key(), to edit.
    pub fn key_mut(&mut self) -> &mut BkeyI<V> {
        // SAFETY: as key()'s, and we have the buffer mutably
        unsafe { &mut *self.ptr.as_ptr().cast() }
    }

    pub fn v(&self) -> &V {
        &self.key().v
    }

    pub fn v_mut(&mut self) -> &mut V {
        &mut self.key_mut().v
    }
}

impl<'a, 't> TransBkey<'a, 't> {
    /// A key C allocated in @t's transaction memory, sized by its own u64s -
    /// what a C helper returning a struct bkey_i * in trans mem hands back.
    ///
    /// # Safety
    /// @ptr is such a key, or an ERR_PTR().
    pub(crate) unsafe fn from_raw(_t: &TransAttempt<'a, 't>, ptr: *mut c::bkey_i)
        -> Result<Self, BchError>
    {
        let ptr = errptr_to_result(NonNull::new(ptr).expect("a trans mem key, or an error"))?;
        Ok(TransBkey {
            buf_u64s: unsafe { ptr.as_ref().k.u64s as u32 },
            ptr,
            t:        PhantomData,
            v:        PhantomData,
        })
    }

    /// A key C has with room past its own u64s - @buf_u64s long, for what
    /// grows it: in @t's memory, or a trigger's update's.
    ///
    /// # Safety
    /// @ptr is a key valid and the caller's to edit while @t's attempt runs,
    /// and @buf_u64s u64s there are its.
    pub(crate) unsafe fn from_raw_room(_t: &TransAttempt<'a, 't>, ptr: *mut c::bkey_i, buf_u64s: u32) -> Self {
        let ptr = NonNull::new(ptr).expect("a key with room");
        assert!(buf_u64s >= unsafe { ptr.as_ref().k.u64s }.into(),
                "a key's buffer of {buf_u64s} u64s, shorter than it is");
        TransBkey { buf_u64s, ptr, t: PhantomData, v: PhantomData }
    }

    /// The key as a @V's, whole, unchecked.
    ///
    /// # Safety
    /// It's of @V's type, and the buffer holds a whole BkeyI<@V>.
    pub(crate) unsafe fn whole_unchecked<V: BkeyWhole>(self) -> TransBkey<'a, 't, V> {
        debug_assert!(self.k().is::<V>());
        debug_assert!(self.buf_u64s as usize * size_of::<u64>() >= size_of::<BkeyI<V>>());
        // SAFETY: the caller's
        unsafe { self.cast() }
    }
}

/// A key in transaction memory, as a form of key like the others.
impl<'a, 't, V: ?Sized + BkeyVal> BkeyV for TransBkey<'a, 't, V> {
    type V = V;
    type As<W: ?Sized + BkeyVal + 'static> = TransBkey<'a, 't, W>;

    fn as_sc(&self) -> BkeySC<'_, V> {
        BkeySC::from(self)
    }

    unsafe fn arm_unchecked<K: BkeyFamily + ?Sized>(self) -> TransBkey<'a, 't, Narrowed<K, V>> {
        // SAFETY: the caller's: a @K, as long as its u64s say, in our buffer
        self.cast()
    }
}

impl<V: ?Sized + BkeyVal> BkeyVMut for TransBkey<'_, '_, V> {
    fn as_s(&mut self) -> BkeyS<'_, V> {
        BkeyS::from(self)
    }
}

/// The key, to read, keeping its type.
impl<'k, V: ?Sized + BkeyVal> From<&'k TransBkey<'_, '_, V>> for BkeySC<'k, V> {
    fn from(k: &'k TransBkey<'_, '_, V>) -> Self {
        let p = k.ptr.as_ptr();
        // SAFETY: the key in our buffer, its value after its header
        BkeySC { k: unsafe { &(*p).k }, v: unsafe { &raw const (*p).v }, iter: PhantomData, val: PhantomData }
    }
}

/// The key, to edit, keeping its type.
impl<'k, V: ?Sized + BkeyVal> From<&'k mut TransBkey<'_, '_, V>> for BkeyS<'k, V> {
    fn from(k: &'k mut TransBkey<'_, '_, V>) -> Self {
        let p = k.ptr.as_ptr();
        // SAFETY: as BkeySC's, and we have the buffer mutably
        BkeyS { k: unsafe { &mut (*p).k }, v: unsafe { &raw mut (*p).v }, val: PhantomData }
    }
}

/// The key, all of it, as long as its u64s say - a validated key's, as
/// BkeyI's AsRef. Its length is its u64s when it's taken: one that changes
/// them - grows the key, in its buffer - takes it again after.
impl AsRef<BkeyI> for TransBkey<'_, '_> {
    fn as_ref(&self) -> &BkeyI {
        // SAFETY: a key in our buffer, which the pointer reaches all of
        unsafe { &*cast_unsized(self.ptr.as_ptr()) }
    }
}

impl AsMut<BkeyI> for TransBkey<'_, '_> {
    fn as_mut(&mut self) -> &mut BkeyI {
        // SAFETY: as as_ref()'s, and we have the buffer mutably
        unsafe { &mut *cast_unsized_mut(self.ptr.as_ptr()) }
    }
}

impl<V: ?Sized + InFamily<AllTypes> + BkeyVal<State: Implies<Validated>>> AsRef<BkeyI>
    for TransBkey<'_, '_, V>
{
    fn as_ref(&self) -> &BkeyI {
        self.into()
    }
}

impl<V: ?Sized + InFamily<AllTypes> + BkeyVal<State: Implies<Validated>>> AsMut<BkeyI>
    for TransBkey<'_, '_, V>
{
    fn as_mut(&mut self) -> &mut BkeyI {
        self.into()
    }
}

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct BtreeIterFlags: u32 {
        const SLOTS = c::btree_iter_update_trigger_flags::BTREE_ITER_slots.bits();
        const PREV = c::btree_iter_update_trigger_flags::BTREE_ITER_prev.bits();
        const INTENT = c::btree_iter_update_trigger_flags::BTREE_ITER_intent.bits();
        const PREFETCH = c::btree_iter_update_trigger_flags::BTREE_ITER_prefetch.bits();
        const IS_EXTENTS = c::btree_iter_update_trigger_flags::BTREE_ITER_is_extents.bits();
        const NOT_EXTENTS = c::btree_iter_update_trigger_flags::BTREE_ITER_not_extents.bits();
        const CACHED = c::btree_iter_update_trigger_flags::BTREE_ITER_cached.bits();
        const KEY_CACHED = c::btree_iter_update_trigger_flags::BTREE_ITER_with_key_cache.bits();
        const WITH_JOURNAL = c::btree_iter_update_trigger_flags::BTREE_ITER_with_journal.bits();
        const SNAPSHOT_FIELD = c::btree_iter_update_trigger_flags::BTREE_ITER_snapshot_field.bits();
        const ALL_SNAPSHOTS = c::btree_iter_update_trigger_flags::BTREE_ITER_all_snapshots.bits();
        const FILTER_SNAPSHOTS = c::btree_iter_update_trigger_flags::BTREE_ITER_filter_snapshots.bits();
        const NOFILTER_WHITEOUTS = c::btree_iter_update_trigger_flags::BTREE_ITER_nofilter_whiteouts.bits();
        const NOPRESERVE = c::btree_iter_update_trigger_flags::BTREE_ITER_nopreserve.bits();
        const NOFILL = c::btree_iter_update_trigger_flags::BTREE_ITER_nofill.bits();
        const CACHED_NOFILL = c::btree_iter_update_trigger_flags::BTREE_ITER_cached_nofill.bits();
        const KEY_CACHE_FILL = c::btree_iter_update_trigger_flags::BTREE_ITER_key_cache_fill.bits();
        const COMMITTED = c::btree_iter_update_trigger_flags::BTREE_ITER_committed.bits();
        /// Extents: a peek returns its key cut to the window - [position, end),
        /// or backwards [min, position) - the iterator's key with it, so a walk
        /// steps to the window's edge at most, where it ends. An update of
        /// what's returned replaces only the window.
        const TRIM = c::btree_iter_update_trigger_flags::BTREE_ITER_trim.bits();
    }
}

bitflags! {
    /// The BTREE_UPDATE_* and BTREE_TRIGGER_* half of C's
    /// btree_iter_update_trigger_flags: how an update commits, and which
    /// triggers it runs.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct UpdateTriggerFlags: u32 {
        const INTERNAL_SNAPSHOT_NODE   = c::btree_iter_update_trigger_flags::BTREE_UPDATE_internal_snapshot_node.bits();
        const NOJOURNAL                = c::btree_iter_update_trigger_flags::BTREE_UPDATE_nojournal.bits();
        const KEY_CACHE_RECLAIM        = c::btree_iter_update_trigger_flags::BTREE_UPDATE_key_cache_reclaim.bits();
        const NORUN                    = c::btree_iter_update_trigger_flags::BTREE_TRIGGER_norun.bits();
        const TRANSACTIONAL            = c::btree_iter_update_trigger_flags::BTREE_TRIGGER_transactional.bits();
        const ATOMIC                   = c::btree_iter_update_trigger_flags::BTREE_TRIGGER_atomic.bits();
        const GC                       = c::btree_iter_update_trigger_flags::BTREE_TRIGGER_gc.bits();
        const INSERT                   = c::btree_iter_update_trigger_flags::BTREE_TRIGGER_insert.bits();
        const OVERWRITE                = c::btree_iter_update_trigger_flags::BTREE_TRIGGER_overwrite.bits();
        const IS_DISCARD               = c::btree_iter_update_trigger_flags::BTREE_TRIGGER_is_discard.bits();
        const SET_NEEDS_RECONCILE_DONE = c::btree_iter_update_trigger_flags::BTREE_TRIGGER_set_needs_reconcile_done.bits();
    }
}

bitflags! {
    /// The bits of bch_trans_commit_flags above the watermark.
    pub struct CommitFlags: u32 {
        const NO_ENOSPC             = c::bch_trans_commit_flags::BCH_TRANS_COMMIT_no_enospc.bits();
        const NO_CHECK_RW           = c::bch_trans_commit_flags::BCH_TRANS_COMMIT_no_check_rw.bits();
        const NO_JOURNAL_RES        = c::bch_trans_commit_flags::BCH_TRANS_COMMIT_no_journal_res.bits();
        const NO_SKIP_NOOPS         = c::bch_trans_commit_flags::BCH_TRANS_COMMIT_no_skip_noops.bits();
        const JOURNAL_RECLAIM       = c::bch_trans_commit_flags::BCH_TRANS_COMMIT_journal_reclaim.bits();
        const JOURNAL_REPLAY        = c::bch_trans_commit_flags::BCH_TRANS_COMMIT_journal_replay.bits();
        const SKIP_ACCOUNTING_APPLY = c::bch_trans_commit_flags::BCH_TRANS_COMMIT_skip_accounting_apply.bits();
    }
}

/// The low bits of the commit flags: how deep into the reserves the commit may
/// dip. Unset - stripe, 0 - is normal for the ENOSPC gate, so it usually isn't
/// set.
#[derive(Clone, Copy)]
pub struct Watermark(c::bch_watermark);

impl Watermark {
    pub const STRIPE:           Self = Watermark(c::bch_watermark::BCH_WATERMARK_stripe);
    pub const NORMAL:           Self = Watermark(c::bch_watermark::BCH_WATERMARK_normal);
    pub const COPYGC:           Self = Watermark(c::bch_watermark::BCH_WATERMARK_copygc);
    pub const BTREE:            Self = Watermark(c::bch_watermark::BCH_WATERMARK_btree);
    pub const BTREE_COPYGC:     Self = Watermark(c::bch_watermark::BCH_WATERMARK_btree_copygc);
    pub const RECLAIM:          Self = Watermark(c::bch_watermark::BCH_WATERMARK_reclaim);
    pub const INTERIOR_UPDATES: Self = Watermark(c::bch_watermark::BCH_WATERMARK_interior_updates);
}

/// A Watermark and CommitFlags, C's one word. The commit functions take
/// `impl Into<CommitOpts>`, so CommitFlags alone do, and a CommitOpts is only
/// for a watermark:
///
/// `t.commit(None, CommitFlags::NO_ENOSPC)`
#[derive(Clone, Copy, Default)]
pub struct CommitOpts(u32);

impl CommitOpts {
    /// Mask of the watermark bits: everything below the lowest flag bit.
    const WATERMARK_MASK: u32 = CommitFlags::NO_ENOSPC.bits() - 1;

    pub const fn new() -> Self {
        CommitOpts(0)
    }

    pub const fn flags(self, flags: CommitFlags) -> Self {
        CommitOpts(self.0 | flags.bits())
    }

    pub const fn watermark(self, w: Watermark) -> Self {
        CommitOpts((self.0 & !Self::WATERMARK_MASK) | (w.0 as u32 & Self::WATERMARK_MASK))
    }

    pub(crate) const fn bits(self) -> u32 {
        self.0
    }

    /// A C caller's flags and watermark, as one unsigned.
    pub(crate) const fn from_bits(bits: u32) -> Self {
        CommitOpts(bits)
    }

    pub(crate) const fn to_c(self) -> c::bch_trans_commit_flags {
        c::bch_trans_commit_flags::from_bits_retain(self.0)
    }
}

impl From<CommitFlags> for CommitOpts {
    fn from(flags: CommitFlags) -> Self {
        CommitOpts::new().flags(flags)
    }
}

pub fn lockrestart_do<'t, T, F>(trans: &BtreeTrans<'t>, mut f: F) -> Result<T, BchError>
where
    F: for<'a> FnMut(&TransAttempt<'a, 't>) -> Result<T, BchError>
{
    loop {
        let t = trans.begin();

        match f(&t) {
            Err(e) if is_restart(&e) => continue,
            Err(e) => return Err(e),
            Ok(v) => {
                t.verify_not_restarted();
                return Ok(v);
            }
        }
    }
}

/// lockrestart_do(), committing after @f.
pub fn commit_do<'t, F>(
    trans: &BtreeTrans<'t>,
    disk_res: Option<&DiskReservation<'_>>,
    flags: impl Into<CommitOpts>,
    mut f: F,
) -> Result<(), BchError>
where
    F: for<'a> FnMut(&TransAttempt<'a, 't>) -> Result<(), BchError>,
{
    let flags = flags.into();
    lockrestart_do(trans, |t| {
        // If @f fails, the commit - which resets the updates whether it
        // succeeds or fails - never runs. An error that isn't a restart
        // abandons the attempt: drop what @f queued, as the commit would
        // have, so the next begin doesn't take them for updates someone
        // forgot to commit. A restart's are dropped by the restart. As C's
        // __commit_do().
        if let Err(e) = f(t) {
            if !is_restart(&e) {
                t.reset_updates();
            }
            return Err(e);
        }
        t.commit(disk_res, flags)
    })
}

/// commit_do() in a transaction of its own.
pub fn trans_commit_do<'t, F>(
    fs: &'t Fs,
    disk_res: Option<&DiskReservation<'_>>,
    flags: impl Into<CommitOpts>,
    f: F,
) -> Result<(), BchError>
where
    F: for<'a> FnMut(&TransAttempt<'a, 't>) -> Result<(), BchError>,
{
    let trans = crate::btree_trans!(fs);
    commit_do(&trans, disk_res, flags, f)
}

/// lockrestart_do() in a transaction of its own.
pub fn trans_run<'t, T, F>(fs: &'t Fs, f: F) -> Result<T, BchError>
where
    F: for<'a> FnMut(&TransAttempt<'a, 't>) -> Result<T, BchError>,
{
    let trans = crate::btree_trans!(fs);
    lockrestart_do(&trans, f)
}

/// @k as a @K, its value in place, if it's one: ENOENT_bkey_type_mismatch if
/// it isn't.
fn bkey_arm<'k, K: BkeyFamily + ?Sized>(fs: &Fs, k: BkeySC<'k>) -> Result<BkeySC<'k, K::Validated>, BchError> {
    match k.arm::<K>() {
        Some(k) => Ok(k),
        None    => fs.throw(bch_errcode::BCH_ERR_ENOENT_bkey_type_mismatch),
    }
}

/*
 * The iterator's own: what C has as static inlines, in Rust for Rust - and
 * the raw peeks, which the loops below wrap.
 */
impl BtreeIter<'_> {
    pub(crate) fn iter_flags(&self) -> BtreeIterFlags {
        BtreeIterFlags::from_bits_retain(self.flags)
    }

    /// Its path isn't wanted after this: not preserved, and - if only this
    /// iterator holds it - not needing to stay locked.
    /// bch2_set_btree_iter_dontneed().
    pub fn set_dontneed(&mut self) {
        unsafe { c::bch2_set_btree_iter_dontneed(self.raw()) }
    }

    /// Move to @pos - in the iterator's own snapshot, unless it's
    /// all_snapshots - with no key there yet: the next peek finds it. Drops the
    /// update path, which was for the old position. The static inline
    /// bch2_btree_iter_set_pos().
    pub fn set_pos(&mut self, mut pos: c::bpos) {
        if self.update_path != 0 {
            let intent = self.iter_flags().contains(BtreeIterFlags::INTENT);
            unsafe { c::bch2_path_put(self.trans, self.update_path, intent) };
        }
        self.update_path = 0;

        if !self.iter_flags().contains(BtreeIterFlags::ALL_SNAPSHOTS) {
            pos.snapshot = self.snapshot;
        }

        self.k.type_ = c::bch_bkey_type::KEY_TYPE_deleted.0 as u8;
        self.k.p     = pos;
        self.k.size  = 0;
        self.pos     = pos;
    }

    /// The next key up to @end, walking as @flags say: backwards with prev, a
    /// slot at a time - holes included - with slots. The static inline
    /// bch2_btree_iter_peek_max_type().
    fn peek_max_type_raw(&mut self, end: c::bpos, flags: BtreeIterFlags) -> c::bkey_s_c {
        let slots = flags.contains(BtreeIterFlags::SLOTS);
        let prev  = flags.contains(BtreeIterFlags::PREV);
        let trim  = self.iter_flags().contains(BtreeIterFlags::TRIM | BtreeIterFlags::IS_EXTENTS);

        unsafe {
            match (prev, slots) {
                (false, false) => c::bch2_btree_iter_peek_max(self.raw_mut(), &end),
                (true,  false) => c::bch2_btree_iter_peek_prev_min(self.raw_mut(), end),
                (true,  true)  =>
                    if bkey_lt(self.pos, end) { c::bkey_s_c::default() }
                    else { self.peek_prev_type_raw(flags) },
                // trimming, a slot is cut to the window, and an extents window
                // ends where @end starts: the slot there is past it
                (false, true) if trim =>
                    if bkey_ge(self.pos, end) { c::bkey_s_c::default() }
                    else { c::bch2_btree_iter_peek_slot_max(self.raw_mut(), end) },
                (false, true) =>
                    if bkey_gt(self.pos, end) { c::bkey_s_c::default() }
                    else { c::bch2_btree_iter_peek_slot(self.raw_mut()) },
            }
        }
    }

    /// The previous key, or with slots the slot here: the static inline
    /// bch2_btree_iter_peek_prev_type().
    fn peek_prev_type_raw(&mut self, flags: BtreeIterFlags) -> c::bkey_s_c {
        unsafe {
            if flags.contains(BtreeIterFlags::SLOTS) {
                // reverse slots aren't defined for extents: peek_slot() is the slot from @pos on
                assert!(!self.iter_flags().contains(BtreeIterFlags::IS_EXTENTS));
                c::bch2_btree_iter_peek_slot(self.raw_mut())
            } else {
                c::bch2_btree_iter_peek_prev_min(self.raw_mut(), POS_MIN)
            }
        }
    }

    /// For C that takes the iterator without moving it - an update, which
    /// may set key_cache_path, its Cell.
    pub(crate) fn raw(&self) -> *mut c::btree_iter {
        core::ptr::from_ref(self).cast_mut().cast()
    }

    pub(crate) fn raw_mut(&mut self) -> *mut c::btree_iter {
        core::ptr::from_mut(self).cast()
    }
}

/// XXX polonius: NLL problem case #3. A loop that returns the key it stops
/// at, borrowed from its iterator, and advances otherwise:
///
/// ```ignore
/// let (iter, k) = self.peek_with(..)?;
/// if f(iter, k)? { return Ok(Some(k)) }
/// self.advance();
/// ```
///
/// is sound - the key's borrow is dead on the path that advances - but NLL
/// sees a borrow returned on one path as live on all of them, and rejects
/// it; Polonius accepts it (nightly does, as of 2026-10). Until it's
/// stable, the loop returns the key through this, which gives it the
/// lifetime the loop's caller borrowed the iterator for.
///
/// The same goes for a function that returns a key it found on one path,
/// and goes on to use the iterator on another.
///
/// Delete this once Polonius is stable: each use becomes a plain `k`.
///
/// # Safety
/// The iterator @k borrows mustn't be touched again before the caller
/// returns @k - which ends its use of it.
pub(crate) unsafe fn polonius_key<'i, V: ?Sized + BkeyVal>(k: BkeySC<'_, V>) -> BkeySC<'i, V> {
    BkeySC { k: unsafe { &*(k.k as *const c::bkey) }, v: k.v, iter: PhantomData, val: PhantomData }
}

/// Btrees walked in parallel, by position: each of @iters peeked from where it
/// is, and the positions they're at compared - each as @to_walk has it, for a
/// btree the walk keys apart from the others - giving where the walk is, the
/// least of them, and each iterator with its key there: None for one that's
/// ahead. The keys are borrowed from their iterators, shared, as the
/// iterators are from here.
pub fn peek_parallel<'k, 't, const N: usize>(
    _t:      &'k TransAttempt<'_, 't>,
    mut iters: [&'k mut BtreeIter<'t>; N],
    to_walk:   impl Fn(usize, &BtreeIter<'t>) -> bpos,
) -> Result<(bpos, [(&'k BtreeIter<'t>, Option<BkeySC<'k>>); N]), BchError>
{
    // Stop at the first error: it may be a restart, and nothing may peek in a
    // transaction that's restarting
    let mut keys: [Option<BkeySC<'k>>; N] = [None; N];
    for (iter, k) in iters.iter_mut().zip(&mut keys) {
        // SAFETY: the key, valid while the iterator's borrowed - shared, from
        // here, within the attempt
        *k = unsafe { bkey_s_c_to_result::<'k>(c::bch2_btree_iter_peek_max(iter.raw_mut(), &SPOS_MAX)) }?;
    }
    let iters = iters.map(|iter| -> &'k BtreeIter<'t> { iter });

    let pos: [bpos; N] = core::array::from_fn(|i| to_walk(i, iters[i]));
    let at = pos.iter().copied().reduce(bpos_min).expect("an iterator to walk");

    Ok((at, core::array::from_fn(|i| (iters[i], if bpos_lt(at, pos[i]) { None } else { keys[i] }))))
}

/// A key C returned - a peek, a lookup - or its error.
///
/// # Safety
/// The key's lifetime is whatever the caller says: 'i must be no longer than
/// the key is valid - a borrow of the iterator it came through, during which
/// nothing moves that iterator. Callers tie it to one in their signature.
pub(crate) unsafe fn bkey_s_c_to_result<'i>(k: c::bkey_s_c)
    -> Result<Option<BkeySC<'i>>, BchError>
{
    // NULL past the end
    Ok(errptr_or_null(k.k.cast_mut())?.map(|kp| unsafe {
        BkeySC {
            k:    kp.as_ref(),
            // Hole slots (peek_slot) return a deleted key with a NULL val: a
            // dangling, aligned one instead, as val_bytes() makes a slice of
            // it - empty, a deleted key's value, but a slice's pointer is
            // never null.
            v:    if !k.v.is_null() { k.v } else { NonNull::dangling().as_ptr() },
            iter: PhantomData,
            val:  PhantomData,
        }
    }))
}

impl<'t> BtreeIter<'t> {
    pub fn uninit() -> BtreeIter<'t> {
        // SAFETY: all zeroes is an iterator with no paths - whose exit, its
        // drop, puts none
        unsafe { core::mem::zeroed() }
    }

    /// The key pointing to @btree's root - none, if its root's below @level -
    /// with this iterator initialized at it, and given back shared with the
    /// key: bch2_btree_iter_peek_root(), for for_btree_root_key_at_level().
    pub fn peek_root<'i>(&'i mut self, t: &TransAttempt<'_, 't>, btree: c::btree_id, level: u32)
        -> Result<Option<(&'i BtreeIter<'t>, BkeySC<'i>)>, BchError>
    {
        // SAFETY: the key, in the iterator's path, for as long as it's borrowed
        let k = unsafe { bkey_s_c_to_result::<'i>(c::bch2_btree_iter_peek_root(t.raw(), self.raw_mut(), btree, level)) }?;
        Ok(k.map(|k| (&*self, k)))
    }

    /// The node the iterator's path holds at its level, if it has one - a
    /// level without one holds NULL or an error pointer, as IS_ERR_OR_NULL().
    pub(crate) fn node_at_iter_level<'a>(
        &self,
        t: &TransAttempt<'a, 't>,
    ) -> Option<BtreeNodeRef> {
        unsafe {
            let path = (*t.raw()).paths.add(self.path as usize);
            let node = (*path).l[(*path).level() as usize].b;

            errptr_or_null(node).ok()?.map(|raw| BtreeNodeRef { raw })
        }
    }

    /// btree_iter_path()->l[0]: None with no node there.
    ///
    /// # Safety
    /// The transaction doesn't unlock while it's used - the borrow of @t
    /// doesn't see to that, as unlock() takes &self.
    pub unsafe fn leaf<'a>(&self, t: &'a TransAttempt<'_, 't>) -> Option<PathLeaf<'a>> {
        unsafe {
            let path = (*t.raw()).paths.add(self.path as usize);
            let l = &raw mut (*path).l[0];
            // NULL above the root, or an error pointer
            errptr_or_null((*l).b).ok()??;
            Some(PathLeaf { l, _trans: PhantomData })
        }
    }

    pub fn pos(&self) -> c::bpos {
        self.pos
    }

    /// @k is a key at the iterator's level, above the leaves.
    pub fn prefetch_child(&self, t: &TransAttempt<'_, 't>, k: &BkeyI) -> Result<(), BchError> {
        let ret = unsafe {
            let path = (*t.raw()).paths.add(self.path as usize);
            assert!((*path).level() != 0, "prefetching below the leaves");
            c::bch2_btree_node_prefetch(t.raw(), path, k.as_ffi(), (*path).btree_id(),
                                        u32::from((*path).level()) - 1)
        };
        t.result(ret)
    }

    /// For an extents iterator: back to the start of the key it's at.
    pub fn set_pos_to_extent_start(&mut self) {
        debug_assert!(self.is_extents());
        self.pos = self.k.start_pos();
    }

    /// Look in @snapshot, at the same position.
    pub fn set_snapshot(&mut self, snapshot: u32) {
        self.snapshot = snapshot;
        self.set_pos(c::bpos { snapshot, ..self.pos });
    }

    pub fn snapshot(&self) -> u32 {
        self.snapshot
    }

    /// Whether it walks extents - keys covering a range, ending at their
    /// position.
    pub fn is_extents(&self) -> bool {
        self.iter_flags().contains(BtreeIterFlags::IS_EXTENTS)
    }

    /// Turn on @flags: for walking an iterator with flags it wasn't made
    /// with - the loops peek with the iterator's own - as C's _continue
    /// loops pass theirs.
    pub fn set_flags(&mut self, flags: BtreeIterFlags) {
        self.flags |= flags.bits();
    }

    /// Turn off @flags: for an iterator going on with flags it was made
    /// without.
    pub fn clear_flags(&mut self, flags: BtreeIterFlags) {
        self.flags &= !flags.bits();
    }

    /// A second iterator at the same position, sharing this one's paths.
    pub fn copy(&self) -> BtreeIter<'t> {
        unsafe {
            let mut iter = MaybeUninit::<BtreeIter<'t>>::uninit();

            c::bch2_trans_copy_iter(iter.as_mut_ptr().cast(), self.raw());

            iter.assume_init()
        }
    }

    pub fn new(
        trans: impl TransRef<'t>,
        btree: impl Into<u32>,
        pos: bpos,
        flags: BtreeIterFlags,
    ) -> BtreeIter<'t> {
        unsafe {
            let mut iter = MaybeUninit::<BtreeIter<'t>>::uninit();

            c::bch2_trans_iter_init_outlined(
                trans.trans_raw(),
                iter.as_mut_ptr().cast(),
                c::btree_id::from_raw(btree.into()).expect("invalid btree id"),
                pos,
                c::btree_iter_update_trigger_flags::from_bits_retain(flags.bits()),
                0
            );

            iter.assume_init()
        }
    }

    pub fn new_level(
        trans: impl TransRef<'t>,
        btree: impl Into<u32>,
        pos: bpos,
        level: u32,
        flags: BtreeIterFlags,
    ) -> BtreeIter<'t> {
        unsafe {
            let mut iter = MaybeUninit::<BtreeIter<'t>>::uninit();

            c::__bch2_trans_node_iter_init(
                trans.trans_raw(),
                iter.as_mut_ptr().cast(),
                c::btree_id::from_raw(btree.into()).expect("invalid btree id"),
                pos,
                0,
                level,
                c::btree_iter_update_trigger_flags::from_bits_retain(flags.bits())
            );

            iter.assume_init()
        }
    }

    // ── Peeking ──────────────────────────────────────────────────────────
    //
    // A key peeked borrows the iterator - its header is in it, iter->k, and
    // its value in the node the iterator's path holds - and the attempt it
    // was peeked in, @t: the next trans_begin() unlocks, and it's only the
    // loop driving the attempt that begins again.
    //
    // Not expressed: a key is also stale after a commit, which changes the
    // leaf nodes it wrote in place - a rule about order within the attempt,
    // as "no work after a restart" is.

    /// Peek with the C function @peek, under attempt @t.
    fn peek_raw<'k>(&'k mut self, _t: &'k TransAttempt<'_, 't>,
                    peek: impl FnOnce(*mut c::btree_iter) -> c::bkey_s_c)
        -> Result<Option<BkeySC<'k>>, BchError>
    {
        // 'k: the key is valid while self is borrowed, within the attempt
        unsafe { bkey_s_c_to_result::<'k>(peek(self.raw_mut())) }
    }

    pub fn peek_max<'k>(&'k mut self, t: &'k TransAttempt<'_, 't>, end: bpos)
        -> Result<Option<BkeySC<'k>>, BchError>
    {
        self.peek_raw(t, |raw| unsafe { c::bch2_btree_iter_peek_max(raw, &end) })
    }

    /// At the iterator's level - new_level()'s; None if the btree isn't that deep.
    pub fn peek_node<'k>(&'k mut self, _t: &'k TransAttempt<'_, 't>)
        -> Result<Option<&'k c::btree>, BchError>
    {
        // SAFETY: the node is locked by the iterator's path, while self is
        // borrowed, within the attempt
        unsafe {
            let b = c::bch2_btree_iter_peek_node(self.raw_mut());
            // NULL past the last
            Ok(errptr_or_null(b)?.map(|b| b.as_ref()))
        }
    }

    /// The key at the iterator's position, or a deleted key for a hole.
    pub fn peek_slot<'k>(&'k mut self, t: &'k TransAttempt<'_, 't>)
        -> Result<Option<BkeySC<'k>>, BchError>
    {
        self.peek_raw(t, |raw| unsafe { c::bch2_btree_iter_peek_slot(raw) })
    }

    /// peek_slot() with an end, past the position: for extents a hole ends
    /// there at the latest, and with TRIM the key is cut to [position, @end).
    pub fn peek_slot_max<'k>(&'k mut self, t: &'k TransAttempt<'_, 't>, end: bpos)
        -> Result<Option<BkeySC<'k>>, BchError>
    {
        self.peek_raw(t, |raw| unsafe { c::bch2_btree_iter_peek_slot_max(raw, end) })
    }

    /// Back one position, and the key there, or a deleted key for a hole -
    /// None if there's nowhere further back to go.
    pub fn prev_slot<'k>(&'k mut self, t: &'k TransAttempt<'_, 't>)
        -> Result<Option<BkeySC<'k>>, BchError>
    {
        self.peek_raw(t, |raw| unsafe { c::bch2_btree_iter_prev_slot(raw) })
    }

    /// The key here as a @K, its value in place: ENOENT_bkey_type_mismatch if
    /// it isn't one.
    pub fn peek_slot_typed<'k, K: BkeyFamily + ?Sized>(&'k mut self, t: &'k TransAttempt<'_, 't>)
        -> Result<BkeySC<'k, K::Validated>, BchError>
    {
        let k = self.peek_slot(t)?.expect("a slot always has a key");
        bkey_arm::<K>(t.fs(), k)
    }

    /// As peek_slot(), for a key to update through this iterator: the
    /// iterator comes back shared, for as long as the key is used, as
    /// peek_slot_typed_shared().
    pub fn peek_slot_shared<'k>(&'k mut self, _t: &'k TransAttempt<'_, 't>)
        -> Result<(&'k Self, BkeySC<'k>), BchError>
    {
        let this: &'k Self = self;
        // 'k: the key is valid while the iterator is borrowed, within the
        // attempt - shared, it can't be moved or peeked again
        let k = unsafe { bkey_s_c_to_result::<'k>(c::bch2_btree_iter_peek_slot(this.raw()))? };
        Ok((this, k.expect("a slot always has a key")))
    }

    /// As peek_slot_typed(), for a key to update through this iterator: the
    /// iterator comes back shared, for as long as the key is used - as the
    /// for_each loops pass it - so the key can't be moved out from under.
    pub fn peek_slot_typed_shared<'k, K: BkeyFamily + ?Sized>(&'k mut self, t: &'k TransAttempt<'_, 't>)
        -> Result<(&'k Self, BkeySC<'k, K::Validated>), BchError>
    {
        let this: &'k Self = self;
        // 'k: the key is valid while the iterator is borrowed, within the
        // attempt - shared, it can't be moved or peeked again
        let k = unsafe { bkey_s_c_to_result::<'k>(c::bch2_btree_iter_peek_slot(this.raw()))? };
        Ok((this, bkey_arm::<K>(t.fs(), k.expect("a slot always has a key"))?))
    }

    pub fn peek_max_flags<'k>(&'k mut self, t: &'k TransAttempt<'_, 't>, end: bpos,
                                      flags: BtreeIterFlags)
        -> Result<Option<BkeySC<'k>>, BchError>
    {
        if !flags.contains(BtreeIterFlags::SLOTS) {
            self.peek_max(t, end)
        } else if bkey_le(self.pos(), end) {
            self.peek_slot(t)
        } else {
            Ok(None)
        }
    }

    pub fn peek_max_type<'k>(&'k mut self, t: &'k TransAttempt<'_, 't>, end: bpos,
                                     flags: BtreeIterFlags)
        -> Result<Option<BkeySC<'k>>, BchError>
    {
        self.peek_raw(t, |raw| unsafe { (*raw).peek_max_type_raw(end, flags) })
    }

    pub fn peek<'k>(&'k mut self, t: &'k TransAttempt<'_, 't>)
        -> Result<Option<BkeySC<'k>>, BchError>
    {
        self.peek_max(t, SPOS_MAX)
    }

    pub fn peek_prev_min<'k>(&'k mut self, t: &'k TransAttempt<'_, 't>, min: bpos)
        -> Result<Option<BkeySC<'k>>, BchError>
    {
        self.peek_raw(t, |raw| unsafe { c::bch2_btree_iter_peek_prev_min(raw, min) })
    }

    pub fn peek_prev<'k>(&'k mut self, t: &'k TransAttempt<'_, 't>)
        -> Result<Option<BkeySC<'k>>, BchError>
    {
        self.peek_prev_min(t, c::bpos { inode: 0, offset: 0, snapshot: 0 })
    }

    pub fn traverse(&mut self, t: &TransAttempt<'_, 't>) -> Result<(), BchError> {
        let ret = unsafe { c::bch2_btree_iter_traverse(self.raw_mut()) };
        t.result(ret)
    }

    /// The loop behind the for_each family: get a key with @peek, hand it to
    /// @f, move on with @step.
    ///
    /// @f's result is two things, kept apart: whether its work succeeded
    /// (`Result`), and whether to go on (LoopControl: `()` for a body that
    /// always goes on, `ControlFlow` for one that can stop early - what it
    /// breaks with is what the loop returns). A transaction restart, from
    /// the peek or from @f, retries the same position; any other error is
    /// returned.
    ///
    /// @f runs in the attempt begun for its key, and gets it: what it does
    /// beyond reading the key - a lookup, an update - is in that attempt.
    ///
    /// @f gets the iterator too, as the C loop bodies do - to update at its
    /// position, or read where it is - shared, borrowed with the key: the
    /// key's header is in the iterator, and its value in the node the
    /// iterator's path holds, so nothing may move the iterator while the key
    /// is in use. A body that needs to move it is a loop of its own.
    fn for_each_inner<P, S, F, R>(
        &mut self,
        trans:    &BtreeTrans<'t>,
        mut peek: P,
        mut step: S,
        mut f:    F,
    ) -> Result<R::Break, BchError>
    where
        P: FnMut(*mut c::btree_iter) -> c::bkey_s_c,
        S: FnMut(*mut c::btree_iter) -> bool,
        F: for<'a> FnMut(&'a TransAttempt<'_, 't>, &'a BtreeIter<'t>, BkeySC<'a>) -> Result<R, BchError>,
        R: LoopControl,
    {
        loop {
            let t = trans.begin();

            let (iter, k) = match self.peek_with(&t, &mut peek) {
                Err(e) if is_restart(&e) => continue,
                Err(e) => return Err(e),
                Ok(None) => return Ok(Default::default()),
                Ok(Some(ik)) => ik,
            };

            let step_to = match f(&t, iter, k) {
                Err(e) if is_restart(&e) => continue,
                Err(e) => return Err(e),
                Ok(flow) => {
                    t.verify_not_restarted();
                    flow.into_step()
                }
            };

            match step_to {
                Step::Next => if !step(self.raw_mut()) {
                    return Ok(Default::default());
                },
                Step::From(pos) => self.set_pos(pos),
                Step::Break(b)  => return Ok(b),
            }
        }
    }

    /// Peek with @peek, under attempt @t: the key, and the iterator it
    /// borrows, shared - both from the one &mut borrow, so neither outlives
    /// it and nothing moves the iterator while they're in use.
    fn peek_with<'k, P>(&'k mut self, _t: &'k TransAttempt<'_, 't>, peek: &mut P)
        -> Result<Option<(&'k BtreeIter<'t>, BkeySC<'k>)>, BchError>
    where
        P: FnMut(*mut c::btree_iter) -> c::bkey_s_c,
    {
        // 'k: the key is valid while self is borrowed, shared, within the attempt
        let k = unsafe { bkey_s_c_to_result::<'k>(peek(self.raw_mut())) }?;
        Ok(k.map(|k| (&*self, k)))
    }

    /// The next key up to @end, peeking as the iterator's own flags say - for a
    /// SLOTS iterator, a slot at a time, holes included. The for_each family
    /// peeks with this, as C's loops do, so no loop can ignore its iterator's
    /// flags.
    fn peek_own_type(raw: *mut c::btree_iter, end: bpos) -> c::bkey_s_c {
        unsafe {
            let flags = (*raw).iter_flags();
            (*raw).peek_max_type_raw(end, flags)
        }
    }

    pub fn for_each_max<F, R>(&mut self, trans: &BtreeTrans<'t>, end: bpos, f: F)
        -> Result<R::Break, BchError>
    where
        F: for<'a> FnMut(&'a TransAttempt<'_, 't>, &'a BtreeIter<'t>, BkeySC<'a>) -> Result<R, BchError>,
        R: LoopControl,
    {
        self.for_each_inner(trans,
            |raw| Self::peek_own_type(raw, end),
            // advance() returns false when the key just visited ended at
            // SPOS_MAX and the position can't move forward — true for the
            // rightmost key of any interior node level. Looping again would
            // peek the same key forever.
            |raw| unsafe { c::bch2_btree_iter_advance(raw) },
            f)
    }

    /// Walk the keys from the iterator's position, inside the caller's attempt
    /// @t. Unlike the rest of the family this doesn't begin or retry anything -
    /// a restart is returned, for the loop the caller is in.
    pub fn for_each_norestart<F, R>(&mut self, t: &TransAttempt<'_, 't>, f: F)
        -> Result<R::Break, BchError>
    where
        F: for<'a> FnMut(&'a BtreeIter<'t>, BkeySC<'a>) -> Result<R, BchError>,
        R: LoopControl,
    {
        self.for_each_max_norestart(t, SPOS_MAX, f)
    }

    /// As for_each_norestart(), up to and including @end.
    pub fn for_each_max_norestart<F, R>(&mut self, t: &TransAttempt<'_, 't>, end: bpos, mut f: F)
        -> Result<R::Break, BchError>
    where
        F: for<'a> FnMut(&'a BtreeIter<'t>, BkeySC<'a>) -> Result<R, BchError>,
        R: LoopControl,
    {
        loop {
            let Some((iter, k)) = self.peek_with(t, &mut |raw| Self::peek_own_type(raw, end))?
            else {
                return Ok(Default::default());
            };

            match f(iter, k)?.into_step() {
                Step::Next => if !self.advance() {
                    return Ok(Default::default());
                },
                Step::From(pos) => self.set_pos(pos),
                Step::Break(b)  => return Ok(b),
            }
        }
    }

    /// As for_each_max_norestart(), stopping at the first key @f picks: that
    /// key, borrowed from the iterator, which is left at it, and no longer
    /// than attempt @t; None if @f picks none. As Iterator::find().
    pub fn find_max_norestart<'i, F>(&'i mut self, t: &'i TransAttempt<'_, 't>, end: bpos,
                                     mut f: F)
        -> Result<Option<BkeySC<'i>>, BchError>
    where
        F: for<'k> FnMut(&'k BtreeIter<'t>, BkeySC<'k>) -> Result<bool, BchError>,
    {
        loop {
            let Some((iter, k)) = self.peek_with(t, &mut |raw| Self::peek_own_type(raw, end))?
            else {
                return Ok(None);
            };

            if f(iter, k)? {
                return Ok(Some(unsafe { polonius_key(k) }));
            }
            if !self.advance() {
                return Ok(None);
            }
        }
    }

    /// As for_each_norestart(), backwards from the iterator's position down to
    /// @min.
    pub fn for_each_reverse_norestart<F, R>(&mut self, t: &TransAttempt<'_, 't>, min: bpos,
                                            mut f: F)
        -> Result<R::Break, BchError>
    where
        F: for<'a> FnMut(&'a BtreeIter<'t>, BkeySC<'a>) -> Result<R, BchError>,
        R: LoopControl,
    {
        loop {
            let Some((iter, k)) =
                self.peek_with(t, &mut |raw| unsafe { c::bch2_btree_iter_peek_prev_min(raw, min) })?
            else {
                return Ok(Default::default());
            };

            match f(iter, k)?.into_step() {
                Step::Next => if !self.rewind() {
                    return Ok(Default::default());
                },
                Step::From(pos) => self.set_pos(pos),
                Step::Break(b)  => return Ok(b),
            }
        }
    }

    pub fn for_each<F, R>(&mut self, trans: &BtreeTrans<'t>, f: F) -> Result<R::Break, BchError>
    where
        F: for<'a> FnMut(&'a TransAttempt<'_, 't>, &'a BtreeIter<'t>, BkeySC<'a>) -> Result<R, BchError>,
        R: LoopControl,
    {
        self.for_each_max(trans, SPOS_MAX, f)
    }

    /// As for_each_inner(), @f getting the attempt, and committing its updates
    /// after each key when @commit says how (None: @f commits for itself). A
    /// restart from @f or the commit retries the key.
    ///
    /// @f finishes a key in one of three ways:
    ///  - Ok(()): commit what it queued, and go on to the next key - also
    ///    how a body finishes a key early, keeping its repairs.
    ///  - Err(fc_continue): go on to the next key *without* committing. What
    ///    was queued is dropped - and bch2_trans_begin()'s dropped-updates
    ///    check reports it - so this is for a body that has queued nothing,
    ///    or wants it discarded.
    ///  - Err(fc_break): stop the walk, not an error, without committing.
    ///
    /// As in C, fc_continue and fc_break come from the body itself, never
    /// passed up from a helper: they name *a* loop, and one that travels
    /// through a helper's own loop names the wrong one. Helpers return what
    /// happened; the body decides what that means for the walk.
    fn for_each_attempt_inner<P, S, F>(
        &mut self,
        trans:    &BtreeTrans<'t>,
        commit:   Option<(Option<&DiskReservation<'_>>, CommitOpts)>,
        mut peek: P,
        mut step: S,
        mut f:    F,
    ) -> Result<(), BchError>
    where
        P: FnMut(*mut c::btree_iter) -> c::bkey_s_c,
        S: FnMut(*mut c::btree_iter) -> bool,
        F: for<'a, 'k> FnMut(
            &'k TransAttempt<'a, 't>,
            &'k BtreeIter<'t>,
            BkeySC<'k>,
        ) -> Result<(), BchError>,
    {
        loop {
            let t = trans.begin();

            let (iter, k) = match self.peek_with(&t, &mut peek) {
                Err(e) if is_restart(&e) => continue,
                Err(e) => return Err(e),
                Ok(None) => return Ok(()),
                Ok(Some(ik)) => ik,
            };

            match f(&t, iter, k) {
                Ok(()) => {}
                Err(e) if is_restart(&e) => continue,
                Err(e) if e.matches(bch_errcode::BCH_ERR_fc_continue) => {
                    t.verify_not_restarted();
                    if !step(self.raw_mut()) {
                        return Ok(());
                    }
                    continue;
                }
                Err(e) if e.matches(bch_errcode::BCH_ERR_fc_break) => {
                    t.verify_not_restarted();
                    return Ok(());
                }
                Err(e) => return Err(e),
            }

            if let Some((disk_res, flags)) = commit {
                match t.commit(disk_res, flags) {
                    Err(e) if is_restart(&e) => continue,
                    r => r?,
                }
            }

            t.verify_not_restarted();

            if !step(self.raw_mut()) {
                return Ok(());
            }
        }
    }

    /// Walk the keys from the iterator's position, @f getting each key's
    /// attempt and committing for itself - for a body with work to do after its
    /// commit, which a restart there would get wrong. Otherwise as
    /// for_each_commit().
    pub fn for_each_attempt<F>(&mut self, trans: &BtreeTrans<'t>, f: F) -> Result<(), BchError>
    where
        F: for<'a, 'k> FnMut(
            &'k TransAttempt<'a, 't>,
            &'k BtreeIter<'t>,
            BkeySC<'k>,
        ) -> Result<(), BchError>,
    {
        self.for_each_attempt_inner(trans, None,
            |raw| unsafe { c::bch2_btree_iter_peek_max(raw, &SPOS_MAX) },
            |raw| unsafe { c::bch2_btree_iter_advance(raw) },
            f)
    }

    pub fn for_each_commit<F>(
        &mut self,
        trans:    &BtreeTrans<'t>,
        disk_res: Option<&DiskReservation<'_>>,
        flags:    impl Into<CommitOpts>,
        f:        F,
    ) -> Result<(), BchError>
    where
        F: for<'a, 'k> FnMut(
            &'k TransAttempt<'a, 't>,
            &'k BtreeIter<'t>,
            BkeySC<'k>,
        ) -> Result<(), BchError>,
    {
        self.for_each_attempt_inner(trans, Some((disk_res, flags.into())),
            |raw| unsafe { c::bch2_btree_iter_peek_max(raw, &SPOS_MAX) },
            |raw| unsafe { c::bch2_btree_iter_advance(raw) },
            f)
    }

    /// As for_each_commit(), from the iterator's position back down to @min.
    pub fn for_each_reverse_commit<F>(
        &mut self,
        trans:    &BtreeTrans<'t>,
        min:      bpos,
        disk_res: Option<&DiskReservation<'_>>,
        flags:    impl Into<CommitOpts>,
        f:        F,
    ) -> Result<(), BchError>
    where
        F: for<'a, 'k> FnMut(
            &'k TransAttempt<'a, 't>,
            &'k BtreeIter<'t>,
            BkeySC<'k>,
        ) -> Result<(), BchError>,
    {
        self.for_each_attempt_inner(trans, Some((disk_res, flags.into())),
            |raw| unsafe { c::bch2_btree_iter_peek_prev_min(raw, min) },
            |raw| unsafe { c::bch2_btree_iter_rewind(raw) },
            f)
    }

    pub fn for_each_max_commit<F>(
        &mut self,
        trans:      &BtreeTrans<'t>,
        end:        bpos,
        iter_flags: BtreeIterFlags,
        disk_res:   Option<&DiskReservation<'_>>,
        flags:      impl Into<CommitOpts>,
        f:          F,
    ) -> Result<(), BchError>
    where
        F: for<'a, 'k> FnMut(
            &'k TransAttempt<'a, 't>,
            &'k BtreeIter<'t>,
            BkeySC<'k>,
        ) -> Result<(), BchError>,
    {
        self.for_each_attempt_inner(trans, Some((disk_res, flags.into())),
            |raw| unsafe { (*raw).peek_max_type_raw(end, iter_flags) },
            |raw| unsafe { c::bch2_btree_iter_advance(raw) },
            f)
    }

    pub fn for_each_reverse<F, R>(&mut self, trans: &BtreeTrans<'t>, min: bpos, f: F)
        -> Result<R::Break, BchError>
    where
        F: for<'a> FnMut(&'a TransAttempt<'_, 't>, &'a BtreeIter<'t>, BkeySC<'a>) -> Result<R, BchError>,
        R: LoopControl,
    {
        self.for_each_inner(trans,
            |raw| unsafe { c::bch2_btree_iter_peek_prev_min(raw, min) },
            |raw| unsafe { c::bch2_btree_iter_rewind(raw) },
            f)
    }

    pub fn for_each_reverse_flags<F, R>(
        &mut self,
        trans: &BtreeTrans<'t>,
        flags: BtreeIterFlags,
        f:     F,
    ) -> Result<R::Break, BchError>
    where
        F: for<'a> FnMut(&'a TransAttempt<'_, 't>, &'a BtreeIter<'t>, BkeySC<'a>) -> Result<R, BchError>,
        R: LoopControl,
    {
        self.for_each_inner(trans,
            |raw| unsafe { (*raw).peek_prev_type_raw(flags) },
            |raw| unsafe { c::bch2_btree_iter_rewind(raw) },
            f)
    }

    /// Step past the key just returned: false at the end of the btree.
    pub fn advance(&mut self) -> bool {
        unsafe { c::bch2_btree_iter_advance(self.raw_mut()) }
    }

    /// Step back past the key just returned, for walking backwards: false at
    /// the start of the btree.
    pub fn rewind(&mut self) -> bool {
        unsafe { c::bch2_btree_iter_rewind(self.raw_mut()) }
    }
}

impl<'t> Drop for BtreeIter<'t> {
    fn drop(&mut self) {
        unsafe { c::bch2_trans_iter_exit(self.raw_mut()) }
    }
}

pub struct BtreeNodeIter<'t> {
    raw:   BtreeIter<'t>,
}

impl<'t> BtreeNodeIter<'t> {
    pub fn new(
        trans: impl TransRef<'t>,
        btree: impl Into<u32>,
        pos: bpos,
        locks_want: u32,
        depth: u32,
        flags: BtreeIterFlags,
    ) -> BtreeNodeIter<'t> {
        unsafe {
            let mut iter = MaybeUninit::<BtreeIter<'t>>::uninit();
            c::__bch2_trans_node_iter_init(
                trans.trans_raw(),
                iter.as_mut_ptr().cast(),
                c::btree_id::from_raw(btree.into()).expect("invalid btree id"),
                pos,
                locks_want,
                depth,
                c::btree_iter_update_trigger_flags::from_bits_retain(flags.bits()),
            );

            BtreeNodeIter { raw: iter.assume_init() }
        }
    }

    pub fn peek(&mut self) -> Result<Option<&c::btree>, BchError> {
        unsafe {
            let b = c::bch2_btree_iter_peek_node(self.raw.raw_mut());
            // NULL past the last
            Ok(errptr_or_null(b)?.map(|b| b.as_ref()))
        }
    }

    /// peek(), the node held to update its key: btree_node_update_key()
    /// takes it, with iter().
    pub(crate) fn peek_ref(&mut self) -> Result<Option<BtreeNodeRef>, BchError> {
        // SAFETY: an initialized iterator; the node, while its path holds it
        let b = unsafe { c::bch2_btree_iter_peek_node(self.raw.raw_mut()) };
        Ok(errptr_or_null(b)?.map(|raw| BtreeNodeRef { raw }))
    }

    pub(crate) fn iter(&mut self) -> &mut BtreeIter<'t> {
        &mut self.raw
    }

    pub fn peek_max_type<'i>(
        &'i mut self,
        end: bpos,
        flags: BtreeIterFlags,
    ) -> Result<Option<BkeySC<'i>>, BchError> {
        unsafe { bkey_s_c_to_result(self.raw.peek_max_type_raw(end, flags)) }
    }

    /// for_each_btree_node(): @f's result as BtreeIter::for_each()'s body's. A
    /// restart retries the node.
    pub fn for_each<F, R>(&mut self, trans: &BtreeTrans<'_>, mut f: F) -> Result<R::Break, BchError>
    where
        F: for<'a> FnMut(&'a c::btree) -> Result<R, BchError>,
        R: LoopControl,
    {
        let raw = self.raw.raw_mut();
        loop {
            let t = trans.begin();
            let b = unsafe { c::bch2_btree_iter_peek_node(raw) };
            let b = match errptr_or_null(b) {
                Err(e) if is_restart(&e) => continue,
                Err(e) => return Err(e),
                Ok(None) => return Ok(Default::default()),
                Ok(Some(b)) => unsafe { b.as_ref() },
            };

            t.verify_not_restarted();

            // peek_node() leaves iter->pos at the node's min_key (so a restart
            // re-finds the node across splits/merges), so we can't use
            // bch2_btree_iter_advance(); advance explicitly off the node's
            // max_key. set_pos + bpos_successor re-traverses from the root, so
            // the journal overlay applies and journal-only nodes aren't skipped
            // the way next_node()'s sibling walk did. Matches for_each_btree_node().
            let end = b.key.k.p;
            match f(b).map(LoopControl::into_step) {
                Err(e) if is_restart(&e) => continue,
                Err(e) => return Err(e),
                Ok(Step::Break(v)) => return Ok(v),
                Ok(Step::From(pos)) => unsafe { (*raw).set_pos(pos) },
                Ok(Step::Next) => {
                    if end == SPOS_MAX {
                        return Ok(Default::default());
                    }
                    unsafe { (*raw).set_pos(end.successor()) };
                }
            }
        }
    }
}

impl<'b, 'f> c::btree {
    pub fn to_text(&'b self, fs: &'f Fs) -> BtreeNodeToText<'b, 'f> {
        BtreeNodeToText { b: self, fs }
    }

    pub fn ondisk_to_text(&'b self, fs: &'f Fs) -> BtreeNodeOndiskToText<'b, 'f> {
        BtreeNodeOndiskToText { b: self, fs }
    }
}

impl c::btree {
    /// The node's key, all of it - its value runs past bkey_i, into key_pad.
    pub fn key_i(&self) -> &BkeyI {
        // SAFETY: the node's key, from a pointer to all of the node
        unsafe { &*BkeyI::from_raw(&raw const self.key) }
    }

    pub fn is_fake(&self) -> bool {
        (self.flags >> c::btree_flags::BTREE_NODE_fake as u64) & 1 != 0
    }

    /// Not yet reachable: an interior update in flight will make it so.
    pub fn will_make_reachable(&self) -> bool {
        (self.flags >> c::btree_flags::BTREE_NODE_will_make_reachable as u64) & 1 != 0
    }

    /// The node's keys, unpacked, in order.
    pub fn for_each_key<F>(&self, mut f: F) -> ControlFlow<()>
    where
        F: for<'a> FnMut(BkeySC<'a>) -> ControlFlow<()>,
    {
        let b = self as *const _ as *mut c::btree;
        let mut node_iter = c::btree_node_iter::default();
        let mut unpacked: c::bkey = pin_init::Zeroable::zeroed();

        unsafe { c::bch2_btree_node_iter_init_from_start(&mut node_iter, b) };

        loop {
            let k = unsafe {
                c::bch2_btree_node_iter_peek_unpack(&mut node_iter, b, &mut unpacked)
            };
            if k.k.is_null() {
                return ControlFlow::Continue(());
            }
            if f(BkeySC {
                k:    unsafe { &*k.k },
                v:    k.v,
                iter: PhantomData,
                val:  PhantomData,
            }).is_break() {
                return ControlFlow::Break(());
            }
            unsafe { c::bch2_btree_node_iter_advance(&mut node_iter, b) };
        }
    }
}

pub struct PathLeaf<'a> {
    l:      *mut c::btree_path_level,
    _trans: PhantomData<&'a ()>,
}

impl<'a> PathLeaf<'a> {
    pub fn node(&self) -> &'a c::btree {
        unsafe { &*(*self.l).b }
    }

    /// Where the bset search tree failed at the path's key, if it did:
    /// bch2_bfloat_to_text(). Peeks the node iterator, moving it past deleted
    /// keys.
    pub fn bfloat_to_text(&self, out: &mut Printbuf) {
        unsafe {
            c::bch2_btree_node_iter_bfloat_to_text(out.as_raw(), (*self.l).b, &raw mut (*self.l).iter)
        }
    }
}

pub struct BtreeNodeToText<'b, 'f> {
    b:  &'b c::btree,
    fs: &'f Fs,
}

impl ToText for BtreeNodeToText<'_, '_> {
    fn to_text(&self, out: &mut Printbuf) {
        unsafe { c::bch2_btree_node_to_text(out.as_raw(), self.fs.as_ptr(), self.b) }
    }
}
crate::display_via_to_text!(BtreeNodeToText<'_, '_>);

pub struct BtreeNodeOndiskToText<'b, 'f> {
    b:  &'b c::btree,
    fs: &'f Fs,
}

impl ToText for BtreeNodeOndiskToText<'_, '_> {
    fn to_text(&self, out: &mut Printbuf) {
        unsafe { c::bch2_btree_node_ondisk_to_text(out.as_raw(), self.fs.as_ptr(), self.b) }
    }
}
crate::display_via_to_text!(BtreeNodeOndiskToText<'_, '_>);

// ── The filesystem's transactions: c->btree.trans.list ──────────────────────

/// A transaction off the list, kept live - its paths, its task - by the ref.
pub struct ListedTrans<'a> {
    trans: *mut c::btree_trans,
    _ref:  ClosureRef<'a>,
}

impl ListedTrans<'_> {
    /// Whether it holds the SRCU lock btree nodes are freed under.
    pub fn srcu_held(&self) -> bool {
        // its own thread changes it: READ_ONCE()
        unsafe { core::ptr::read_volatile(&raw const (*self.trans).bits) }.srcu_held()
    }

    pub fn backtrace_to_text(&self, out: &mut Printbuf) {
        unsafe {
            let task = core::ptr::read_volatile(&raw const (*self.trans).locking_wait.task);
            c::bch2_prt_task_backtrace(out.as_raw(), task, 0, c::GFP_KERNEL);
        }
    }

    /// The cycle, if it's in one, written to @out.
    pub fn check_for_deadlock(&self, out: &mut Printbuf) -> bool {
        unsafe { c::bch2_check_for_deadlock(self.trans, out.as_raw()) != 0 }
    }
}

impl ToText for ListedTrans<'_> {
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
        pred:   impl Fn(&ListedTrans<'_>) -> bool,
        mut f:  impl FnMut(ListedTrans<'_>) -> Result<ControlFlow<()>, E>,
    ) -> Result<(), E> {
        let lock = Lock::from_c(&self.btree.trans.lock);
        let head = (&raw const self.btree.trans.list).cast_mut();

        // SAFETY, throughout: the list, under its lock
        'restart: loop {
            let mut guard = lock.lock();
            unsafe { <crate::list!(c::btree_trans, list)>::from_c(head) }
                .sort(|a, b| core::ptr::from_ref(a).cmp(&core::ptr::from_ref(b)));

            let mut pos = unsafe { (*head).next };
            while pos != head {
                let trans: *mut c::btree_trans =
                    unsafe { pos.byte_sub(core::mem::offset_of!(c::btree_trans, list)) }.cast();

                if trans as usize > *cursor {
                    *cursor = trans as usize;

                    let t = closure_get_not_zero(unsafe { &(*trans).ref_ })
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
