use super::bkey::*;
use crate::alloc::buckets::DiskReservation;
use crate::c;
use crate::errcode::{
    BchError,
    bch_errcode,
    errptr_to_result,
    errptr_to_result_c,
    ret_to_result_void as ret_to_result,
};
use crate::fs::Fs;
use crate::printbuf_to_formatter;
use crate::util::ffi::Opaque;
use crate::util::log::CFnName;
use crate::SPOS_MAX;
use bitflags::bitflags;
use core::cell::UnsafeCell;
use core::fmt;
use core::marker::PhantomData;
use core::mem::{size_of, ManuallyDrop, MaybeUninit};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use core::ptr::NonNull;
use core::slice;
use core::ops::ControlFlow;

use c::bpos;

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
        $crate::btree::iter::BtreeTrans::new_fn($fs, FN.idx($crate::function_name!()))
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

pub struct BtreeTrans<'f> {
    raw: *mut c::btree_trans,
    fs:  &'f Fs,
}

impl<'f> BtreeTrans<'f> {
    /// Use btree_trans!(), which names the transaction for its caller.
    #[doc(hidden)]
    pub fn new_fn(fs: &'f Fs, fn_idx: u32) -> BtreeTrans<'f> {
        unsafe {
            BtreeTrans {
                raw: &mut *c::__bch2_trans_get(fs.as_ptr(), fn_idx),
                fs,
            }
        }
    }

    /// A transaction C owns, for Rust that C calls with one: it isn't put when
    /// dropped. Its filesystem is its own trans->c.
    ///
    /// # Safety
    /// @raw is a live transaction, for as long as this lives.
    pub unsafe fn borrow_raw(raw: *mut c::btree_trans) -> ManuallyDrop<BtreeTrans<'f>> {
        let fs = unsafe { &*(*raw).c };
        ManuallyDrop::new(BtreeTrans { raw, fs })
    }

    /// As borrow_raw(), for an entry point C calls with a transaction: safe,
    /// as an Opaque is always a live one, and lives as long as the borrow.
    pub fn from_c(trans: &'f Opaque<c::btree_trans>) -> ManuallyDrop<BtreeTrans<'f>> {
        unsafe { Self::borrow_raw(trans.as_ptr()) }
    }

    /// How many times this transaction has committed: what's cached from the
    /// btree is stale once it moves on.
    pub fn commit_count(&self) -> u32 {
        unsafe { (*self.raw).commit_count }
    }

    /// The journal sequence number the commit in progress is going into: for
    /// atomic triggers, which run with the journal reservation held.
    pub fn journal_res_seq(&self) -> u64 {
        unsafe { (*self.raw).journal_res.seq }
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
            c::bch2_btree_delete_range_trans(self.raw, btree, start, end,
                                             c::btree_iter_update_trigger_flags::from_bits_retain(flags.bits()))
        }).map(|_| ())
    }

    /// Flush the btree write buffer, through to the journal sequence number
    /// being written. It leaves the transaction unlocked - the next btree
    /// access has to begin it again.
    pub fn write_buffer_flush_sync(&self) -> Result<(), BchError> {
        crate::errcode::ret_to_result(unsafe {
            c::bch2_btree_write_buffer_flush_sync(self.raw)
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
            restart_count: unsafe { (*self.raw).restart_count },
            t:             PhantomData,
        }
    }

    /// bch2_trans_begin(), for a loop that tracks restarts itself: the new
    /// restart count.
    pub(crate) fn begin_raw(&self) -> u32 {
        unsafe { c::bch2_trans_begin(self.raw) }
    }

    /// Take back the locks unlock() dropped: a transaction restart if they've
    /// changed hands meanwhile.
    pub fn relock(&self) -> Result<(), BchError> {
        let trans = unsafe { &*self.raw };
        if trans.locked() && trans.restarted().0 == 0 {
            return Ok(());
        }
        crate::errcode::ret_to_result(unsafe { c::__bch2_trans_relock(self.raw, true) }).map(|_| ())
    }

    /// relock(), always attempted and without the tracepoint: as
    /// bch2_trans_relock_notrace().
    pub fn relock_notrace(&self) -> Result<(), BchError> {
        crate::errcode::ret_to_result(unsafe { c::bch2_trans_relock_notrace(self.raw) }).map(|_| ())
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
            if (*self.raw).restart_count != restart_count {
                c::bch2_trans_restart_error(self.raw, restart_count);
            }
        }
    }

    pub fn raw(&self) -> *mut c::btree_trans {
        self.raw
    }

    pub(crate) fn fs(&self) -> &'f Fs {
        self.fs
    }

    pub fn unlock(&self) {
        unsafe { c::bch2_trans_unlock(self.raw) };
    }

    pub fn unlock_long(&self) {
        unsafe { c::bch2_trans_unlock_long(self.raw) };
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
            let trans = self.raw;
            (*trans).disk_res    = disk_res.map_or(core::ptr::null_mut(), |r| r.as_mut_ptr());
            (*trans).journal_seq = core::ptr::null_mut();
            (*trans).flush       = core::ptr::null_mut();
            c::__bch2_trans_commit(trans, flags.to_c(), lazy)
        };
        ret_to_result(ret)
    }
}

impl<'f> Drop for BtreeTrans<'f> {
    fn drop(&mut self) {
        unsafe { c::bch2_trans_put(&mut *self.raw) }
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
pub trait TransRef<'t> {
    fn trans_raw(&self) -> *mut c::btree_trans;
}

impl<'i, 'f> TransRef<'i> for &'i BtreeTrans<'f> {
    fn trans_raw(&self) -> *mut c::btree_trans { self.raw }
}

impl<'a, 't> TransRef<'a> for &TransAttempt<'a, 't> {
    fn trans_raw(&self) -> *mut c::btree_trans { self.trans.raw }
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

/// What a for_each body returns on success: whether the loop goes on, and
/// if not, what it stopped with. `()` for a body that always goes on - most
/// of them, as a C loop body returns 0 - and `ControlFlow` for one that can
/// stop early, breaking with what the loop returns.
///
/// A loop that runs to the end returns Break's default: `()`, `None`,
/// `false` - nothing found.
pub trait LoopControl {
    type Break: Default;

    /// What the body stopped with; None to go on.
    fn into_break(self) -> Option<Self::Break>;
}

impl LoopControl for () {
    type Break = ();

    fn into_break(self) -> Option<()> { None }
}

impl<B: Default> LoopControl for ControlFlow<B> {
    type Break = B;

    fn into_break(self) -> Option<B> { self.break_value() }
}

/// A key in the transaction's memory, to update with: of key type @V, its
/// value whole - as the typed constructors make one - or, untyped, any key.
pub struct TransBkey<'a, 't, V: ?Sized = c::bch_val> {
    ptr:      NonNull<c::bkey_i>,
    buf_u64s: u32,
    t:        PhantomData<&'a mut TransAttempt<'a, 't>>,
    v:        PhantomData<*const V>,
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

    fn as_ptr(&self) -> *mut c::btree {
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
                return Ok(errptr_to_result(c::__bch2_trans_kmalloc(trans, size, 0))? as *mut u8);
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

    pub fn bkey_alloc_typed<K: SizedBkey>(&self) -> Result<TransBkey<'a, 't, K>, BchError> {
        debug_assert_eq!(size_of::<BkeyI<K>>() % size_of::<u64>(), 0);

        let u64s = (size_of::<BkeyI<K>>() / size_of::<u64>()) as u32;
        let k = self.bkey_alloc(u64s)?;

        unsafe {
            k.as_ptr().cast::<BkeyI<K>>().write(BkeyI::new());
        }

        // SAFETY: a @K's key, all of it
        Ok(unsafe { k.typed() })
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
            core::ptr::copy_nonoverlapping(k.k, &mut dst.k_i_mut().k, 1);
            core::ptr::copy_nonoverlapping(
                k.v as *const c::bch_val as *const u64,
                dst.as_mut_u64s().as_mut_ptr().add(BKEY_U64S),
                copy_val,
            );
        }
        dst.k_mut().u64s = (BKEY_U64S + val_u64s) as u8;

        Ok(dst)
    }

    /// A fresh key: bkey_init()ed header with `type_` at `pos`, and
    /// `val_u64s` of zeroed value space.
    pub fn bkey_alloc_init(&self, val_u64s: usize, type_: u8, pos: c::bpos)
        -> Result<TransBkey<'a, 't>, BchError>
    {
        let mut k = self.bkey_alloc((BKEY_U64S + val_u64s) as u32)?;
        k.as_mut_u64s().fill(0);
        *k.k_mut() = c::bkey::new();
        k.k_mut().u64s = (BKEY_U64S + val_u64s) as u8;
        k.k_mut().type_ = type_;
        k.k_mut().p = pos;
        Ok(k)
    }

    /// @k as a mutable copy, for an update, with at least @min_bytes: past
    /// @k's own value, zeroes, which is what those fields read as, and the key
    /// covers them, so writes there go in with the commit. A u64 more is
    /// allocated past the end, for varint_decode_fast().
    fn make_mut_noupdate(&self, k: BkeySC<'_>, min_bytes: usize)
        -> Result<TransBkey<'a, 't>, BchError>
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
        dst.k_mut().u64s = u64s as u8;
        Ok(dst)
    }

    /// @k as a mutable copy, not yet an update: of the type @k is read as -
    /// any key's, as it is, or a typed key's, grown to all of its value
    /// (MutBkey).
    pub fn bkey_make_mut_noupdate<V: MutBkey + ?Sized>(&self, k: BkeySC<'_, V>)
        -> Result<TransBkey<'a, 't, V::Mut>, BchError>
    {
        let k = self.make_mut_noupdate(k.s_c(), V::MIN_BYTES)?;
        // SAFETY: @k's, grown to MIN_BYTES: MutBkey's
        Ok(unsafe { k.cast() })
    }

    /// The value of the @K at @pos in @btree, zero padded:
    /// ENOENT_bkey_type_mismatch if it isn't one.
    pub fn bkey_get_val<K: SizedBkey>(&self, btree: c::btree_id, pos: bpos, flags: BtreeIterFlags)
        -> Result<K, BchError>
    {
        let mut iter = BtreeIter::new(self.trans, btree, pos, flags);
        let k = iter.peek_slot_typed::<K>(self)?.s_c();
        Ok(K::val_copy_pad(k).expect("peek_slot_typed() checked the type"))
    }

    /// bkey_get_val(), the whole key.
    pub fn bkey_get_key<K: SizedBkey>(&self, btree: c::btree_id, pos: bpos, flags: BtreeIterFlags)
        -> Result<BkeyI<K>, BchError>
    {
        let mut iter = BtreeIter::new(self.trans, btree, pos, flags);
        let k = iter.peek_slot_typed::<K>(self)?.s_c();
        Ok(BkeyI::from_key(k).expect("peek_slot_typed() checked the type"))
    }

    /// The @K at @pos in @btree, as a mutable copy already queued as its
    /// update, grown to all of its value - ENOENT_bkey_type_mismatch if it
    /// isn't a @K. @iter_flags for the lookup - BtreeIterFlags::CACHED, for a
    /// btree read through the key cache - and @flags for the update: C passes
    /// both in one enum.
    pub fn bkey_get_mut<K: SizedBkey>(
        &self,
        btree:      c::btree_id,
        pos:        bpos,
        iter_flags: BtreeIterFlags,
        flags:      UpdateTriggerFlags,
    ) -> Result<TransBkey<'a, 't, K>, BchError> {
        let mut iter = BtreeIter::new(self.trans, btree, pos, iter_flags | BtreeIterFlags::INTENT);
        let k = iter.peek_slot_typed::<K>(self)?.s_c();
        let k = self.make_mut_noupdate(k, size_of::<BkeyI<K>>())?;
        self.update(&iter, &k, flags)?;
        // SAFETY: a @K's, grown to all of it
        Ok(unsafe { k.typed() })
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
                                                  old.to_raw(), new.to_raw())
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

    /// delete_at() on an iterator C owns, for Rust that C calls with one.
    pub(crate) fn delete_at_raw(
        &self,
        iter:  &mut c::btree_iter,
        flags: UpdateTriggerFlags,
    ) -> Result<(), BchError> {
        let ret = unsafe {
            c::bch2_btree_delete_at(self.raw(), iter, c::btree_iter_update_trigger_flags::from_bits_retain(flags.bits()))
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

    pub(crate) fn btree_node_update_key(
        &self,
        iter:          &mut BtreeIter<'a>,
        node:          BtreeNodeRef,
        key:           TransBkey<'_, 't>,
        flags:         impl Into<CommitOpts>,
        iter_searched: bool,
    ) -> Result<(), BchError> {
        let ret = unsafe {
            c::bch2_btree_node_update_key(
                self.raw(),
                iter.raw_mut(),
                node.as_ptr(),
                key.as_ptr(),
                flags.into().bits(),
                iter_searched,
            )
        };
        self.result(ret)
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

    /// The key, all of it, as long as its u64s say.
    pub fn k_i(&self) -> &BkeyI {
        // SAFETY: a key in our buffer, which the pointer reaches all of
        unsafe { &*BkeyI::from_raw(self.ptr.as_ptr()) }
    }

    /// k_i(), to edit. Its length is its u64s when it's taken: one that
    /// changes them - grows the key, in its buffer - takes it again after.
    pub fn k_i_mut(&mut self) -> &mut BkeyI {
        // SAFETY: as k_i()'s, and we have the buffer mutably
        unsafe { &mut *BkeyI::from_raw_mut(self.ptr.as_ptr()) }
    }

    pub fn as_u64s(&self) -> &[u64] {
        self.as_ref()
    }

    pub fn as_mut_u64s(&mut self) -> &mut [u64] {
        self.as_mut()
    }
}

impl<'a, 't, V: SizedBkey> TransBkey<'a, 't, V> {
    /// The key, as a @V's: all of its value there, as the typed constructors
    /// make it.
    pub fn key(&self) -> &BkeyI<V> {
        // SAFETY: the buffer holds a whole BkeyI<@V> - typed()'s - whatever
        // the header has been set to since, and any bytes are a @V: SizedBkey
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
        let ptr = errptr_to_result(ptr)?;
        let ptr = NonNull::new(ptr).expect("a trans mem key, or an error");
        Ok(TransBkey {
            buf_u64s: unsafe { ptr.as_ref().k.u64s as u32 },
            ptr,
            t:        PhantomData,
            v:        PhantomData,
        })
    }

    /// The key as a @V's.
    ///
    /// # Safety
    /// It's of @V's type, and the buffer holds a whole BkeyI<@V>.
    pub(crate) unsafe fn typed<V: SizedBkey>(self) -> TransBkey<'a, 't, V> {
        debug_assert!(self.k().is::<V>());
        debug_assert!(self.buf_u64s as usize * size_of::<u64>() >= size_of::<BkeyI<V>>());
        // SAFETY: the caller's
        unsafe { self.cast() }
    }

    /// The key as a @V's, unchecked: typed(), for a @V that may be any key's.
    ///
    /// # Safety
    /// As typed()'s, for a @V that's a SizedBkey.
    unsafe fn cast<V: ?Sized>(self) -> TransBkey<'a, 't, V> {
        TransBkey { ptr: self.ptr, buf_u64s: self.buf_u64s, t: PhantomData, v: PhantomData }
    }
}

impl<V: ?Sized> AsRef<[u64]> for TransBkey<'_, '_, V> {
    fn as_ref(&self) -> &[u64] {
        unsafe { slice::from_raw_parts(self.ptr.as_ptr() as *const u64, self.buf_u64s as usize) }
    }
}

impl<V: ?Sized> AsMut<[u64]> for TransBkey<'_, '_, V> {
    fn as_mut(&mut self) -> &mut [u64] {
        unsafe { slice::from_raw_parts_mut(self.ptr.as_ptr() as *mut u64, self.buf_u64s as usize) }
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
        const CACHED_NOFILL = c::btree_iter_update_trigger_flags::BTREE_ITER_cached_nofill.bits();
        const KEY_CACHE_FILL = c::btree_iter_update_trigger_flags::BTREE_ITER_key_cache_fill.bits();
        // For hash table inserts, which take them with the iterator flags:
        const STR_HASH_MUST_CREATE = c::btree_iter_update_trigger_flags::STR_HASH_must_create.bits();
        const STR_HASH_MUST_REPLACE = c::btree_iter_update_trigger_flags::STR_HASH_must_replace.bits();
    }
}

bitflags! {
    /// The BTREE_UPDATE_* and BTREE_TRIGGER_* half of C's
    /// btree_iter_update_trigger_flags: how an update commits, and which
    /// triggers it runs. Apart from the iterator half because the str_hash
    /// inserts take both.
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
fn bkey_arm<'k, K: TypedBkey + ?Sized>(fs: &Fs, k: BkeySC<'k>) -> Result<BkeySC<'k, K::InPlace>, BchError> {
    match k.arm::<K>() {
        Some(k) => Ok(k),
        None    => fs.throw(bch_errcode::BCH_ERR_ENOENT_bkey_type_mismatch),
    }
}

impl c::btree_iter {
    fn iter_flags(&self) -> BtreeIterFlags {
        BtreeIterFlags::from_bits_retain(self.flags)
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
    pub fn peek_max_type(&mut self, end: c::bpos, flags: BtreeIterFlags) -> c::bkey_s_c {
        let slots = flags.contains(BtreeIterFlags::SLOTS);
        let prev  = flags.contains(BtreeIterFlags::PREV);

        unsafe {
            match (prev, slots) {
                (false, false) => return c::bch2_btree_iter_peek_max(self, &end),
                (true,  false) => return c::bch2_btree_iter_peek_prev_min(self, end),
                (true,  true)  if bkey_lt(self.pos, end) => return c::bkey_s_c::default(),
                (false, true)  if bkey_gt(self.pos, end) => return c::bkey_s_c::default(),
                _ => {}
            }
            c::bch2_btree_iter_peek_slot(self)
        }
    }

    /// The previous key, or with slots the slot here: the static inline
    /// bch2_btree_iter_peek_prev_type().
    pub fn peek_prev_type(&mut self, flags: BtreeIterFlags) -> c::bkey_s_c {
        unsafe {
            if flags.contains(BtreeIterFlags::SLOTS) {
                c::bch2_btree_iter_peek_slot(self)
            } else {
                c::bch2_btree_iter_peek_prev_min(self, POS_MIN)
            }
        }
    }
}

/// A btree iterator. Its position, and the key a peek returned - whose
/// header is in the iterator, iter->k - only change through &mut self;
/// what changes through &self is internal state the outside world doesn't
/// see, as an update setting up the iterator's key cache path. Hence the
/// UnsafeCell.
#[repr(transparent)]
pub struct BtreeIter<'t> {
    raw:   UnsafeCell<c::btree_iter>,
    trans: PhantomData<&'t BtreeTrans<'t>>,
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
pub(crate) unsafe fn polonius_key<'i>(k: BkeySC<'_>) -> BkeySC<'i> {
    unsafe { core::mem::transmute::<BkeySC<'_>, BkeySC<'i>>(k) }
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
    errptr_to_result_c(k.k).map(|_| {
        if !k.k.is_null() {
            unsafe {
                Some(BkeySC {
                    k:    &*k.k,
                    // Hole slots (peek_slot) return a deleted key with a NULL
                    // val: a dangling, aligned one instead, as val_bytes()
                    // makes a slice of it - empty, a deleted key's value, but
                    // a slice's pointer is never null.
                    v:    if !k.v.is_null() { k.v } else { NonNull::dangling().as_ptr() },
                    iter: PhantomData,
                    val:  PhantomData,
                })
            }
        } else {
            None
        }
    })
}

impl<'t> BtreeIter<'t> {
    /// A C caller's iterator, for Rust that C calls with one: borrowed, so
    /// it's still the caller's to exit.
    ///
    /// # Safety
    /// @raw is an iterator, initialized or not, that nothing else touches
    /// for 'a.
    pub(crate) unsafe fn borrow_raw<'a>(raw: *mut c::btree_iter) -> &'a mut BtreeIter<'t> {
        unsafe { &mut *(raw as *mut BtreeIter<'t>) }
    }

    /// As borrow_raw(), for an entry point C calls with an iterator: safe,
    /// as an Opaque is always C's, and the &mut says nothing else is using it.
    pub fn from_c<'a>(iter: &'a mut Opaque<c::btree_iter>) -> &'a mut BtreeIter<'t> {
        unsafe { Self::borrow_raw(iter.as_ptr()) }
    }

    pub fn uninit() -> BtreeIter<'t> {
        Self::from_raw(Default::default())
    }

    fn from_raw(raw: c::btree_iter) -> BtreeIter<'t> {
        BtreeIter {
            raw:   UnsafeCell::new(raw),
            trans: PhantomData,
        }
    }

    /// For C that takes the iterator without moving it - an update.
    pub(crate) fn raw(&self) -> *mut c::btree_iter {
        self.raw.get()
    }

    pub(crate) fn raw_mut(&mut self) -> *mut c::btree_iter {
        self.raw.get_mut()
    }

    fn r(&self) -> &c::btree_iter {
        unsafe { &*self.raw.get() }
    }

    /// The node the iterator's path holds at its level, if it has one - a
    /// level without one holds NULL or an error pointer, as IS_ERR_OR_NULL().
    pub(crate) fn node_at_iter_level<'a>(
        &self,
        t: &TransAttempt<'a, 't>,
    ) -> Option<BtreeNodeRef> {
        unsafe {
            let path = (*t.raw()).paths.add(self.r().path as usize);
            let node = (*path).l[(*path).level() as usize].b;

            // The top 4095 addresses are error pointers: -MAX_ERRNO..-1
            if node as usize > usize::MAX - 4095 {
                return None;
            }
            NonNull::new(node).map(|raw| BtreeNodeRef { raw })
        }
    }

    pub fn pos(&self) -> c::bpos {
        self.r().pos
    }

    pub fn btree(&self) -> c::btree_id {
        self.r().btree_id()
    }

    pub fn set_pos(&mut self, pos: c::bpos) {
        self.raw.get_mut().set_pos(pos);
    }

    /// For an extents iterator: back to the start of the key it's at.
    pub fn set_pos_to_extent_start(&mut self) {
        debug_assert!(self.is_extents());
        let iter = self.raw.get_mut();
        iter.pos = bkey_start_pos(&iter.k);
    }

    /// Look in @snapshot, at the same position.
    pub fn set_snapshot(&mut self, snapshot: u32) {
        let iter = self.raw.get_mut();
        iter.snapshot = snapshot;
        iter.set_pos(c::bpos { snapshot, ..iter.pos });
    }

    pub fn snapshot(&self) -> u32 {
        self.r().snapshot
    }

    /// Whether it walks extents - keys covering a range, ending at their
    /// position.
    pub fn is_extents(&self) -> bool {
        self.r().flags & BtreeIterFlags::IS_EXTENTS.bits() != 0
    }

    /// Turn on @flags: for walking an iterator with flags it wasn't made
    /// with - the loops peek with the iterator's own - as C's _continue
    /// loops pass theirs.
    pub fn set_flags(&mut self, flags: BtreeIterFlags) {
        self.raw.get_mut().flags |= flags.bits();
    }

    /// Turn off @flags: for an iterator going on with flags it was made
    /// without.
    pub fn clear_flags(&mut self, flags: BtreeIterFlags) {
        self.raw.get_mut().flags &= !flags.bits();
    }

    /// A second iterator at the same position, sharing this one's paths.
    pub fn copy(&self) -> BtreeIter<'t> {
        unsafe {
            let mut iter: MaybeUninit<c::btree_iter> = MaybeUninit::uninit();

            c::bch2_trans_copy_iter(iter.as_mut_ptr(), self.raw());

            Self::from_raw(iter.assume_init())
        }
    }

    pub fn new(
        trans: impl TransRef<'t>,
        btree: impl Into<u32>,
        pos: bpos,
        flags: BtreeIterFlags,
    ) -> BtreeIter<'t> {
        unsafe {
            let mut iter: MaybeUninit<c::btree_iter> = MaybeUninit::uninit();

            c::bch2_trans_iter_init_outlined(
                trans.trans_raw(),
                iter.as_mut_ptr(),
                c::btree_id::from_raw(btree.into()).expect("invalid btree id"),
                pos,
                c::btree_iter_update_trigger_flags::from_bits_retain(flags.bits()),
                0
            );

            Self::from_raw(iter.assume_init())
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
            let mut iter: MaybeUninit<c::btree_iter> = MaybeUninit::uninit();

            c::__bch2_trans_node_iter_init(
                trans.trans_raw(),
                iter.as_mut_ptr(),
                c::btree_id::from_raw(btree.into()).expect("invalid btree id"),
                pos,
                0,
                level,
                c::btree_iter_update_trigger_flags::from_bits_retain(flags.bits())
            );

            Self::from_raw(iter.assume_init())
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
        unsafe { bkey_s_c_to_result::<'k>(peek(self.raw.get_mut())) }
    }

    pub fn peek_max<'k>(&'k mut self, t: &'k TransAttempt<'_, 't>, end: bpos)
        -> Result<Option<BkeySC<'k>>, BchError>
    {
        self.peek_raw(t, |raw| unsafe { c::bch2_btree_iter_peek_max(raw, &end) })
    }

    /// The key at the iterator's position, or a deleted key for a hole.
    pub fn peek_slot<'k>(&'k mut self, t: &'k TransAttempt<'_, 't>)
        -> Result<Option<BkeySC<'k>>, BchError>
    {
        self.peek_raw(t, |raw| unsafe { c::bch2_btree_iter_peek_slot(raw) })
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
    pub fn peek_slot_typed<'k, K: TypedBkey + ?Sized>(&'k mut self, t: &'k TransAttempt<'_, 't>)
        -> Result<BkeySC<'k, K::InPlace>, BchError>
    {
        let k = self.peek_slot(t)?.expect("a slot always has a key");
        bkey_arm::<K>(t.fs(), k)
    }

    /// As peek_slot_typed(), for a key to update through this iterator: the
    /// iterator comes back shared, for as long as the key is used - as the
    /// for_each loops pass it - so the key can't be moved out from under.
    pub fn peek_slot_typed_shared<'k, K: TypedBkey + ?Sized>(&'k mut self, t: &'k TransAttempt<'_, 't>)
        -> Result<(&'k Self, BkeySC<'k, K::InPlace>), BchError>
    {
        let this: &'k Self = self;
        // 'k: the key is valid while the iterator is borrowed, within the
        // attempt - shared, it can't be moved or peeked again
        let k = unsafe { bkey_s_c_to_result::<'k>(c::bch2_btree_iter_peek_slot(this.raw.get()))? };
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
        self.peek_raw(t, |raw| unsafe { (*raw).peek_max_type(end, flags) })
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

            match f(&t, iter, k) {
                Err(e) if is_restart(&e) => continue,
                Err(e) => return Err(e),
                Ok(flow) => {
                    t.verify_not_restarted();
                    if let Some(b) = flow.into_break() {
                        return Ok(b);
                    }
                }
            }

            if !step(self.raw.get_mut()) {
                return Ok(Default::default());
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
        let k = unsafe { bkey_s_c_to_result::<'k>(peek(self.raw.get_mut())) }?;
        Ok(k.map(|k| (&*self, k)))
    }

    /// The next key up to @end, peeking as the iterator's own flags say - for a
    /// SLOTS iterator, a slot at a time, holes included. The for_each family
    /// peeks with this, as C's loops do, so no loop can ignore its iterator's
    /// flags.
    fn peek_own_type(raw: *mut c::btree_iter, end: bpos) -> c::bkey_s_c {
        unsafe {
            let flags = (*raw).iter_flags();
            (*raw).peek_max_type(end, flags)
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

            if let Some(b) = f(iter, k)?.into_break() {
                return Ok(b);
            }
            if !self.advance() {
                return Ok(Default::default());
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

            if let Some(b) = f(iter, k)?.into_break() {
                return Ok(b);
            }
            if !self.rewind() {
                return Ok(Default::default());
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
                    if !step(self.raw.get_mut()) {
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

            if !step(self.raw.get_mut()) {
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
            |raw| unsafe { (*raw).peek_max_type(end, iter_flags) },
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
            |raw| unsafe { (*raw).peek_prev_type(flags) },
            |raw| unsafe { c::bch2_btree_iter_rewind(raw) },
            f)
    }

    /// Step past the key just returned: false at the end of the btree.
    pub fn advance(&mut self) -> bool {
        unsafe { c::bch2_btree_iter_advance(self.raw.get_mut()) }
    }

    /// Step back past the key just returned, for walking backwards: false at
    /// the start of the btree.
    pub fn rewind(&mut self) -> bool {
        unsafe { c::bch2_btree_iter_rewind(self.raw.get_mut()) }
    }
}

impl<'t> Drop for BtreeIter<'t> {
    fn drop(&mut self) {
        unsafe { c::bch2_trans_iter_exit(self.raw.get_mut()) }
    }
}

pub struct BtreeNodeIter<'t> {
    raw:   c::btree_iter,
    trans: PhantomData<&'t BtreeTrans<'t>>,
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
            let mut iter: MaybeUninit<c::btree_iter> = MaybeUninit::uninit();
            c::__bch2_trans_node_iter_init(
                trans.trans_raw(),
                iter.as_mut_ptr(),
                c::btree_id::from_raw(btree.into()).expect("invalid btree id"),
                pos,
                locks_want,
                depth,
                c::btree_iter_update_trigger_flags::from_bits_retain(flags.bits()),
            );

            BtreeNodeIter {
                raw:   iter.assume_init(),
                trans: PhantomData,
            }
        }
    }

    pub fn peek(&mut self) -> Result<Option<&c::btree>, BchError> {
        unsafe {
            let b = c::bch2_btree_iter_peek_node(&mut self.raw);
            errptr_to_result_c(b).map(|b| if !b.is_null() { Some(&*b) } else { None })
        }
    }

    pub fn peek_max_type<'i>(
        &'i mut self,
        end: bpos,
        flags: BtreeIterFlags,
    ) -> Result<Option<BkeySC<'i>>, BchError> {
        unsafe { bkey_s_c_to_result(self.raw.peek_max_type(end, flags)) }
    }

    pub fn for_each<F>(&mut self, trans: &BtreeTrans<'_>, mut f: F) -> Result<(), BchError>
    where
        F: for<'a> FnMut(&'a c::btree) -> ControlFlow<()>,
    {
        let raw = &mut self.raw as *mut c::btree_iter;
        loop {
            let t = trans.begin();
            let b = unsafe { c::bch2_btree_iter_peek_node(raw) };
            let b = match errptr_to_result_c(b) {
                Err(e) if is_restart(&e) => continue,
                Err(e) => return Err(e),
                Ok(b) if b.is_null() => return Ok(()),
                Ok(b) => unsafe { &*b },
            };

            t.verify_not_restarted();

            // peek_node() leaves iter->pos at the node's min_key (so a restart
            // re-finds the node across splits/merges), so we can't use
            // bch2_btree_iter_advance(); advance explicitly off the node's
            // max_key. set_pos + bpos_successor re-traverses from the root, so
            // the journal overlay applies and journal-only nodes aren't skipped
            // the way next_node()'s sibling walk did. Matches for_each_btree_node().
            let end = b.key.k.p;
            if let ControlFlow::Break(()) = f(b) {
                return Ok(());
            }

            if end == SPOS_MAX {
                return Ok(());
            }
            unsafe { (*raw).set_pos(end.successor()) };
        }
    }
}

impl<'t> Drop for BtreeNodeIter<'t> {
    fn drop(&mut self) {
        unsafe { c::bch2_trans_iter_exit(&mut self.raw) }
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

    /// The node's keys, unpacked, in order.
    pub fn for_each_key<F>(&self, mut f: F) -> ControlFlow<()>
    where
        F: for<'a> FnMut(BkeySC<'a>) -> ControlFlow<()>,
    {
        let b = self as *const _ as *mut c::btree;
        let mut node_iter = c::btree_node_iter::default();
        let mut unpacked: c::bkey = unsafe { core::mem::zeroed() };

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

pub struct BtreeNodeToText<'b, 'f> {
    b:  &'b c::btree,
    fs: &'f Fs,
}

impl<'b, 'f> fmt::Display for BtreeNodeToText<'b, 'f> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        printbuf_to_formatter(f, |buf| unsafe {
            c::bch2_btree_node_to_text(buf, self.fs.as_ptr(), self.b)
        })
    }
}

pub struct BtreeNodeOndiskToText<'b, 'f> {
    b:  &'b c::btree,
    fs: &'f Fs,
}

impl<'b, 'f> fmt::Display for BtreeNodeOndiskToText<'b, 'f> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        printbuf_to_formatter(f, |buf| unsafe {
            c::bch2_btree_node_ondisk_to_text(buf, self.fs.as_ptr(), self.b)
        })
    }
}
