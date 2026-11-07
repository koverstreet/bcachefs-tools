// SPDX-License-Identifier: GPL-2.0

use super::bkey::*;
use super::types::{BtreeIter, BtreeTrans};
use super::iter::{BtreeIterFlags, BtreeNodeIter, BtreeNodeRef, CommitFlags};
use crate::alloc::capacity::DiskReservation;
use crate::c;
use crate::errcode::{
    BchError,
    bch_errcode,
    errptr_to_result,
    ret_to_result_void as ret_to_result,
};
use crate::btree::trans::TransGuard;
use crate::fs::Fs;
use crate::locking::MemallocFlags;
use crate::util::ffi::Opaque;
use crate::util::{Printbuf, ToText};
use core::cell::Cell;
use core::fmt;
use core::marker::PhantomData;
use core::mem::size_of;
use core::ptr::NonNull;
use core::slice;
use crate::btree::cache::btree_ptr_hash_val;
use crate::util::vstructs::{State, Validated};
use crate::format::{Bpos, Btree, BkeyType};

pub struct TransAttempt<'a, 't> {
    trans:         &'a BtreeTrans<'t>,
    /// the transaction's when the attempt began - or when restarts_handled()
    restart_count: Cell<u32>,
    /// ended by ignore_restarts()
    spent:         Cell<bool>,
    t:             PhantomData<&'a mut ()>,
}

impl<'a, 't> TransAttempt<'a, 't> {
    /// For @trans's begin(), and C's attempt in progress.
    pub(super) fn new(trans: &'a BtreeTrans<'t>, restart_count: u32) -> Self {
        TransAttempt { trans, restart_count: Cell::new(restart_count), spent: Cell::new(false), t: PhantomData }
    }
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
///   Not further: an unlock that consumes the attempt, or drop_locks_do() on
///   &mut, would reject keys used after a successful relock - which C relies
///   on, and is right (a relock fails if a node's lock changed hands) - and
///   spread &mut through every function that might unlock.
/// - That's lifetimes standing in for the state, and approximately. What we
///   want is the verifier tracking it: locked, unlocked or in restart, a Cell
///   in BtreeTrans, changed only by begin(), relock(), unlock() and anything
///   that returns an error - each proven to have its effect, construction
///   proven to need locked, and runtime asserts where a proof can't reach,
///   which later proofs can then assume. Verus can express this now (a
///   PCell and its permission); Aeneas, which we're leaning towards, first
///   needs UnsafeCell support - shared borrows of !Freeze types as heap
///   locations, reads and writes through them as effects.
/// What an iterator's made from, two kinds as C's walks are. A transaction,
/// outside any attempt, is a for_each_btree_key() walk's - each key its own
/// attempt - and like the macro it's begun before the iterator's made. An
/// attempt's is a _norestart walk's, inside it: nothing's begun.
pub trait TransRef<'t> {
    fn trans_raw(&self) -> *mut BtreeTrans<'_>;
}

impl<'i, 'f> TransRef<'i> for &'i BtreeTrans<'f> {
    fn trans_raw(&self) -> *mut BtreeTrans<'_> {
        self.begin_raw();
        self.raw()
    }
}

impl<'i, 'f> TransRef<'i> for &'i TransGuard<'f> {
    fn trans_raw(&self) -> *mut BtreeTrans<'_> {
        self.begin_raw();
        self.raw()
    }
}

impl<'a, 't> TransRef<'a> for &TransAttempt<'a, 't> {
    fn trans_raw(&self) -> *mut BtreeTrans<'_> { self.trans.raw() }
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
        let old = trans.begin_may_drop_updates();
        trans.set_begin_may_drop_updates(true);
        TransMayDropUpdates { trans, old }
    }
}

impl Drop for TransMayDropUpdates<'_, '_> {
    fn drop(&mut self) {
        self.trans.set_begin_may_drop_updates(self.old);
    }
}

/// Whether @e is a transaction restart: the attempt is over, and the loop
/// it's in begins again.
pub fn is_restart(e: &BchError) -> bool {
    e.matches(bch_errcode::BCH_ERR_transaction_restart)
}

/// A key to update with, valid and ours to edit while the attempt runs - in
/// the transaction's memory, or a buffer C hands over (from_raw_room()): of
/// key type @V, its value whole - as the typed constructors make one - or,
/// untyped, any key. It's edited in place, in its buffer, never moved: what
/// won't fit is NoRoom.
pub struct TransBkey<'a, 't, V: ?Sized = FamilyVal<AllTypes>> {
    ptr:      NonNull<c::bkey_i>,
    pub(crate) buf_u64s: u32,
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

impl<'a, 't> TransAttempt<'a, 't> {
    pub fn raw(&self) -> *mut BtreeTrans<'_> {
        self.trans.raw()
    }

    pub(crate) fn fs(&self) -> &'t Fs {
        self.trans.fs()
    }

    /// What the commit in progress changes usage by, as its triggers
    /// accounted it.
    pub fn fs_usage_delta(&self) -> c::bch_fs_usage_base {
        self.trans.fs_usage_delta.get()
    }

    /// The disk reservation the commit in progress charges, if it was given
    /// one.
    pub fn disk_res(&self) -> Option<&DiskReservation<'t>> {
        // SAFETY: commit()'s caller's, which outlives the commit - and the
        // attempt
        unsafe { self.trans.disk_res.get().as_ref() }
    }

    pub fn verify_not_restarted(&self) {
        if !self.spent.get() {
            self.trans.verify_not_restarted(self.restart_count.get());
        }
    }

    /// The restarts since the attempt began were a callee's, run and handled
    /// inside it - a read's own retries, under a data move: not this
    /// attempt's, which goes on. C's restoring trans->restart_count after the
    /// call.
    pub fn restarts_handled(&self) {
        self.restart_count.set(self.trans.restart_count());
    }

    /// Run @f - transactions of its own, top-level loops beginning and
    /// committing as they go - from inside this attempt, and end the attempt
    /// as though its step had: C's saving and restoring trans->restart_count
    /// around a nested loop.
    ///
    /// @f's first begin ends this attempt, so the attempt must be finished
    /// when @f starts: nothing queued, and nothing it holds used again - its
    /// keys, its iterators' positions, its locks, which @f's attempts have
    /// had since. After @f it's spent: verify_not_restarted() passes, and the
    /// loop running it steps its iterator and begins anew, by is_spent().
    ///
    /// Checked: nothing queued, and no relock or commit of a spent attempt.
    ///
    /// Not checked - the caller's to see to, until it can be verified:
    /// nothing borrowed from this attempt is used after @f. The key a step
    /// was given points into a btree node the attempt had locked; @f's
    /// attempts drop those locks, and by the time @f returns the node may
    /// have been rewritten, split, or freed and reused - reading the key then
    /// reads memory that isn't it any more. The types don't stop it: the
    /// key's borrow is of the attempt, which is still alive, so the key is
    /// still in scope after @f. Nor an update queued after @f, at the step's
    /// iterator: nothing commits a spent attempt, so the next begin drops it
    /// - with a warning under CONFIG_BCACHEFS_DEBUG, as any update never
    /// committed.
    pub fn ignore_restarts<T>(&self, f: impl FnOnce(&'a BtreeTrans<'t>) -> Result<T, BchError>)
        -> Result<T, BchError>
    {
        assert!(!self.has_updates(), "ignore_restarts() with updates queued: @f's begin would drop them");
        self.spent.set(true);
        f(self.trans)
    }

    /// Ended by ignore_restarts(): the loop running it must begin anew.
    pub fn is_spent(&self) -> bool {
        self.spent.get()
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
        assert!(!self.spent.get(), "relock of an attempt ignore_restarts() ended");
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
        self.trans.has_updates()
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
        assert!(!self.spent.get(), "commit in an attempt ignore_restarts() ended");
        self.trans.commit(disk_res, flags)
    }

    /// commit(), with a ref on @flush until the commit's journal entry is
    /// flushed.
    pub fn commit_flush(
        &self,
        disk_res: Option<&DiskReservation<'_>>,
        flush:    Option<&Opaque<c::closure>>,
        flags:    impl Into<CommitOpts>,
    ) -> Result<(), BchError> {
        assert!(!self.spent.get(), "commit in an attempt ignore_restarts() ended");
        self.trans.commit_inner(disk_res, flush, flags.into(), false)
    }

    /// Commit what's queued, if anything - and having committed, return
    /// transaction_restart_commit, so the caller's loop re-runs against the
    /// committed state.
    pub fn commit_lazy(&self, disk_res: Option<&DiskReservation<'_>>, flags: impl Into<CommitOpts>)
        -> Result<(), BchError>
    {
        if !self.has_updates() {
            return Ok(());
        }

        self.trans.commit_inner(disk_res, None, flags.into(), true)
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
        if self.trans.mem_with(|m| m.top_bytes()) < c::BTREE_TRANS_MEM_MAX as usize / 4 {
            return Ok(());
        }
        self.commit_lazy(None, flags)
    }

    pub fn result(&self, ret: i32) -> Result<(), BchError> {
        ret_to_result(ret).map(|_| ())
    }

    /// The transaction has to know: it's marked restarted and its restart_count
    /// bumped, which bch2_trans_begin() and verify_not_restarted() go by.
    pub fn restart(&self, error: bch_errcode) -> BchError {
        self.trans.restart(error)
    }

    /// Drop the btree locks for @f - to block, sleep, do IO - and take them
    /// back, relock_or_nested(): C's drop_locks_do(). @f's error is returned
    /// with the locks still dropped.
    ///
    /// A key peeked before is good after only if this succeeds - a relock
    /// fails if a node's lock changed hands - and not inside @f at all.
    /// Nothing checks either: that's for the verifier (see TransRef).
    pub fn drop_locks_do<T>(&self, f: impl FnOnce() -> Result<T, BchError>) -> Result<T, BchError> {
        self.unlock();
        let v = f()?;
        self.relock_or_nested()?;
        Ok(v)
    }

    /// Retake the locks after what dropped them - drop_locks_do()'s @f, or a
    /// call that drops them itself: a restart if they changed hands, or
    /// transaction_restart_nested, not relocked, if it ran attempts of its
    /// own, which end this one.
    pub fn relock_or_nested(&self) -> Result<(), BchError> {
        if self.trans.restart_count() != self.restart_count.get() {
            return Err(BchError::from(bch_errcode::BCH_ERR_transaction_restart_nested));
        }
        self.relock()
    }

    /// What @alloc makes: tried in a NORECLAIM scope, the locks held, and on
    /// ENOMEM again with them dropped - drop_locks_do(). C's
    /// allocate_dropping_locks() and its variants. @alloc names its ENOMEM
    /// (an Option's None by .ok_or()), and it's thrown if the second try
    /// fails too; any other error is returned as is.
    pub fn allocate_dropping_locks<T>(&self, alloc: impl FnMut() -> Result<T, BchError>)
        -> Result<T, BchError>
    {
        self.allocate_dropping(alloc, || self.unlock())
    }

    /// allocate_dropping_locks(), for what can take a while to get - megabytes:
    /// the second try lets go of SRCU too, unlock_long().
    pub fn allocate_dropping_locks_big<T>(&self, alloc: impl FnMut() -> Result<T, BchError>)
        -> Result<T, BchError>
    {
        self.allocate_dropping(alloc, || self.trans.unlock_long())
    }

    fn allocate_dropping<T>(&self, mut alloc: impl FnMut() -> Result<T, BchError>, unlock: impl FnOnce())
        -> Result<T, BchError>
    {
        let first = {
            let _noreclaim = MemallocFlags::noreclaim();
            alloc()
        };
        match first {
            Err(e) if e.matches(c::ENOMEM) => {}
            r => return r,
        }
        unlock();
        let v = alloc().map_err(|e| {
            if e.matches(c::ENOMEM) { self.fs().err(bch_errcode(e.raw() as u32)) } else { e }
        })?;
        self.relock_or_nested()?;
        Ok(v)
    }

    /// allocate_dropping_locks(), the locks left dropped if it took the
    /// blocking try: for a caller that relocks once, after more of them.
    pub fn allocate_dropping_locks_norelock<T>(&self, mut alloc: impl FnMut() -> Result<T, BchError>)
        -> Result<T, BchError>
    {
        let first = {
            let _noreclaim = MemallocFlags::noreclaim();
            alloc()
        };
        match first {
            Err(e) if e.matches(c::ENOMEM) => {}
            r => return r,
        }
        self.unlock();
        alloc()
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
        let orig_restart_count = trans.restart_count.get();
        let _may_drop = TransMayDropUpdates::new(trans);

        let mut retry;
        let mut t = self;
        loop {
            match f(t) {
                Ok(v) => {
                    t.verify_not_restarted();

                    if trans.restart_count.get() != orig_restart_count {
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

    /// Whether @p is in memory this attempt has allocated: a key read back
    /// from an update queued in it, rather than from a btree node.
    pub fn in_mem(&self, p: *const u8) -> bool {
        self.trans.mem_with(|m| m.contains(p))
    }

    /// Freed when the transaction next begins.
    pub fn kmalloc(&self, bytes: usize) -> Result<&'a mut [u8], BchError> {
        let ptr = self.kmalloc_raw(bytes)?;
        Ok(unsafe { core::slice::from_raw_parts_mut(ptr, bytes) })
    }

    /// A @T in this attempt's memory, zeroed: kmalloc()'s, typed. The memory
    /// is freed when the transaction next begins, without running
    /// destructors, so @T has none.
    pub fn alloc_zeroed<T: ::pin_init::Zeroable>(&self) -> Result<&'a mut T, BchError> {
        // bch2_trans_kmalloc() gives 8 byte alignment:
        const { assert!(core::mem::align_of::<T>() <= 8 && !core::mem::needs_drop::<T>()) };

        let p = self.kmalloc_raw(size_of::<T>())? as *mut T;
        // SAFETY: zeroed, and all-zeroes is a @T; aligned, and this attempt's
        unsafe { Ok(&mut *p) }
    }

    /// @init's T, in the attempt's memory. Never dropped - the transaction's
    /// memory is reused without, as C's is - so a T owning anything is
    /// emptied by whoever's done with it.
    pub fn alloc_init<T, E: Into<BchError>>(&self, init: impl ::pin_init::Init<T, E>) -> Result<&'a mut T, BchError> {
        // bch2_trans_kmalloc() gives 8 byte alignment:
        const { assert!(core::mem::align_of::<T>() <= 8) };

        let p = self.kmalloc_raw(size_of::<T>())?.cast::<T>();
        // SAFETY: room for a @T, aligned, this attempt's: init writes it
        unsafe { crate::util::alloc::init_at(init, p) }.map_err(Into::into)?;
        // SAFETY: just made
        Ok(unsafe { &mut *p })
    }

    /// @bytes of zeroed transaction memory, rounded up to u64s: off the top of
    /// the transaction's buffer, or __bch2_trans_kmalloc() grows it - which
    /// restarts the transaction if it had a buffer already. As
    /// bch2_trans_kmalloc(), less CONFIG_BCACHEFS_TRANS_KMALLOC_TRACE's
    /// record of the allocation.
    fn kmalloc_raw(&self, bytes: usize) -> Result<*mut u8, BchError> {
        let size = bytes.next_multiple_of(size_of::<u64>());
        let t = self.trans;

        match t.mem_with(|m| m.alloc(size / size_of::<u64>())) {
            Some(p) => Ok(p.as_ptr().cast()),
            None    => Ok(t.kmalloc_grow(&mut t.paths(), size, crate::ret_ip!())?.cast()),
        }
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
                k.k.val_u64s(),
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

        let copy_val = k.k.val_u64s().min(val_u64s);
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
    pub fn bkey_alloc_init<K: BkeyTag + BkeyFamily>(&self, val_u64s: usize, pos: Bpos)
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
    /// varint::decode_fast().
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
                k.k.val_u64s(),
            );
        }
        // SAFETY: @k's, as long as it was: a @V, as bkey_reassemble()'s copy is
        Ok(unsafe { dst.cast() })
    }

    /// @k as a mutable copy, for an update, with at least @min_bytes: past
    /// @k's own value, zeroes, which is what those fields read as, and the key
    /// covers them, so writes there go in with the commit.
    pub(crate) fn make_mut_noupdate<V: ?Sized + BkeyVal>(&self, k: BkeySC<'_, V>, min_bytes: usize)
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
    pub fn bkey_get_val<K: BkeyWhole>(&self, btree: Btree, pos: Bpos, flags: BtreeIterFlags)
        -> Result<K, BchError>
    {
        let mut iter = BtreeIter::new(self, btree, pos, flags);
        let k = iter.peek_slot_typed::<K>(self)?.untyped();
        Ok(K::val_copy_pad(k).expect("peek_slot_typed() checked the type"))
    }

    /// bkey_get_val(), the whole key.
    pub fn bkey_get_key<K: BkeyWhole>(&self, btree: Btree, pos: Bpos, flags: BtreeIterFlags)
        -> Result<BkeyI<K>, BchError>
    {
        let mut iter = BtreeIter::new(self, btree, pos, flags);
        let k = iter.peek_slot_typed::<K>(self)?.untyped();
        Ok(BkeyI::from_key(k).expect("peek_slot_typed() checked the type"))
    }

    pub fn btree_increase_depth(&self, iter: &BtreeIter<'a>) -> Result<(), BchError> {
        let trans = self.trans();
        super::interior::increase_depth(trans, &mut trans.paths(), iter.path, CommitOpts::new())
    }

    /// The pointer to the node at @level of @btree, at @key's position, made
    /// @key, committed: bch2_btree_node_update_key_at_pos().
    pub fn btree_node_update_key_at_pos<V: ?Sized>(&self, btree: Btree, level: u32,
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

    /// The pointer at @level of @btree, at @pos, edited by @f - handed a copy
    /// of the pointer the node below has, with room for any - and committed:
    /// the node's key updated in place, as btree_node_update_key_at_pos().
    pub fn btree_node_edit_key(&self, btree: Btree, level: u32, pos: Bpos,
                               f: impl FnOnce(&mut TransBkey<'a, 't>) -> Result<(), BchError>)
        -> Result<(), BchError>
    {
        let mut node_iter = BtreeNodeIter::new(self, btree, pos, 0, level - 1, BtreeIterFlags::empty());
        let b = node_iter.peek_ref()?.expect("the node a btree node pointer points to");
        let mut n = self.bkey_reassemble_room(b.key_sc(), c::BKEY_BTREE_PTR_U64s_MAX as usize * size_of::<u64>())?
            .into();
        f(&mut n)?;
        self.btree_node_update_key(node_iter.iter(), b, n, CommitFlags::NO_ENOSPC, false)
    }

    /// The pointer at @level of @btree, at @key's position, made @key and
    /// committed, its triggers not run - btree_node_update_key_at_pos(), for
    /// a repair or an injection that does the accounting itself, or none.
    pub fn btree_node_update_key_at_pos_raw<V: ?Sized>(&self, btree: Btree, level: u32,
                                                       key: TransBkey<'_, 't, V>) -> Result<(), BchError> {
        let mut node_iter = BtreeNodeIter::new(self, btree, key.k().p, 0, level - 1, BtreeIterFlags::INTENT);
        let b = node_iter.peek_ref()?.expect("the node a btree node pointer points to");
        self.btree_node_update_key(node_iter.iter(), b, key, CommitFlags::NO_ENOSPC, true)
    }

    /// @node, which @iter is at, rewritten.
    pub(crate) fn btree_node_rewrite(
        &self,
        iter:        &mut BtreeIter<'a>,
        node:        BtreeNodeRef,
        target:      u32,
        flags:       impl Into<CommitOpts>,
        write_flags: c::bch_write_flags,
    ) -> Result<(), BchError> {
        let trans = self.trans();
        super::interior::node_rewrite(trans, &mut trans.paths(), iter, unsafe { &*node.as_ptr() }, target,
                                      flags.into(), write_flags)
    }

    /// The node @pos's pointer at @level of @btree points to, rewritten:
    /// bch2_btree_node_rewrite_pos().
    pub fn btree_node_rewrite_pos(
        &self,
        btree:       Btree,
        level:       u32,
        pos:         Bpos,
        target:      u32,
        flags:       impl Into<CommitOpts>,
        write_flags: c::bch_write_flags,
    ) -> Result<(), BchError> {
        assert!(level > 0, "btree_node_rewrite_pos() of a leaf");
        let mut node_iter = BtreeNodeIter::new(self, btree, pos, level + 1, level - 1, BtreeIterFlags::empty());
        let b = node_iter.peek_ref()?.expect("the node a btree node pointer points to");
        self.btree_node_rewrite(node_iter.iter(), b, target, flags, write_flags)
    }

    /// The node @k points to, at @level of @btree, rewritten - if @k is still
    /// its pointer; ENOENT if the node's been replaced since.
    pub fn btree_node_rewrite_key<V: ?Sized + BkeyVal>(&self, btree: Btree, level: u32, k: BkeySC<'_, V>,
                                                       flags: impl Into<CommitOpts>) -> Result<(), BchError> {
        let mut node_iter = BtreeNodeIter::new(self, btree, k.k.p, c::BTREE_MAX_DEPTH, level, BtreeIterFlags::empty());
        match node_iter.peek_ref()? {
            Some(b) if btree_ptr_hash_val(b.key_sc()) == btree_ptr_hash_val(k) =>
                self.btree_node_rewrite(node_iter.iter(), b, 0, flags, c::bch_write_flags::empty()),
            _ => Err(BchError::from(c::ENOENT)),
        }
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
        k.set_key_type(BkeyType::deleted);
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
        debug_assert!(self.k().key_type().is::<V>());
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

/// The low bits of the commit flags: how deep into the reserves the commit may
/// dip. Unset - stripe, 0 - is normal for the ENOSPC gate, so it usually isn't
/// set.
#[derive(Clone, Copy)]
pub struct Watermark(c::bch_watermark);

impl Watermark {
    pub const STRIPE:           Self = Watermark(c::bch_watermark::stripe);
    pub const NORMAL:           Self = Watermark(c::bch_watermark::normal);
    pub const COPYGC:           Self = Watermark(c::bch_watermark::copygc);
    pub const BTREE:            Self = Watermark(c::bch_watermark::btree);
    pub const BTREE_COPYGC:     Self = Watermark(c::bch_watermark::btree_copygc);
    pub const RECLAIM:          Self = Watermark(c::bch_watermark::reclaim);
    pub const INTERIOR_UPDATES: Self = Watermark(c::bch_watermark::interior_updates);
}

/// A C caller's: a struct's, say.
impl From<c::bch_watermark> for Watermark {
    fn from(w: c::bch_watermark) -> Self {
        Watermark(w)
    }
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

    pub(crate) fn get_watermark(self) -> c::bch_watermark {
        c::bch_watermark::from_bits(self.0 & Self::WATERMARK_MASK)
    }

    pub(crate) const fn to_c(self) -> c::bch_trans_commit_flags {
        c::bch_trans_commit_flags::from_bits_retain(self.0)
    }

    /// A btree update's watermark: copygc's becomes btree_copygc, and below
    /// btree, btree - btree_update_set_watermark_hipri().
    pub fn watermark_hipri(self) -> Self {
        use c::bch_watermark as wm;
        let w = match wm::from_bits(self.0 & Self::WATERMARK_MASK) {
            wm::copygc                         => wm::btree_copygc,
            w if (w as u32) < wm::btree as u32 => wm::btree,
            w                                                => w,
        };
        self.watermark(Watermark(w))
    }
}

impl From<CommitFlags> for CommitOpts {
    fn from(flags: CommitFlags) -> Self {
        CommitOpts::new().flags(flags)
    }
}

impl ToText for CommitOpts {
    fn to_text(&self, out: &mut Printbuf) {
        let watermark = self.get_watermark();
        let flags = c::bch_trans_commit_flags::from_bits_retain(self.0 & !Self::WATERMARK_MASK);

        crate::prt!(out, "watermark={watermark}");
        if !flags.is_empty() {
            crate::prt!(out, " {flags}");
        }
    }
}

/// A C caller's: a struct's, say.
impl From<c::bch_trans_commit_flags> for CommitOpts {
    fn from(flags: c::bch_trans_commit_flags) -> Self {
        CommitOpts(flags.bits())
    }
}

