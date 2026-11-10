// SPDX-License-Identifier: GPL-2.0

//! TransAttempt: an attempt at a transaction, which btree work is done in.
//!
//! A transaction (BtreeTrans, owned by a TransGuard) runs as attempts, each
//! from one begin() to the next. The loops in for_each.rs - lockrestart_do(),
//! commit_do(), the for_each walks - begin an attempt and run a step in it,
//! and begin again if the step returns a restart. Everything a step does -
//! peeking keys, queueing updates, allocating transaction memory, committing -
//! takes the &TransAttempt it runs in, and a restart is an error like any
//! other, returned to the loop.
//!
//! In this file:
//!  - TransAttempt, and TransRef: what an iterator is made from - the
//!    transaction, begun first, or an attempt.
//!  - The attempt's operations: commits; work that runs loops of its own
//!    inside the attempt (nested(), close(), ignore_restarts(),
//!    restarts_handled()); dropping the locks to block (drop_locks_do(),
//!    allocate_dropping_locks()); transaction memory, and keys made in it;
//!    btree node helpers.
//!  - TransBkey: a key in transaction memory, or in a buffer C hands over,
//!    edited in place.
//!  - Watermark and CommitOpts: the commit's flags word.
//!
//! What an attempt hands out - memory, keys, iterator positions - is good
//! until the transaction next begins. A begin inside an attempt ends it: the
//! attempt must then finish with a restart, nothing borrowed from it used
//! again. That's checked only at runtime, by verify_not_restarted() and a
//! spent attempt's asserts.

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
use crate::types::Le64;
use crate::util::ffi::Opaque;
use crate::util::{Printbuf, ToText};
use core::cell::Cell;
use core::fmt;
use core::marker::PhantomData;
use core::mem::{size_of, ManuallyDrop};
use core::ptr::NonNull;
use core::slice;
use crate::btree::cache::btree_ptr_hash_val;
use crate::btree::node::iter::KeyIter;
use crate::btree::write::{node_write_trans, BtreeWriteFlags};
use crate::cstructs::c::BtreeWriteType;
use crate::util::vstructs::{State, Validated};
use crate::format::{Bpos, Btree, BkeyType};

/// One attempt at @trans: from one begin() to the next.
pub struct TransAttempt<'a, 't> {
    trans:         &'a BtreeTrans<'t>,
    /// the transaction's when the attempt began - or when restarts_handled()
    restart_count: Cell<u32>,
    /// ended by ignore_restarts()
    spent:         Cell<bool>,
    /// counted in the transaction's attempts_live: until dropped, or ended
    /// for work that runs loops of its own
    live:          Cell<bool>,
    t:             PhantomData<&'a mut ()>,
}

impl<'a, 't> TransAttempt<'a, 't> {
    /// For @trans's begin(), and C's attempt in progress.
    pub(super) fn new(trans: &'a BtreeTrans<'t>, restart_count: u32) -> Self {
        trans.set_attempts_live(trans.attempts_live() + 1);
        TransAttempt {
            trans,
            restart_count: Cell::new(restart_count),
            spent:         Cell::new(false),
            live:          Cell::new(true),
            t:             PhantomData,
        }
    }

    fn end(&self) {
        if self.live.replace(false) {
            self.trans.set_attempts_live(self.trans.attempts_live() - 1);
        }
    }
}

impl Drop for TransAttempt<'_, '_> {
    fn drop(&mut self) {
        self.end();
    }
}

/// What an iterator is made from. An iterator can't be made with the
/// transaction unlocked or in a restart: made from the transaction, it's begun
/// first if it's either; from an attempt, it's the attempt's. @'t bounds the
/// iterator by the transaction it points into.
///
/// XXX: a &BtreeTrans may be an attempt's t.trans(), and then that begin is a
/// nested one - warned of at runtime, with an attempt live, not prevented.
pub trait TransRef<'t> {
    fn trans_raw(&self) -> *mut BtreeTrans<'_>;
}

fn begin_if_unusable(trans: &BtreeTrans<'_>) {
    crate::warn_once!(trans.attempts_live() != 0, trans.fs(),
                      "{}: an iterator made from the transaction inside an attempt, not from the attempt",
                      trans.fn_name());

    if trans.restarted().0 != 0 || !trans.locked() {
        trans.begin_raw();
    }
}

impl<'i, 'f> TransRef<'i> for &'i BtreeTrans<'f> {
    fn trans_raw(&self) -> *mut BtreeTrans<'_> {
        begin_if_unusable(self);
        self.raw()
    }
}

impl<'i, 'f> TransRef<'i> for &'i TransGuard<'f> {
    fn trans_raw(&self) -> *mut BtreeTrans<'_> {
        begin_if_unusable(self);
        self.raw()
    }
}

impl<'a, 't> TransRef<'a> for &TransAttempt<'a, 't> {
    fn trans_raw(&self) -> *mut BtreeTrans<'_> { self.trans.raw() }
}

/// Exempts begin() from the dropped-updates warning while it lives; restores
/// the previous value when dropped, so exemptions nest.
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

/// A key to update with, ours to edit while the attempt runs: in its buffer,
/// never moved - what won't fit is NoRoom. Typed, its value is a whole @V;
/// untyped, any key.
pub struct TransBkey<'a, 't, V: ?Sized = FamilyVal<AllTypes>> {
    ptr:      NonNull<c::bkey_i>,
    pub(crate) buf_u64s: u32,
    t:        PhantomData<&'a mut TransAttempt<'a, 't>>,
    v:        PhantomData<*const V>,
}

/// A key that won't fit: @need_u64s, header and all, where its buffer - or a
/// key's u8 of u64s - has @have_u64s.
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
        unsafe { self.trans.disk_res.get().as_ref() }
    }

    pub fn verify_not_restarted(&self) {
        if !self.spent.get() {
            self.trans.verify_not_restarted(self.restart_count.get());
        }
    }

    /// The restarts since the attempt began were a callee's, handled inside it
    /// - a read's own retries, under a data move - and this attempt goes on.
    pub fn restarts_handled(&self) {
        self.restart_count.set(self.trans.restart_count());
    }

    /// Run @f - loops of its own - from inside this attempt, which @f's first
    /// begin ends: after @f the attempt is spent, and the loop running it
    /// begins anew (is_spent()).
    ///
    /// Checked: nothing queued before @f, and no relock or commit after.
    /// Not checked: nothing borrowed from the attempt - a key, which points
    /// into a node @f's attempts may have rewritten or freed - is used after.
    pub fn ignore_restarts<T>(&self, f: impl FnOnce(&TransGuard<'t>) -> Result<T, BchError>)
        -> Result<T, BchError>
    {
        assert!(!self.has_updates(), "ignore_restarts() with updates queued: @f's begin would drop them");
        self.spent.set(true);
        self.end();
        f(&self.lend_owner())
    }

    /// The transaction's owner, for work an ended attempt runs loops in: lent,
    /// not owned - the guard it came from puts it.
    fn lend_owner(&self) -> ManuallyDrop<TransGuard<'t>> {
        ManuallyDrop::new(unsafe { TransGuard::from_raw(self.trans.raw()) })
    }

    /// Ended by ignore_restarts(): the loop running it must begin anew.
    pub fn is_spent(&self) -> bool {
        self.spent.get()
    }

    /// For what a step does to the transaction itself - a write buffer flush,
    /// a nested loop - which ends this attempt.
    pub fn trans(&self) -> &'a BtreeTrans<'t> {
        self.trans
    }

    pub fn unlock(&self) {
        self.trans.unlock()
    }

    pub fn unlock_long(&self) {
        self.trans.unlock_long()
    }

    pub fn closure_sync_timeout(&self, cl: &Opaque<c::closure>, timeout: core::ffi::c_long) -> bool {
        self.trans.closure_sync_timeout(cl, timeout)
    }

    pub fn wait_until<T>(&self, ev: &crate::util::futex::EventCount, cond: impl FnMut() -> Option<T>) -> T {
        self.trans.wait_until(ev, cond)
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

    pub fn fn_name(&self) -> &str {
        self.trans.fn_name()
    }

    pub fn reserve_extra_journal_u64s(&self, u64s: u32) {
        self.trans.reserve_extra_journal_u64s(u64s)
    }

    /// Whether anything is queued for the next commit: key updates, journal
    /// entries or accounting.
    pub fn has_updates(&self) -> bool {
        self.trans.has_updates()
    }

    // ── The attempt's operations ─────────────────────────────────────────

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

    /// End this attempt for @f, work that runs loops of its own: commit what's
    /// queued, so @f's first begin drops nothing, run @f, and return the
    /// restart for the caller to return - `return Err(t.close(..))` - or the
    /// commit's or @f's error. Keys peeked in this attempt aren't used after.
    #[must_use]
    pub fn close<F>(&self, flags: impl Into<CommitOpts>, f: F) -> BchError
    where
        F: FnOnce(&TransGuard<'t>) -> Result<(), BchError>,
    {
        if self.has_updates() {
            if let Err(e) = self.commit(None, flags) {
                return e;
            }
        }
        self.end();
        if let Err(e) = f(&self.lend_owner()) {
            return e;
        }
        self.trans.begin().restart(bch_errcode::BCH_ERR_transaction_restart_nested)
    }

    /// End this attempt for @f, work that runs loops of its own, lending it the
    /// transaction's owner - which the loops take: the restart for the caller
    /// to return - `return Err(t.owner_and_restart(..))` - or @f's error.
    /// Nothing may be queued: @f's first begin would drop it.
    #[must_use]
    pub fn owner_and_restart<F>(&self, f: F) -> BchError
    where
        F: FnOnce(&TransGuard<'t>) -> Result<(), BchError>,
    {
        assert!(!self.has_updates(), "owner_and_restart() with updates queued: @f's begin would drop them");
        self.spent.set(true);
        self.end();

        if let Err(e) = f(&self.lend_owner()) {
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
    /// back: relock_or_nested(). @f's error is returned with the locks still
    /// dropped. A key peeked before is good after only if this succeeds, and
    /// not inside @f at all; nothing checks either.
    pub fn drop_locks_do<T>(&self, f: impl FnOnce() -> Result<T, BchError>) -> Result<T, BchError> {
        self.unlock();
        let v = f()?;
        self.relock_or_nested()?;
        Ok(v)
    }

    /// Retake the locks after what dropped them: a restart if they changed
    /// hands, or transaction_restart_nested, not relocked, if what dropped
    /// them ran attempts of its own.
    pub fn relock_or_nested(&self) -> Result<(), BchError> {
        if self.trans.restart_count() != self.restart_count.get() {
            return Err(BchError::from(bch_errcode::BCH_ERR_transaction_restart_nested));
        }
        self.relock()
    }

    /// What @alloc makes: tried in a NORECLAIM scope with the locks held, and
    /// on ENOMEM again with them dropped. @alloc names its ENOMEM, thrown if
    /// the second try fails too; any other error is returned as is.
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

    /// Run @f in this attempt, retried on restart. If anything restarted, this
    /// attempt is over - its locks and queued updates gone - and once @f
    /// succeeds the caller gets transaction_restart_nested, for its loop to
    /// begin again. The dropped-updates warning is off while @f runs.
    ///
    /// XXX: nothing stops the caller using a key it peeked before a retry.
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
        unsafe { Ok(&mut *p) }
    }

    /// @init's T, in the attempt's memory. Never dropped: a T owning anything
    /// is emptied by whoever's done with it.
    pub fn alloc_init<T, E: Into<BchError>>(&self, init: impl ::pin_init::Init<T, E>) -> Result<&'a mut T, BchError> {
        // bch2_trans_kmalloc() gives 8 byte alignment:
        const { assert!(core::mem::align_of::<T>() <= 8) };

        let p = self.kmalloc_raw(size_of::<T>())?.cast::<T>();
        unsafe { crate::util::alloc::init_at(init, p) }.map_err(Into::into)?;
        Ok(unsafe { &mut *p })
    }

    /// @bytes of zeroed transaction memory, rounded up to u64s: off the top of
    /// the transaction's buffer, or the transaction restarted to grow it.
    fn kmalloc_raw(&self, bytes: usize) -> Result<*mut u8, BchError> {
        let size = bytes.next_multiple_of(size_of::<u64>());
        let t = self.trans;

        /*
         * Growing borrows the path table - it may drop locks to allocate: a
         * caller holding it must hear so when it doesn't grow, too
         */
        ebug_assert!(!t.paths_borrowed(), "{}: allocating with the path table borrowed - kmalloc_in() takes it",
                     t.fn_name());

        match t.mem_with(|m| m.alloc(size / size_of::<u64>())) {
            Some(p) => Ok(p.as_ptr().cast()),
            None    => Err(t.kmalloc_grow(size, crate::ret_ip!())),
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

        Ok(unsafe { k.whole_unchecked() })
    }

    pub fn bkey_copy(&self, k: &BkeyI) -> Result<TransBkey<'a, 't>, BchError> {
        let dst = self.bkey_alloc(k.k.u64s as u32)?;
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
        Ok(unsafe { k.arm_unchecked::<K>() })
    }

    /// A copy of @k, as long as it was, in a buffer of at least @min_bytes -
    /// room for a callee to grow it - and a u64 more past the end, for
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

    /// The pointer at @level of @btree, at @pos, edited by @f - handed a copy
    /// of it, with room for any pointer - and committed: the node's key
    /// updated in place.
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
    /// committed, its triggers not run: for a repair or an injection that does
    /// the accounting itself, or none.
    pub fn btree_node_update_key_at_pos_raw(&self, btree: Btree, level: u32, key: TransBkey<'_, 't>)
        -> Result<(), BchError>
    {
        let mut node_iter = BtreeNodeIter::new(self, btree, key.k().p, 0, level - 1, BtreeIterFlags::INTENT);
        let b = node_iter.peek_ref()?.expect("the node a btree node pointer points to");
        self.btree_node_update_key(node_iter.iter(), b, key, CommitFlags::NO_ENOSPC, true)
    }

    /// @key put in the node at @level of @btree that its position is in, as
    /// it is - a deleted key takes out what's there - and the node left dirty:
    /// no interior update, nothing journalled, the nodes below left alone.
    /// Damage for check_topology to find: kvdb's set -L and copy -L. Keys
    /// injected before the node's written share a bset.
    ///
    /// # Safety
    /// Nothing checks @key against its node's range or its neighbours': the
    /// btree is left however @key breaks it, for fsck to repair. Only for a
    /// filesystem being damaged on purpose.
    ///
    /// The caller writes the node, with btree_node_write_at(): with no
    /// journal pin, going read-only doesn't.
    pub unsafe fn btree_node_inject_key(&self, btree: Btree, level: u32, mut key: TransBkey<'_, 't>)
        -> Result<(), BchError>
    {
        let trans = self.trans();
        let fs = self.fs();
        let lvl = level as usize;

        let mut iter = BtreeNodeIter::new(self, btree, key.k().p, level + 1, level, BtreeIterFlags::INTENT);
        if iter.peek()?.is_none() {
            return Err(fs.err(bch_errcode::BCH_ERR_no_btree_node_shallow));
        }

        let idx = iter.raw.path;
        let mut paths = trans.paths();
        let b = paths.held_btree_node(idx, lvl).expect("a node, peeked");

        trans.node_lock_write(&mut paths, idx, lvl)?;
        trans.node_prep_for_write(&mut paths, b.node());

        if usize::from(key.k().u64s) > b.keys_u64s_remaining(b.contents()) {
            trans.node_unlock_write(&mut paths, idx, lvl);
            return Err(fs.err(bch_errcode::BCH_ERR_btree_insert_btree_node_full));
        }

        let mut node_iter = KeyIter::default();
        node_iter.init(fs, &b.keys(b.contents()), &key.k().p);

        unsafe {
            trans.bset_insert_key(&mut paths, idx, b.node(), &mut node_iter, key.as_mut());

            let contents = b.node().contents_mut();
            let set = contents.last_set().data_offset;
            let mut keys = b.node().keys_mut(contents);
            let i = keys.bset_head_mut(set);
            i.journal_seq = Le64::new(fs.journal.lock.seq_ondisk.get().max(i.journal_seq.get()));

            fs.btree_node_set_dirty(b.node());
        }
        trans.node_unlock_write(&mut paths, idx, lvl);
        Ok(())
    }

    /// The node at @level of @btree that @pos is in written, if it's dirty:
    /// btree_node_inject_key()'s, once its keys are in.
    pub fn btree_node_write_at(&self, btree: Btree, level: u32, pos: Bpos) -> Result<(), BchError> {
        let trans = self.trans();

        let mut iter = BtreeNodeIter::new(self, btree, pos, level + 1, level, BtreeIterFlags::INTENT);
        if iter.peek()?.is_none() {
            return Err(self.fs().err(bch_errcode::BCH_ERR_no_btree_node_shallow));
        }

        let idx = iter.raw.path;
        let mut paths = trans.paths();
        let b = paths.held_btree_node(idx, level as usize).expect("a node, peeked");

        node_write_trans(trans, &mut paths, b.node(), c::six_lock_type::SIX_LOCK_intent,
                         BtreeWriteType::interior, BtreeWriteFlags::empty());
        Ok(())
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

    /// The node @pos's pointer at @level of @btree points to, rewritten.
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
        unsafe { &(*self.ptr.as_ptr()).k }
    }

    pub fn k_mut(&mut self) -> &mut c::bkey {
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
        unsafe { slice::from_raw_parts(self.ptr.as_ptr() as *const u64, self.buf_u64s as usize) }
    }

    pub fn as_mut_u64s(&mut self) -> &mut [u64] {
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
        unsafe { &*self.ptr.as_ptr().cast() }
    }

    /// key(), to edit.
    pub fn key_mut(&mut self) -> &mut BkeyI<V> {
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
        BkeySC { k: unsafe { &(*p).k }, v: unsafe { &raw const (*p).v }, iter: PhantomData, val: PhantomData }
    }
}

/// The key, to edit, keeping its type.
impl<'k, V: ?Sized + BkeyVal> From<&'k mut TransBkey<'_, '_, V>> for BkeyS<'k, V> {
    fn from(k: &'k mut TransBkey<'_, '_, V>) -> Self {
        let p = k.ptr.as_ptr();
        BkeyS { k: unsafe { &mut (*p).k }, v: unsafe { &raw mut (*p).v }, val: PhantomData }
    }
}

/// The key, all of it, as long as its u64s say - a validated key's, as
/// BkeyI's AsRef. Its length is its u64s when it's taken: one that changes
/// them - grows the key, in its buffer - takes it again after.
impl AsRef<BkeyI> for TransBkey<'_, '_> {
    fn as_ref(&self) -> &BkeyI {
        unsafe { &*cast_unsized(self.ptr.as_ptr()) }
    }
}

impl AsMut<BkeyI> for TransBkey<'_, '_> {
    fn as_mut(&mut self) -> &mut BkeyI {
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
/// dip. Usually unset: 0, stripe, is normal for the ENOSPC gate.
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

impl From<c::bch_watermark> for Watermark {
    fn from(w: c::bch_watermark) -> Self {
        Watermark(w)
    }
}

/// A Watermark and CommitFlags, in C's one word. The commits take
/// `impl Into<CommitOpts>`, so CommitFlags alone do: a CommitOpts is only
/// needed for a watermark.
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

impl From<c::bch_trans_commit_flags> for CommitOpts {
    fn from(flags: c::bch_trans_commit_flags) -> Self {
        CommitOpts(flags.bits())
    }
}

