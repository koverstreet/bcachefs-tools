use super::bkey::*;
use crate::alloc::buckets::DiskReservation;
use crate::c;
use crate::errcode::{
    BchError,
    bch_err_throw,
    bch_errcode,
    errptr_to_result,
    errptr_to_result_c,
    ret_to_result_void as ret_to_result,
};
use crate::fs::Fs;
#[cfg(feature = "std")]
use crate::printbuf_to_formatter;
use crate::SPOS_MAX;
use bitflags::bitflags;
#[cfg(feature = "std")]
use core::fmt;
use core::marker::PhantomData;
use core::mem::{size_of, MaybeUninit};
use core::ptr::NonNull;
use core::slice;
use core::ops::{ControlFlow, Deref};

use c::bpos;

pub struct BtreeTrans<'f> {
    raw: *mut c::btree_trans,
    fs:  &'f Fs,
}

impl<'f> BtreeTrans<'f> {
    pub fn new(fs: &'f Fs) -> BtreeTrans<'f> {
        unsafe {
            BtreeTrans {
                raw: &mut *c::__bch2_trans_get(fs.raw, 0),
                fs,
            }
        }
    }

    fn begin_raw(&self) -> u32 {
        unsafe { c::bch2_trans_begin(self.raw) }
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

    /// Get the raw transaction pointer for passing to C functions.
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

    /// Commit the transaction.
    ///
    /// Equivalent to the static inline bch2_trans_commit() which sets
    /// disk_res/journal_seq then calls __bch2_trans_commit().
    pub fn commit(
        &self,
        disk_res: Option<&DiskReservation<'_>>,
        flags: CommitOpts,
    ) -> Result<(), BchError> {
        unsafe {
            (*self.raw).disk_res = disk_res.map_or(core::ptr::null_mut(), |r| r.as_mut_ptr());
        }
        let ret = unsafe {
            c::__bch2_trans_commit(self.raw, flags.to_c(), false)
        };
        crate::errcode::ret_to_result(ret).map(|_| ())
    }
}

impl<'f> Drop for BtreeTrans<'f> {
    fn drop(&mut self) {
        unsafe {
            // Clear any pending restart state — bch2_trans_put() BUG_ONs
            // if the transaction is in restart, which can happen if Rust
            // code propagates a restart error via ? and unwinds.
            self.begin_raw();
            c::bch2_trans_put(&mut *self.raw)
        }
    }
}

pub struct TransAttempt<'a, 't> {
    trans:         &'a BtreeTrans<'t>,
    restart_count: u32,
    t:             PhantomData<&'a mut ()>,
}

pub enum TransError {
    Restart(BchError),
    Error(BchError),
}

pub type TransResult<'a, 't, T = ()> = Result<(TransAttempt<'a, 't>, T), TransError>;

impl From<BchError> for TransError {
    fn from(error: BchError) -> Self {
        if error.matches(bch_errcode::BCH_ERR_transaction_restart) {
            TransError::Restart(error)
        } else {
            TransError::Error(error)
        }
    }
}

fn retry_restart<T>(result: Result<T, TransError>) -> Result<Option<T>, BchError> {
    match result {
        Ok(v)                       => Ok(Some(v)),
        Err(TransError::Restart(_)) => Ok(None),
        Err(TransError::Error(e))   => Err(e),
    }
}

pub struct TransBkey<'a, 't> {
    ptr:      NonNull<c::bkey_i>,
    buf_u64s: u32,
    t:        PhantomData<&'a mut TransAttempt<'a, 't>>,
}

pub(crate) struct BtreeNodeRef {
    raw: NonNull<c::btree>,
}

impl BtreeNodeRef {
    pub(crate) fn level(&self) -> u32 {
        unsafe { self.raw.as_ref().c.level.into() }
    }

    pub(crate) fn key(&self) -> &c::bkey_i {
        unsafe { &self.raw.as_ref().key }
    }

    pub(crate) fn key_sc(&self) -> BkeySC<'_> {
        self.key().into()
    }

    fn as_ptr(&self) -> *mut c::btree {
        self.raw.as_ptr()
    }
}

impl<'a, 't> TransAttempt<'a, 't> {
    pub fn trans(&self) -> &'a BtreeTrans<'t> {
        self.trans
    }

    /// Get the raw transaction pointer for passing to C functions.
    pub fn raw(&self) -> *mut c::btree_trans {
        self.trans.raw()
    }

    pub(crate) fn fs(&self) -> &Fs {
        self.trans.fs()
    }

    pub fn verify_not_restarted(&self) {
        self.trans.verify_not_restarted(self.restart_count);
    }

    pub fn commit(
        self,
        disk_res: Option<&DiskReservation<'_>>,
        flags:    CommitOpts,
    ) -> Result<Self, TransError> {
        unsafe {
            (*self.raw()).disk_res = disk_res.map_or(core::ptr::null_mut(), |r| r.as_mut_ptr());
        }
        // `lazy = false`: this is the regular commit, mirroring the C
        // bch2_trans_commit() inline (the lazy variant is a separate path).
        let ret = unsafe { c::__bch2_trans_commit(self.raw(), flags.to_c(), false) };
        self.result(ret)
    }

    pub fn result(self, ret: i32) -> Result<Self, TransError> {
        ret_to_result(ret)?;
        Ok(self)
    }

    pub fn restart(self, error: bch_errcode) -> TransError {
        TransError::Restart(bch_err_throw(error))
    }

    pub fn done<T>(self, value: T) -> TransResult<'a, 't, T> {
        Ok((self, value))
    }

    pub fn result_value<T>(self, result: Result<T, BchError>) -> TransResult<'a, 't, T> {
        self.done(result?)
    }

    pub fn try_do<F>(self, f: F) -> Result<Self, TransError>
    where
        F: FnOnce(&BtreeTrans<'t>) -> Result<(), BchError>,
    {
        f(self.trans)?;
        Ok(self)
    }

    pub fn bkey_alloc(&self, u64s: u32) -> Result<TransBkey<'a, 't>, BchError> {
        let bytes = u64s as usize * size_of::<u64>();
        let ptr = unsafe { c::bch2_trans_kmalloc(self.raw(), bytes) };
        let ptr = errptr_to_result(ptr)? as *mut c::bkey_i;

        Ok(TransBkey {
            ptr:      NonNull::new(ptr).expect("bch2_trans_kmalloc returned NULL"),
            buf_u64s: u64s,
            t:        PhantomData,
        })
    }

    pub fn bkey_alloc_typed<K: BkeyInit>(&self) -> Result<TransBkey<'a, 't>, BchError> {
        debug_assert_eq!(size_of::<K>() % size_of::<u64>(), 0);

        let u64s = (size_of::<K>() / size_of::<u64>()) as u32;
        let mut k = self.bkey_alloc(u64s)?;

        unsafe {
            let raw = k.as_mut() as *mut c::bkey_i as *mut K;
            (*raw).init();
        }

        Ok(k)
    }

    pub fn bkey_copy(&self, k: &c::bkey_i) -> Result<TransBkey<'a, 't>, BchError> {
        let mut dst = self.bkey_alloc(k.k.u64s as u32)?;
        unsafe {
            core::ptr::copy_nonoverlapping(
                k as *const c::bkey_i as *const u8,
                dst.as_mut() as *mut c::bkey_i as *mut u8,
                k.k.u64s as usize * size_of::<u64>(),
            );
        }
        Ok(dst)
    }

    pub fn bkey_reassemble(&self, k: BkeySC<'_>) -> Result<TransBkey<'a, 't>, BchError> {
        const BKEY_U64S: usize = size_of::<c::bkey>() / size_of::<u64>();

        let mut dst = self.bkey_alloc(k.k.u64s as u32)?;
        let dst_key: &mut c::bkey_i = dst.as_mut();

        unsafe {
            core::ptr::copy_nonoverlapping(k.k, &mut dst_key.k, 1);
            core::ptr::copy_nonoverlapping(
                k.v as *const c::bch_val as *const u64,
                &mut dst.as_mut_u64s()[BKEY_U64S] as *mut u64,
                k.k.u64s as usize - BKEY_U64S,
            );
        }

        Ok(dst)
    }

    pub fn bkey_make_mut_noupdate(&self, k: BkeySC<'_>) -> Result<TransBkey<'a, 't>, BchError> {
        let raw = c::bkey_s_c {
            k: k.k,
            v: k.v,
        };
        let ptr = unsafe { c::bch2_bkey_make_mut_noupdate(self.raw(), raw) };
        let ptr = errptr_to_result(ptr)?;
        let u64s = unsafe { (*ptr).k.u64s as u32 };

        Ok(TransBkey {
            ptr:      NonNull::new(ptr).expect("bch2_bkey_make_mut_noupdate returned NULL"),
            buf_u64s: u64s,
            t:        PhantomData,
        })
    }

    pub fn update(
        self,
        iter:  &mut BtreeIter<'t>,
        key:   TransBkey<'_, 't>,
        flags: UpdateTriggerFlags,
    ) -> Result<Self, TransError> {
        let ret = unsafe {
            c::bch2_trans_update_buf(
                self.raw(),
                &mut iter.raw,
                key.as_ptr(),
                key.buf_u64s,
                c::btree_iter_update_trigger_flags(flags.bits()),
            )
        };
        self.result(ret)
    }

    pub fn insert(
        self,
        btree: impl Into<u32>,
        key:   TransBkey<'_, 't>,
        flags: UpdateTriggerFlags,
    ) -> Result<Self, TransError> {
        let ret = unsafe {
            c::bch2_btree_insert_trans(
                self.raw(),
                c::btree_id::from_raw(btree.into()).expect("invalid btree id"),
                key.as_ptr(),
                c::btree_iter_update_trigger_flags(flags.bits()),
            )
        };
        self.result(ret)
    }

    pub fn insert_nonextent(
        self,
        btree: impl Into<u32>,
        key:   TransBkey<'_, 't>,
        flags: UpdateTriggerFlags,
    ) -> Result<Self, TransError> {
        let key_ref: &c::bkey_i = key.as_ref();
        let ret = unsafe {
            c::bch2_btree_insert_nonextent(
                self.raw(),
                c::btree_id::from_raw(btree.into()).expect("invalid btree id"),
                key.as_ptr(),
                key_ref.k.u64s as u32,
                c::btree_iter_update_trigger_flags(flags.bits()),
            )
        };
        self.result(ret)
    }

    pub fn delete_at(
        self,
        iter:  &mut BtreeIter<'t>,
        flags: UpdateTriggerFlags,
    ) -> Result<Self, TransError> {
        let ret = unsafe {
            c::bch2_btree_delete_at(
                self.raw(),
                iter.raw_mut(),
                c::btree_iter_update_trigger_flags(flags.bits()),
            )
        };
        self.result(ret)
    }

    pub(crate) fn btree_node_update_key(
        self,
        iter:          &mut BtreeIter<'t>,
        node:          BtreeNodeRef,
        key:           TransBkey<'_, 't>,
        flags:         CommitOpts,
        iter_searched: bool,
    ) -> Result<Self, TransError> {
        let ret = unsafe {
            c::bch2_btree_node_update_key(
                self.raw(),
                iter.raw_mut(),
                node.as_ptr(),
                key.as_ptr(),
                flags.bits(),
                iter_searched,
            )
        };
        self.result(ret)
    }

    pub fn delete(
        self,
        btree: impl Into<u32>,
        pos:   c::bpos,
        flags: UpdateTriggerFlags,
    ) -> Result<Self, TransError> {
        let ret = unsafe {
            c::bch2_btree_delete(
                self.raw(),
                c::btree_id::from_raw(btree.into()).expect("invalid btree id"),
                pos,
                c::btree_iter_update_trigger_flags(flags.bits()),
            )
        };
        self.result(ret)
    }

    pub fn snapshot_node_create(
        self,
        parent:           u32,
        new_snapids:      &mut [u32],
        snapshot_subvols: &[u32],
    ) -> Result<Self, TransError> {
        if new_snapids.len() != snapshot_subvols.len() {
            self.fs().throw(crate::errcode::invalid_snapshot_node)?;
        }

        let ret = unsafe {
            c::bch2_snapshot_node_create(
                self.raw(),
                parent,
                new_snapids.as_mut_ptr(),
                snapshot_subvols.as_ptr() as *mut u32,
                new_snapids.len() as u32,
            )
        };
        self.result(ret)
    }

    pub fn iter_traverse(self, iter: &mut BtreeIter<'t>) -> Result<Self, TransError> {
        let ret = unsafe { c::bch2_btree_iter_traverse(iter.raw_mut()) };
        self.result(ret)
    }
}

impl<'a, 't> TransBkey<'a, 't> {
    pub fn as_ptr(&self) -> *mut c::bkey_i {
        self.ptr.as_ptr()
    }

    pub fn k(&self) -> &c::bkey {
        let k: &c::bkey_i = self.as_ref();
        &k.k
    }

    pub fn k_mut(&mut self) -> &mut c::bkey {
        let k: &mut c::bkey_i = self.as_mut();
        &mut k.k
    }

    pub fn k_i(&self) -> &c::bkey_i {
        AsRef::<c::bkey_i>::as_ref(self)
    }

    pub fn k_i_mut(&mut self) -> &mut c::bkey_i {
        AsMut::<c::bkey_i>::as_mut(self)
    }

    pub fn as_u64s(&self) -> &[u64] {
        self.as_ref()
    }

    pub fn as_mut_u64s(&mut self) -> &mut [u64] {
        self.as_mut()
    }
}

impl AsRef<c::bkey_i> for TransBkey<'_, '_> {
    fn as_ref(&self) -> &c::bkey_i {
        unsafe { self.ptr.as_ref() }
    }
}

impl AsMut<c::bkey_i> for TransBkey<'_, '_> {
    fn as_mut(&mut self) -> &mut c::bkey_i {
        unsafe { self.ptr.as_mut() }
    }
}

impl AsRef<[u64]> for TransBkey<'_, '_> {
    fn as_ref(&self) -> &[u64] {
        unsafe { slice::from_raw_parts(self.ptr.as_ptr() as *const u64, self.buf_u64s as usize) }
    }
}

impl AsMut<[u64]> for TransBkey<'_, '_> {
    fn as_mut(&mut self) -> &mut [u64] {
        unsafe { slice::from_raw_parts_mut(self.ptr.as_ptr() as *mut u64, self.buf_u64s as usize) }
    }
}

impl<'a, 't> Deref for TransAttempt<'a, 't> {
    type Target = BtreeTrans<'t>;

    fn deref(&self) -> &Self::Target {
        self.trans
    }
}

bitflags! {
    pub struct BtreeIterFlags: u32 {
        const SLOTS = c::btree_iter_update_trigger_flags::BTREE_ITER_slots.0;
        const INTENT = c::btree_iter_update_trigger_flags::BTREE_ITER_intent.0;
        const PREFETCH = c::btree_iter_update_trigger_flags::BTREE_ITER_prefetch.0;
        const IS_EXTENTS = c::btree_iter_update_trigger_flags::BTREE_ITER_is_extents.0;
        const NOT_EXTENTS = c::btree_iter_update_trigger_flags::BTREE_ITER_not_extents.0;
        const CACHED = c::btree_iter_update_trigger_flags::BTREE_ITER_cached.0;
        const KEY_CACHED = c::btree_iter_update_trigger_flags::BTREE_ITER_with_key_cache.0;
        const WITH_JOURNAL = c::btree_iter_update_trigger_flags::BTREE_ITER_with_journal.0;
        const SNAPSHOT_FIELD = c::btree_iter_update_trigger_flags::BTREE_ITER_snapshot_field.0;
        const ALL_SNAPSHOTS = c::btree_iter_update_trigger_flags::BTREE_ITER_all_snapshots.0;
        const FILTER_SNAPSHOTS = c::btree_iter_update_trigger_flags::BTREE_ITER_filter_snapshots.0;
        const NOFILTER_WHITEOUTS = c::btree_iter_update_trigger_flags::BTREE_ITER_nofilter_whiteouts.0;
        const NOPRESERVE = c::btree_iter_update_trigger_flags::BTREE_ITER_nopreserve.0;
        const CACHED_NOFILL = c::btree_iter_update_trigger_flags::BTREE_ITER_cached_nofill.0;
        const KEY_CACHE_FILL = c::btree_iter_update_trigger_flags::BTREE_ITER_key_cache_fill.0;
    }
}

bitflags! {
    /// The `BTREE_UPDATE_*` / `BTREE_TRIGGER_*` half of the C
    /// `btree_iter_update_trigger_flags` enum: the flags that control how an
    /// update commits and which triggers it runs. These combine freely with each
    /// other but never with the iter/str_hash half (verified against the C tree),
    /// so they get their own type. (The str_hash flags, which *do* combine with
    /// iter flags, belong with `BtreeIterFlags` and will join it when str_hash is
    /// converted to Rust.)
    pub struct UpdateTriggerFlags: u32 {
        const INTERNAL_SNAPSHOT_NODE   = c::btree_iter_update_trigger_flags::BTREE_UPDATE_internal_snapshot_node.0;
        const NOJOURNAL                = c::btree_iter_update_trigger_flags::BTREE_UPDATE_nojournal.0;
        const KEY_CACHE_RECLAIM        = c::btree_iter_update_trigger_flags::BTREE_UPDATE_key_cache_reclaim.0;
        const NORUN                    = c::btree_iter_update_trigger_flags::BTREE_TRIGGER_norun.0;
        const TRANSACTIONAL            = c::btree_iter_update_trigger_flags::BTREE_TRIGGER_transactional.0;
        const ATOMIC                   = c::btree_iter_update_trigger_flags::BTREE_TRIGGER_atomic.0;
        const GC                       = c::btree_iter_update_trigger_flags::BTREE_TRIGGER_gc.0;
        const INSERT                   = c::btree_iter_update_trigger_flags::BTREE_TRIGGER_insert.0;
        const OVERWRITE                = c::btree_iter_update_trigger_flags::BTREE_TRIGGER_overwrite.0;
        const IS_DISCARD               = c::btree_iter_update_trigger_flags::BTREE_TRIGGER_is_discard.0;
        const SET_NEEDS_RECONCILE_DONE = c::btree_iter_update_trigger_flags::BTREE_TRIGGER_set_needs_reconcile_done.0;
    }
}

bitflags! {
    /// The flag half of the C `bch_trans_commit_flags` word — the bits above the
    /// watermark. Composed onto a [`CommitOpts`] via [`CommitOpts::flags`].
    pub struct CommitFlags: u32 {
        const NO_ENOSPC             = c::bch_trans_commit_flags::BCH_TRANS_COMMIT_no_enospc.0;
        const NO_CHECK_RW           = c::bch_trans_commit_flags::BCH_TRANS_COMMIT_no_check_rw.0;
        const NO_JOURNAL_RES        = c::bch_trans_commit_flags::BCH_TRANS_COMMIT_no_journal_res.0;
        const NO_SKIP_NOOPS         = c::bch_trans_commit_flags::BCH_TRANS_COMMIT_no_skip_noops.0;
        const JOURNAL_RECLAIM       = c::bch_trans_commit_flags::BCH_TRANS_COMMIT_journal_reclaim.0;
        const JOURNAL_REPLAY        = c::bch_trans_commit_flags::BCH_TRANS_COMMIT_journal_replay.0;
        const SKIP_ACCOUNTING_APPLY = c::bch_trans_commit_flags::BCH_TRANS_COMMIT_skip_accounting_apply.0;
    }
}

/// Allocation watermark — the low bits of the commit-flags word, selecting how
/// deep into the reserves the commit may dip. Unset (`stripe`/0) the commit path
/// treats the same as `normal` for the ENOSPC gate, so it usually isn't set.
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

/// A commit-flags word, built from a [`Watermark`] (defaulting to `stripe`/0) and
/// a set of [`CommitFlags`]. The watermark usually isn't set, so the common path
/// is `CommitOpts::new()` or `CommitOpts::new().flags(...)`.
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

    pub(crate) const fn to_c(self) -> c::bch_trans_commit_flags {
        c::bch_trans_commit_flags(self.0)
    }
}

pub fn lockrestart_do<'t, T, F>(trans: &BtreeTrans<'t>, mut f: F) -> Result<T, BchError>
where
    F: for<'a> FnMut(TransAttempt<'a, 't>) -> TransResult<'a, 't, T>
{
    loop {
        let t = trans.begin();

        let Some((t, v)) = retry_restart(f(t))? else {
            continue;
        };

        t.verify_not_restarted();
        return Ok(v);
    }
}

/// Run a closure inside a transaction commit loop.
///
/// Equivalent to the C `commit_do` macro: runs the closure, and if it
/// succeeds, commits the transaction. Retries on transaction restart.
pub fn commit_do<'t, F>(
    trans: &BtreeTrans<'t>,
    disk_res: Option<&DiskReservation<'_>>,
    flags: CommitOpts,
    mut f: F,
) -> Result<(), BchError>
where
    F: for<'a> FnMut(TransAttempt<'a, 't>) -> Result<TransAttempt<'a, 't>, TransError>,
{
    lockrestart_do(trans, |t| {
        let t = f(t)?;
        let t = t.commit(disk_res, flags)?;
        t.done(())
    })
}

/// Create a transaction and run a closure with commit retry.
///
/// Equivalent to the C `bch2_trans_commit_do` macro.
pub fn trans_commit_do<'t, F>(
    fs: &'t Fs,
    disk_res: Option<&DiskReservation<'_>>,
    flags: CommitOpts,
    f: F,
) -> Result<(), BchError>
where
    F: for<'a> FnMut(TransAttempt<'a, 't>) -> Result<TransAttempt<'a, 't>, TransError>,
{
    let trans = BtreeTrans::new(fs);
    commit_do(&trans, disk_res, flags, f)
}

/// Create a transaction and run a closure with restart retry (no commit).
///
/// Equivalent to the C `bch2_trans_run` macro.
pub fn trans_run<'t, T, F>(fs: &'t Fs, f: F) -> Result<T, BchError>
where
    F: for<'a> FnMut(TransAttempt<'a, 't>) -> TransResult<'a, 't, T>,
{
    let trans = BtreeTrans::new(fs);
    lockrestart_do(&trans, f)
}

pub struct BtreeIter<'t> {
    raw:   c::btree_iter,
    trans: PhantomData<&'t BtreeTrans<'t>>,
}

fn bkey_s_c_to_result<'i>(k: c::bkey_s_c) -> Result<Option<BkeySC<'i>>, BchError> {
    errptr_to_result_c(k.k).map(|_| {
        if !k.k.is_null() {
            unsafe {
                Some(BkeySC {
                    k:    &*k.k,
                    // Hole slots (peek_slot) return a deleted key with a NULL
                    // val; bch_val is zero-sized, so a dangling well-aligned
                    // reference is legal - the val is never read through (a
                    // deleted key's val length is zero).
                    v:    if !k.v.is_null() {
                        &*k.v
                    } else {
                        NonNull::dangling().as_ref()
                    },
                    iter: PhantomData,
                })
            }
        } else {
            None
        }
    })
}

impl<'t> BtreeIter<'t> {
    pub fn uninit() -> BtreeIter<'t> {
        BtreeIter {
            raw:   Default::default(),
            trans: PhantomData,
        }
    }

    pub(crate) fn raw_mut(&mut self) -> *mut c::btree_iter {
        &mut self.raw
    }

    pub(crate) fn node_at_iter_level<'a>(
        &mut self,
        t: &TransAttempt<'a, 't>,
    ) -> Option<BtreeNodeRef> {
        unsafe {
            let path = c::btree_iter_path(t.raw(), self.raw_mut());
            let node = (*path).l[(*path).level() as usize].b;

            NonNull::new(node).map(|raw| BtreeNodeRef { raw })
        }
    }

    pub fn pos(&self) -> c::bpos {
        self.raw.pos
    }

    pub fn set_pos(&mut self, pos: c::bpos) {
        unsafe { c::bch2_btree_iter_set_pos(&mut self.raw, pos) };
    }

    pub fn set_pos_to_extent_start(&mut self) {
        unsafe { c::bch2_btree_iter_set_pos_to_extent_start(&mut self.raw) };
    }

    pub fn new(
        trans: &BtreeTrans<'t>,
        btree: impl Into<u32>,
        pos: bpos,
        flags: BtreeIterFlags,
    ) -> BtreeIter<'t> {
        unsafe {
            let mut iter: MaybeUninit<c::btree_iter> = MaybeUninit::uninit();

            c::bch2_trans_iter_init_outlined(
                trans.raw,
                iter.as_mut_ptr(),
                c::btree_id::from_raw(btree.into()).expect("invalid btree id"),
                pos,
                c::btree_iter_update_trigger_flags(flags.bits()),
                0
            );

            BtreeIter {
                raw:   iter.assume_init(),
                trans: PhantomData,
            }
        }
    }

    pub fn new_level(
        trans: &BtreeTrans<'t>,
        btree: impl Into<u32>,
        pos: bpos,
        level: u32,
        flags: BtreeIterFlags,
    ) -> BtreeIter<'t> {
        unsafe {
            let mut iter: MaybeUninit<c::btree_iter> = MaybeUninit::uninit();

            c::__bch2_trans_node_iter_init(
                trans.raw,
                iter.as_mut_ptr(),
                c::btree_id::from_raw(btree.into()).expect("invalid btree id"),
                pos,
                0,
                level,
                c::btree_iter_update_trigger_flags(flags.bits())
            );

            BtreeIter {
                raw:   iter.assume_init(),
                trans: PhantomData,
            }
        }
    }

    pub fn peek_max<'i>(&'i mut self, end: bpos) -> Result<Option<BkeySC<'i>>, BchError> {
        unsafe {
            bkey_s_c_to_result(c::bch2_btree_iter_peek_max(&mut self.raw, &end))
        }
    }

    pub fn peek_max_flags<'i>(&'i mut self, end: bpos, flags: BtreeIterFlags) ->
            Result<Option<BkeySC<'i>>, BchError> {
        unsafe {
            if flags.contains(BtreeIterFlags::SLOTS) {
                if bkey_le(self.raw.pos, end) {
                    bkey_s_c_to_result(c::bch2_btree_iter_peek_slot(&mut self.raw))
                } else {
                    Ok(None)
                }
            } else {
                bkey_s_c_to_result(c::bch2_btree_iter_peek_max(&mut self.raw, &end))
            }
        }
    }

    pub fn peek_max_type<'i>(&'i mut self, end: bpos, flags: BtreeIterFlags) ->
            Result<Option<BkeySC<'i>>, BchError> {
        unsafe {
            bkey_s_c_to_result(c::bch2_btree_iter_peek_max_type(
                &mut self.raw,
                end,
                c::btree_iter_update_trigger_flags(flags.bits()),
            ))
        }
    }

    pub fn peek(&mut self) -> Result<Option<BkeySC<'_>>, BchError> {
        self.peek_max(SPOS_MAX)
    }

    pub fn peek_prev_min<'i>(&'i mut self, min: bpos) -> Result<Option<BkeySC<'i>>, BchError> {
        unsafe {
            bkey_s_c_to_result(c::bch2_btree_iter_peek_prev_min(&mut self.raw, min))
        }
    }

    pub fn peek_prev_type<'i>(&'i mut self, flags: BtreeIterFlags) ->
            Result<Option<BkeySC<'i>>, BchError> {
        unsafe {
            bkey_s_c_to_result(c::bch2_btree_iter_peek_prev_type(
                &mut self.raw,
                c::btree_iter_update_trigger_flags(flags.bits()),
            ))
        }
    }

    pub fn peek_prev(&mut self) -> Result<Option<BkeySC<'_>>, BchError> {
        self.peek_prev_min(c::bpos { inode: 0, offset: 0, snapshot: 0 })
    }

    pub fn traverse<'a>(
        &mut self,
        t: TransAttempt<'a, 't>,
    ) -> Result<TransAttempt<'a, 't>, TransError> {
        let ret = unsafe { c::bch2_btree_iter_traverse(self.raw_mut()) };
        t.result(ret)
    }

    pub fn for_each_max<F>(&mut self, trans: &BtreeTrans<'_>, end: bpos, mut f: F)
        -> Result<(), BchError>
    where
        F: for<'a> FnMut(BkeySC<'a>) -> ControlFlow<()>,
    {
        // Respect the iterator's SLOTS flag: a slots iterator must visit every
        // slot — including holes — via peek_slot, not skip ahead to the next
        // present key via peek_max. Mirrors `peek_max_flags`.
        let slots = (self.raw.flags as u32) & BtreeIterFlags::SLOTS.bits() != 0;
        let raw = &mut self.raw as *mut c::btree_iter;
        loop {
            let t = trans.begin();
            let res = unsafe {
                if slots {
                    if bkey_le((*raw).pos, end) {
                        bkey_s_c_to_result(c::bch2_btree_iter_peek_slot(raw))
                    } else {
                        Ok(None)
                    }
                } else {
                    bkey_s_c_to_result(c::bch2_btree_iter_peek_max(raw, &end))
                }
            };

            match res {
                Err(e) if e.matches(bch_errcode::BCH_ERR_transaction_restart) => continue,
                Err(e) => return Err(e),
                Ok(None) => return Ok(()),
                Ok(Some(k)) => {
                    t.verify_not_restarted();
                    if let ControlFlow::Break(()) = f(k) {
                        return Ok(());
                    }
                }
            }
            // advance() returns false when the key just visited ended at
            // SPOS_MAX and the position can't move forward — true for the
            // rightmost key of any interior node level. Looping again would
            // peek the same key forever.
            if !unsafe { c::bch2_btree_iter_advance(raw) } {
                return Ok(());
            }
        }
    }

    pub fn for_each<F>(&mut self, trans: &BtreeTrans<'_>, f: F) -> Result<(), BchError>
    where
        F: for<'a> FnMut(BkeySC<'a>) -> ControlFlow<()>,
    {
        self.for_each_max(trans, SPOS_MAX, f)
    }

    pub fn for_each_commit<F>(
        &mut self,
        trans:       &BtreeTrans<'t>,
        disk_res:    Option<&DiskReservation<'_>>,
        flags:       CommitOpts,
        mut f:       F,
    ) -> Result<(), BchError>
    where
        F: for<'a, 'k> FnMut(
            TransAttempt<'a, 't>,
            BkeySC<'k>,
        ) -> Result<(TransAttempt<'a, 't>, ControlFlow<()>), TransError>,
    {
        let raw = &mut self.raw as *mut c::btree_iter;
        loop {
            let t = trans.begin();
            let k = unsafe { c::bch2_btree_iter_peek(raw) };

            match bkey_s_c_to_result(k) {
                Err(e) if e.matches(bch_errcode::BCH_ERR_transaction_restart) => continue,
                Err(e) => return Err(e),
                Ok(None) => return Ok(()),
                Ok(Some(k)) => {
                    let Some((t, flow)) = retry_restart(f(t, k))? else {
                        continue;
                    };
                    let Some(t) = retry_restart(t.commit(disk_res, flags))? else {
                        continue;
                    };

                    t.verify_not_restarted();
                    if let ControlFlow::Break(()) = flow {
                        return Ok(());
                    }
                }
            }

            if !unsafe { c::bch2_btree_iter_advance(raw) } {
                return Ok(());
            }
        }
    }

    pub fn for_each_max_commit<F>(
        &mut self,
        trans:       &BtreeTrans<'t>,
        end:         bpos,
        iter_flags:  BtreeIterFlags,
        disk_res:    Option<&DiskReservation<'_>>,
        flags:       CommitOpts,
        mut f:       F,
    ) -> Result<(), BchError>
    where
        F: for<'a, 'k> FnMut(
            TransAttempt<'a, 't>,
            BkeySC<'k>,
        ) -> Result<(TransAttempt<'a, 't>, ControlFlow<()>), TransError>,
    {
        let raw = &mut self.raw as *mut c::btree_iter;
        loop {
            let t = trans.begin();
            let k = unsafe {
                c::bch2_btree_iter_peek_max_type(
                    raw,
                    end,
                    c::btree_iter_update_trigger_flags(iter_flags.bits()),
                )
            };

            match bkey_s_c_to_result(k) {
                Err(e) if e.matches(bch_errcode::BCH_ERR_transaction_restart) => continue,
                Err(e) => return Err(e),
                Ok(None) => return Ok(()),
                Ok(Some(k)) => {
                    let Some((t, flow)) = retry_restart(f(t, k))? else {
                        continue;
                    };
                    let Some(t) = retry_restart(t.commit(disk_res, flags))? else {
                        continue;
                    };

                    t.verify_not_restarted();
                    if let ControlFlow::Break(()) = flow {
                        return Ok(());
                    }
                }
            }

            if !unsafe { c::bch2_btree_iter_advance(raw) } {
                return Ok(());
            }
        }
    }

    pub fn for_each_reverse<F>(&mut self, trans: &BtreeTrans<'_>, min: bpos, mut f: F)
        -> Result<(), BchError>
    where
        F: for<'a> FnMut(BkeySC<'a>) -> ControlFlow<()>,
    {
        let raw = &mut self.raw as *mut c::btree_iter;
        loop {
            let t = trans.begin();
            let k = unsafe { c::bch2_btree_iter_peek_prev_min(raw, min) };

            match bkey_s_c_to_result(k) {
                Err(e) if e.matches(bch_errcode::BCH_ERR_transaction_restart) => continue,
                Err(e) => return Err(e),
                Ok(None) => return Ok(()),
                Ok(Some(k)) => {
                    t.verify_not_restarted();
                    if let ControlFlow::Break(()) = f(k) {
                        return Ok(());
                    }
                }
            }
            if !unsafe { c::bch2_btree_iter_rewind(raw) } {
                return Ok(());
            }
        }
    }

    pub fn for_each_reverse_flags<F>(
        &mut self,
        trans: &BtreeTrans<'_>,
        flags: BtreeIterFlags,
        mut f: F,
    ) -> Result<(), BchError>
    where
        F: for<'a> FnMut(BkeySC<'a>) -> ControlFlow<()>,
    {
        let raw = &mut self.raw as *mut c::btree_iter;
        loop {
            let t = trans.begin();
            let k = unsafe {
                c::bch2_btree_iter_peek_prev_type(
                    raw,
                    c::btree_iter_update_trigger_flags(flags.bits()),
                )
            };

            match bkey_s_c_to_result(k) {
                Err(e) if e.matches(bch_errcode::BCH_ERR_transaction_restart) => continue,
                Err(e) => return Err(e),
                Ok(None) => return Ok(()),
                Ok(Some(k)) => {
                    t.verify_not_restarted();
                    if let ControlFlow::Break(()) = f(k) {
                        return Ok(());
                    }
                }
            }
            if !unsafe { c::bch2_btree_iter_rewind(raw) } {
                return Ok(());
            }
        }
    }

    pub fn advance(&mut self) {
        unsafe {
            c::bch2_btree_iter_advance(&mut self.raw);
        }
    }
}

impl<'t> Drop for BtreeIter<'t> {
    fn drop(&mut self) {
        unsafe { c::bch2_trans_iter_exit(&mut self.raw) }
    }
}

pub struct BtreeNodeIter<'t> {
    raw:   c::btree_iter,
    trans: PhantomData<&'t BtreeTrans<'t>>,
}

impl<'t> BtreeNodeIter<'t> {
    pub fn new(
        trans: &BtreeTrans<'t>,
        btree: impl Into<u32>,
        pos: bpos,
        locks_want: u32,
        depth: u32,
        flags: BtreeIterFlags,
    ) -> BtreeNodeIter<'t> {
        unsafe {
            let mut iter: MaybeUninit<c::btree_iter> = MaybeUninit::uninit();
            c::__bch2_trans_node_iter_init(
                trans.raw,
                iter.as_mut_ptr(),
                c::btree_id::from_raw(btree.into()).expect("invalid btree id"),
                pos,
                locks_want,
                depth,
                c::btree_iter_update_trigger_flags(flags.bits()),
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
        unsafe {
            bkey_s_c_to_result(c::bch2_btree_iter_peek_max_type(
                &mut self.raw,
                end,
                c::btree_iter_update_trigger_flags(flags.bits()),
            ))
        }
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
                Err(e) if e.matches(bch_errcode::BCH_ERR_transaction_restart) => continue,
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
            unsafe { c::bch2_btree_iter_set_pos(raw, c::bpos_successor(end)) };
        }
    }
}

impl<'t> Drop for BtreeNodeIter<'t> {
    fn drop(&mut self) {
        unsafe { c::bch2_trans_iter_exit(&mut self.raw) }
    }
}

#[cfg(feature = "std")]
impl<'b, 'f> c::btree {
    pub fn to_text(&'b self, fs: &'f Fs) -> BtreeNodeToText<'b, 'f> {
        BtreeNodeToText { b: self, fs }
    }

    pub fn ondisk_to_text(&'b self, fs: &'f Fs) -> BtreeNodeOndiskToText<'b, 'f> {
        BtreeNodeOndiskToText { b: self, fs }
    }
}

impl c::btree {
    /// Check if this btree node is a fake/placeholder node.
    pub fn is_fake(&self) -> bool {
        (self.flags >> c::btree_flags::BTREE_NODE_fake as u64) & 1 != 0
    }

    /// Iterate over unpacked keys within this btree node.
    ///
    /// Equivalent to the C `for_each_btree_node_key_unpack` macro.
    /// The callback receives each key in order; return `Break` to
    /// stop early.
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
                k: unsafe { &*k.k },
                v: unsafe { &*k.v },
                iter: PhantomData,
            }).is_break() {
                return ControlFlow::Break(());
            }
            unsafe { c::bch2_btree_node_iter_advance(&mut node_iter, b) };
        }
    }
}

#[cfg(feature = "std")]
pub struct BtreeNodeToText<'b, 'f> {
    b:  &'b c::btree,
    fs: &'f Fs,
}

#[cfg(feature = "std")]
impl<'b, 'f> fmt::Display for BtreeNodeToText<'b, 'f> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        printbuf_to_formatter(f, |buf| unsafe {
            c::bch2_btree_node_to_text(buf, self.fs.raw, self.b)
        })
    }
}

#[cfg(feature = "std")]
pub struct BtreeNodeOndiskToText<'b, 'f> {
    b:  &'b c::btree,
    fs: &'f Fs,
}

#[cfg(feature = "std")]
impl<'b, 'f> fmt::Display for BtreeNodeOndiskToText<'b, 'f> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        printbuf_to_formatter(f, |buf| unsafe {
            c::bch2_btree_node_ondisk_to_text(buf, self.fs.raw, self.b)
        })
    }
}
