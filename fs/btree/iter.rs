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
use crate::util::log::CFnName;
use crate::SPOS_MAX;
use bitflags::bitflags;
use core::fmt;
use core::marker::PhantomData;
use core::mem::{size_of, ManuallyDrop, MaybeUninit};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use core::ptr::NonNull;
use core::slice;
use core::ops::{ControlFlow, Deref};

use c::bpos;

/// btree_trans!(fs): a transaction named for the function it's created in, as
/// C's bch2_trans_get(). Transaction stats and lock contention are reported per
/// function, under that name; without it a transaction is "(unknown)".
///
/// XXX: C registers __func__ - a NUL-terminated string that lives forever -
/// so each call site carries a CFnName to copy its name into. Clean this up
/// once enough is converted to key the stats on Rust's own names.
#[macro_export]
macro_rules! btree_trans {
    ($fs:expr) => {{
        static FN: $crate::btree::iter::TransFnName = $crate::btree::iter::TransFnName::new();
        $crate::btree::iter::BtreeTrans::new_fn($fs, FN.idx($crate::function_name!()))
    }};
}

/// One btree_trans!() call site's registered name: see there.
#[doc(hidden)]
pub struct TransFnName {
    idx:   AtomicU32,
    claim: AtomicBool,
    name:  CFnName,
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

        let idx = unsafe { c::bch2_trans_get_fn_idx(self.name.get(name).as_ptr()) };
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
                raw: &mut *c::__bch2_trans_get(fs.raw, fn_idx),
                fs,
            }
        }
    }

    /// A transaction C owns, for Rust that C calls with one: as
    /// Fs::borrow_raw(), it isn't put when dropped.
    ///
    /// # Safety
    /// @raw is a live transaction on @fs, for as long as this lives.
    pub unsafe fn borrow_raw(fs: &'f Fs, raw: *mut c::btree_trans) -> ManuallyDrop<BtreeTrans<'f>> {
        ManuallyDrop::new(BtreeTrans { raw, fs })
    }

    /// How many times this transaction has committed: what's cached from the
    /// btree is stale once it moves on.
    pub fn commit_count(&self) -> u32 {
        unsafe { (*self.raw).commit_count }
    }

    /// The attempt C began, for Rust that C calls partway through one.
    pub(crate) fn attempt_in_progress<'a>(&'a self) -> TransAttempt<'a, 'f> {
        TransAttempt {
            trans:         self,
            restart_count: unsafe { (*self.raw).restart_count },
            t:             PhantomData,
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
        flags: impl Into<CommitOpts>,
    ) -> Result<(), BchError> {
        unsafe {
            (*self.raw).disk_res = disk_res.map_or(core::ptr::null_mut(), |r| r.as_mut_ptr());
        }
        let ret = unsafe {
            c::__bch2_trans_commit(self.raw, flags.into().to_c(), false)
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

/// A BchError, sorted by whether it's a transaction restart: the conversions
/// both ways are trivial.
#[derive(Clone, Copy)]
pub enum TransError {
    Restart(BchError),
    Error(BchError),
}

pub type TransResult<'a, 't, T = ()> = Result<(TransAttempt<'a, 't>, T), TransError>;

/// What a transaction step - a commit loop body, or a helper it calls -
/// returns: the attempt, to carry on with, or why not.
pub type TransRet<'a, 't> = Result<TransAttempt<'a, 't>, TransError>;

impl From<BchError> for TransError {
    fn from(error: BchError) -> Self {
        if error.matches(bch_errcode::BCH_ERR_transaction_restart) {
            TransError::Restart(error)
        } else {
            TransError::Error(error)
        }
    }
}

impl From<bch_errcode> for TransError {
    fn from(code: bch_errcode) -> Self {
        BchError::from(code).into()
    }
}

impl From<crate::util::alloc::AllocError> for TransError {
    fn from(error: crate::util::alloc::AllocError) -> Self {
        TransError::Error(error.into())
    }
}

impl From<TransError> for BchError {
    fn from(error: TransError) -> Self {
        match error {
            TransError::Restart(e) | TransError::Error(e) => e,
        }
    }
}

/// What a for_each body returns on success: whether the loop goes on. `()`
/// for a body that always does - most of them, as a C loop body returns 0 -
/// and `ControlFlow` for one that can stop early.
pub trait LoopControl {
    fn stops(&self) -> bool;
}

impl LoopControl for () {
    fn stops(&self) -> bool { false }
}

impl LoopControl for ControlFlow<()> {
    fn stops(&self) -> bool { self.is_break() }
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

    /// Whether anything is queued for the next commit: key updates, journal
    /// entries or accounting.
    pub fn has_updates(&self) -> bool {
        unsafe { c::bch2_trans_has_updates(self.raw()) }
    }

    pub fn commit(
        self,
        disk_res: Option<&DiskReservation<'_>>,
        flags:    impl Into<CommitOpts>,
    ) -> Result<Self, TransError> {
        unsafe {
            (*self.raw()).disk_res = disk_res.map_or(core::ptr::null_mut(), |r| r.as_mut_ptr());
        }
        // `lazy = false`: this is the regular commit, mirroring the C
        // bch2_trans_commit() inline (the lazy variant is a separate path).
        let ret = unsafe { c::__bch2_trans_commit(self.raw(), flags.into().to_c(), false) };
        self.result(ret)
    }

    /// Commit what's queued, if anything - and having committed, return
    /// transaction_restart_commit, so the caller's loop re-runs against the
    /// committed state: as bch2_trans_commit_lazy().
    pub fn commit_lazy(self, flags: impl Into<CommitOpts>) -> Result<Self, TransError> {
        let ret = unsafe {
            c::bch2_trans_commit_lazy(self.raw(), core::ptr::null_mut(),
                                      core::ptr::null_mut(), flags.into().to_c().0)
        };
        self.result(ret)
    }

    /// Commit only once the transaction's memory is getting full, as
    /// bch2_trans_commit_lazy_if_full(): for a key whose repairs are bounded
    /// only by something like snapshot count. A commit here restarts, and the
    /// re-drive has less to queue, so it converges.
    pub fn commit_lazy_if_full(self, flags: impl Into<CommitOpts>) -> Result<Self, TransError> {
        let ret = unsafe {
            c::bch2_trans_commit_lazy_if_full(self.raw(), core::ptr::null_mut(),
                                              core::ptr::null_mut(), flags.into().to_c().0)
        };
        self.result(ret)
    }

    pub fn result(self, ret: i32) -> Result<Self, TransError> {
        ret_to_result(ret)?;
        Ok(self)
    }

    /// Restart the transaction, as C's btree_trans_restart(): the transaction
    /// has to know - it's marked restarted and its restart_count bumped, which
    /// is what bch2_trans_begin() and verify_not_restarted() go by.
    pub fn restart(self, error: bch_errcode) -> TransError {
        let ip = Self::restart as *const () as core::ffi::c_ulong;
        let ret = unsafe { c::bch2_trans_restart_ip(self.raw(), error as i32, ip) };
        TransError::Restart(BchError::from_raw(-ret))
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
    pub fn nested<T, F>(self, mut f: F) -> TransResult<'a, 't, T>
    where
        F: FnMut(TransAttempt<'a, 't>) -> TransResult<'a, 't, T>,
    {
        let trans = self.trans;
        let orig_restart_count = unsafe { (*trans.raw()).restart_count };
        let _may_drop = TransMayDropUpdates::new(trans);

        let mut t = self;
        loop {
            match f(t) {
                Ok((t, v)) => {
                    t.verify_not_restarted();

                    if t.restart_count != orig_restart_count {
                        return Err(TransError::Restart(BchError::from_raw(
                            bch_errcode::BCH_ERR_transaction_restart_nested as i32)));
                    }
                    return Ok((t, v));
                }
                Err(TransError::Restart(_)) => t = trans.begin(),
                Err(e) => return Err(e),
            }
        }
    }

    /// Run @f, which doesn't work within this attempt: it begins and commits
    /// attempts of its own - calls a helper that commits for itself, such as
    /// C's bch2_fsck_write_inode() - and so spends this one. Whatever this
    /// attempt had queued is dropped, and if @f began an attempt,
    /// transaction_restart_nested is returned so the caller's loop retries
    /// against what @f committed. As the C's hand-rolled check_i_sectors()
    /// and check_subdir_dirents_count().
    ///
    /// Not nested(): that's for a body working within the attempt, which it
    /// checks on success, and that check fires for this one.
    ///
    /// XXX: an odd contract, named rather than fixed - repairs that commit
    /// should hand their commits back to the caller's loop instead.
    pub fn self_committing<T, F>(self, f: F) -> TransResult<'a, 't, T>
    where
        F: FnOnce(&'a BtreeTrans<'t>) -> Result<T, BchError>,
    {
        let trans = self.trans;
        let _may_drop = TransMayDropUpdates::new(trans);

        let v = f(trans)?;

        if unsafe { (*trans.raw()).restart_count } != self.restart_count {
            return Err(TransError::Restart(BchError::from_raw(
                bch_errcode::BCH_ERR_transaction_restart_nested as i32)));
        }
        Ok((self, v))
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

    /// bkey_reassemble() into a buffer with `val_u64s` of value space:
    /// the copied value is zero-extended (or truncated) to the new size.
    pub fn bkey_reassemble_resized(&self, k: BkeySC<'_>, val_u64s: usize)
        -> Result<TransBkey<'a, 't>, BchError>
    {
        const BKEY_U64S: usize = size_of::<c::bkey>() / size_of::<u64>();

        let mut dst = self.bkey_alloc((BKEY_U64S + val_u64s) as u32)?;
        dst.as_mut_u64s().fill(0);

        let copy_val = (k.k.u64s as usize - BKEY_U64S).min(val_u64s);
        unsafe {
            core::ptr::copy_nonoverlapping(k.k, &mut dst.k_i_mut().k, 1);
            core::ptr::copy_nonoverlapping(
                k.v as *const c::bch_val as *const u64,
                &mut dst.as_mut_u64s()[BKEY_U64S] as *mut u64,
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
        const BKEY_U64S: usize = size_of::<c::bkey>() / size_of::<u64>();

        let mut k = self.bkey_alloc((BKEY_U64S + val_u64s) as u32)?;
        k.as_mut_u64s().fill(0);
        unsafe { c::bkey_init(k.k_mut()) };
        k.k_mut().u64s = (BKEY_U64S + val_u64s) as u8;
        k.k_mut().type_ = type_;
        k.k_mut().p = pos;
        Ok(k)
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

    /// The key at @pos in @btree, as a mutable copy already queued as its
    /// update - an error unless it's of @type_; at least @min_bytes: as
    /// __bch2_bkey_get_mut(), bch2_bkey_get_mut_typed().
    pub fn bkey_get_mut(
        &self,
        btree:     c::btree_id,
        pos:       bpos,
        flags:     UpdateTriggerFlags,
        type_:     c::bch_bkey_type,
        min_bytes: usize,
    ) -> Result<TransBkey<'a, 't>, BchError> {
        unsafe {
            let k = c::__bch2_bkey_get_mut(self.raw(), btree, pos,
                                           c::btree_iter_update_trigger_flags(flags.bits()),
                                           type_.0, min_bytes as u32);
            TransBkey::from_raw(self, k)
        }
    }

    /// @k, the key at @iter, as a mutable copy already queued as its update -
    /// an error unless it's of @type_; at least @min_bytes, zero padded: as
    /// __bch2_bkey_make_mut(), bch2_bkey_make_mut_typed(). Edits to it go in
    /// with the commit.
    pub fn bkey_make_mut(
        &self,
        iter:      &mut BtreeIter<'t>,
        k:         BkeySC<'_>,
        flags:     UpdateTriggerFlags,
        type_:     c::bch_bkey_type,
        min_bytes: usize,
    ) -> Result<TransBkey<'a, 't>, BchError> {
        let mut raw = k.to_raw();
        unsafe {
            let k = c::__bch2_bkey_make_mut(self.raw(), iter.raw_mut(), &mut raw,
                                            c::btree_iter_update_trigger_flags(flags.bits()),
                                            type_.0, min_bytes as u32);
            TransBkey::from_raw(self, k)
        }
    }

    /// Overwrite @old with @new where they overlap, @old being the extent at
    /// @iter: as bch2_trans_update_extent_overwrite().
    pub fn update_extent_overwrite(
        self,
        iter:  &mut BtreeIter<'t>,
        flags: UpdateTriggerFlags,
        old:   BkeySC<'_>,
        new:   BkeySC<'_>,
    ) -> Result<Self, TransError> {
        let ret = unsafe {
            c::bch2_trans_update_extent_overwrite(self.raw(), &mut iter.raw,
                                                  c::btree_iter_update_trigger_flags(flags.bits()),
                                                  old.to_raw(), new.to_raw())
        };
        self.result(ret)
    }

    /// Have the commit reserve @sectors more, at @nr_replicas, for data an
    /// update rewrites: as bch2_trans_extra_disk_res_add().
    pub fn extra_disk_res_add(&self, sectors: u64, nr_replicas: u32) {
        unsafe { c::bch2_trans_extra_disk_res_add(self.raw(), sectors, nr_replicas) }
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

    /// A key moved from @old_pos to @new_pos, in the same snapshot: in every
    /// descendant snapshot where it was overwritten at @old_pos, whiteout
    /// @new_pos too, so it stays hidden there: as
    /// bch2_insert_snapshot_whiteouts().
    pub fn insert_snapshot_whiteouts(self, btree: c::btree_id, old_pos: bpos, new_pos: bpos)
        -> Result<Self, TransError>
    {
        let ret = unsafe { c::bch2_insert_snapshot_whiteouts(self.raw(), btree, old_pos, new_pos) };
        self.result(ret)
    }

    /// Set or clear the bit at @pos in bitset btree @btree, through the
    /// write buffer: as bch2_btree_bit_mod_buffered().
    pub fn bit_mod_buffered(self, btree: c::btree_id, pos: bpos, set: bool) -> Result<Self, TransError> {
        let ret = unsafe { c::bch2_btree_bit_mod_buffered(self.raw(), btree, pos, set) };
        self.result(ret)
    }

    pub fn insert(
        self,
        btree: impl Into<u32>,
        key:   TransBkey<'_, 't>,
        flags: UpdateTriggerFlags,
    ) -> Result<Self, TransError> {
        self.insert_with(btree, key, BtreeIterFlags::empty(), flags)
    }

    /// As insert(), with flags for the iterator the insert goes through, too.
    pub fn insert_with(
        self,
        btree:      impl Into<u32>,
        key:        TransBkey<'_, 't>,
        iter_flags: BtreeIterFlags,
        flags:      UpdateTriggerFlags,
    ) -> Result<Self, TransError> {
        let ret = unsafe {
            c::bch2_btree_insert_trans(
                self.raw(),
                c::btree_id::from_raw(btree.into()).expect("invalid btree id"),
                key.as_ptr(),
                c::btree_iter_update_trigger_flags(iter_flags.bits() | flags.bits()),
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

    /// delete_at() on an iterator C owns, for Rust that C calls with one.
    pub(crate) fn delete_at_raw(
        self,
        iter:  &mut c::btree_iter,
        flags: UpdateTriggerFlags,
    ) -> Result<Self, TransError> {
        let ret = unsafe {
            c::bch2_btree_delete_at(self.raw(), iter, c::btree_iter_update_trigger_flags(flags.bits()))
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
        flags:         impl Into<CommitOpts>,
        iter_searched: bool,
    ) -> Result<Self, TransError> {
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
        })
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
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
        // For hash table inserts, which take them with the iterator flags:
        const STR_HASH_MUST_CREATE = c::btree_iter_update_trigger_flags::STR_HASH_must_create.0;
        const STR_HASH_MUST_REPLACE = c::btree_iter_update_trigger_flags::STR_HASH_must_replace.0;
    }
}

bitflags! {
    /// The `BTREE_UPDATE_*` / `BTREE_TRIGGER_*` half of the C
    /// `btree_iter_update_trigger_flags` enum: the flags that control how an
    /// update commits and which triggers it runs. These combine freely with each
    /// other, and with the iterator half only where one function takes both -
    /// the str_hash inserts, which take the two separately here, so they get
    /// their own type. (The str_hash flags themselves are with `BtreeIterFlags`.)
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
/// a set of [`CommitFlags`]. The watermark usually isn't set, so the commit
/// functions take `impl Into<CommitOpts>`: pass the [`CommitFlags`] alone, and
/// build a `CommitOpts` only to set a watermark.
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

/// Flags alone, with the default watermark: what nearly every commit
/// passes, so the commit functions take `impl Into<CommitOpts>` and a call
/// reads as C's does - `t.commit(None, CommitFlags::NO_ENOSPC)`.
impl From<CommitFlags> for CommitOpts {
    fn from(flags: CommitFlags) -> Self {
        CommitOpts::new().flags(flags)
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
    flags: impl Into<CommitOpts>,
    mut f: F,
) -> Result<(), BchError>
where
    F: for<'a> FnMut(TransAttempt<'a, 't>) -> TransRet<'a, 't>,
{
    let flags = flags.into();
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
    flags: impl Into<CommitOpts>,
    f: F,
) -> Result<(), BchError>
where
    F: for<'a> FnMut(TransAttempt<'a, 't>) -> TransRet<'a, 't>,
{
    let trans = crate::btree_trans!(fs);
    commit_do(&trans, disk_res, flags, f)
}

/// Create a transaction and run a closure with restart retry (no commit).
///
/// Equivalent to the C `bch2_trans_run` macro.
pub fn trans_run<'t, T, F>(fs: &'t Fs, f: F) -> Result<T, BchError>
where
    F: for<'a> FnMut(TransAttempt<'a, 't>) -> TransResult<'a, 't, T>,
{
    let trans = crate::btree_trans!(fs);
    lockrestart_do(&trans, f)
}

pub struct BtreeIter<'t> {
    raw:   c::btree_iter,
    trans: PhantomData<&'t BtreeTrans<'t>>,
}

pub(crate) fn bkey_s_c_to_result<'i>(k: c::bkey_s_c) -> Result<Option<BkeySC<'i>>, BchError> {
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

    pub fn btree(&self) -> c::btree_id {
        self.raw.btree_id()
    }

    pub fn set_pos(&mut self, pos: c::bpos) {
        unsafe { c::bch2_btree_iter_set_pos(&mut self.raw, pos) };
    }

    pub fn set_pos_to_extent_start(&mut self) {
        unsafe { c::bch2_btree_iter_set_pos_to_extent_start(&mut self.raw) };
    }

    pub fn set_snapshot(&mut self, snapshot: u32) {
        unsafe { c::bch2_btree_iter_set_snapshot(&mut self.raw, snapshot) };
    }

    /// A second iterator at the same position, sharing this one's paths: as
    /// C's CLASS(btree_iter_copy).
    pub fn copy(&self) -> BtreeIter<'t> {
        unsafe {
            let mut iter: MaybeUninit<c::btree_iter> = MaybeUninit::uninit();

            c::bch2_trans_copy_iter(iter.as_mut_ptr(), &self.raw as *const _ as *mut _);

            BtreeIter {
                raw:   iter.assume_init(),
                trans: PhantomData,
            }
        }
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

    /// The key at the iterator's position, or a deleted key for a hole.
    pub fn peek_slot(&mut self) -> Result<Option<BkeySC<'_>>, BchError> {
        unsafe { bkey_s_c_to_result(c::bch2_btree_iter_peek_slot(&mut self.raw)) }
    }

    /// The key at the iterator's position, if it's a @type: as C's
    /// bch2_bkey_get_typed() - ENOENT_bkey_type_mismatch if it isn't.
    pub fn peek_slot_typed(&mut self, type_: c::bch_bkey_type) -> Result<BkeySC<'_>, BchError> {
        let k = bkey_s_c_to_result(unsafe { c::__bch2_bkey_get_typed(&mut self.raw, type_) })?;
        Ok(k.expect("a slot always has a key"))
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
    ) -> TransRet<'a, 't> {
        let ret = unsafe { c::bch2_btree_iter_traverse(self.raw_mut()) };
        t.result(ret)
    }

    /// The loop behind the for_each family: get a key with @peek, hand it to
    /// @f, move on with @step.
    ///
    /// @f's result is two things, kept apart: whether its work succeeded
    /// (`Result`), and whether to go on (LoopControl: `()` for a body that
    /// always goes on, `ControlFlow` for one that can stop early). A
    /// transaction restart, from the peek or from @f, retries the same
    /// position; any other error is returned.
    ///
    /// @f gets the iterator too, as the C loop bodies do - to update at its
    /// position, or read where it is. As in C, the key points into the node
    /// the iterator's path holds: it's not to be used once @f has moved the
    /// iterator.
    fn for_each_inner<P, S, F, R>(
        &mut self,
        trans:    &BtreeTrans<'_>,
        mut peek: P,
        mut step: S,
        mut f:    F,
    ) -> Result<(), BchError>
    where
        P: FnMut(*mut c::btree_iter) -> c::bkey_s_c,
        S: FnMut(*mut c::btree_iter) -> bool,
        F: for<'a> FnMut(&mut BtreeIter<'t>, BkeySC<'a>) -> Result<R, BchError>,
        R: LoopControl,
    {
        loop {
            let t = trans.begin();

            let k = match bkey_s_c_to_result(peek(&mut self.raw)) {
                Err(e) if e.matches(bch_errcode::BCH_ERR_transaction_restart) => continue,
                Err(e) => return Err(e),
                Ok(None) => return Ok(()),
                Ok(Some(k)) => k,
            };

            match f(self, k) {
                Err(e) if e.matches(bch_errcode::BCH_ERR_transaction_restart) => continue,
                Err(e) => return Err(e),
                Ok(flow) => {
                    t.verify_not_restarted();
                    if flow.stops() {
                        return Ok(());
                    }
                }
            }

            if !step(&mut self.raw) {
                return Ok(());
            }
        }
    }

    /// The next key, up to @end, as the iterator's own flags say to peek: a
    /// slot at a time - holes included - for a SLOTS iterator. As C's
    /// bch2_btree_iter_peek_max_type() with the flags the iterator was made
    /// with, which is what C's for_each loops peek with; the for_each family
    /// peeks with this so no loop can ignore its iterator's flags.
    fn peek_own_type(raw: *mut c::btree_iter, end: bpos) -> c::bkey_s_c {
        unsafe {
            let flags = c::btree_iter_update_trigger_flags((*raw).flags as u32);
            c::bch2_btree_iter_peek_max_type(raw, end, flags)
        }
    }

    pub fn for_each_max<F, R>(&mut self, trans: &BtreeTrans<'_>, end: bpos, f: F)
        -> Result<(), BchError>
    where
        F: for<'a> FnMut(&mut BtreeIter<'t>, BkeySC<'a>) -> Result<R, BchError>,
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

    /// Walk the keys from the iterator's position, inside the caller's
    /// attempt: as C's for_each_btree_key_norestart(). Unlike the rest of the
    /// family this doesn't begin or retry anything - a restart is returned,
    /// for the loop the caller is in.
    pub fn for_each_norestart<F, R>(&mut self, f: F) -> Result<(), BchError>
    where
        F: for<'a> FnMut(&mut BtreeIter<'t>, BkeySC<'a>) -> Result<R, BchError>,
        R: LoopControl,
    {
        self.for_each_max_norestart(SPOS_MAX, f)
    }

    /// As for_each_norestart(), up to and including @end: as C's
    /// for_each_btree_key_max_norestart().
    pub fn for_each_max_norestart<F, R>(&mut self, end: bpos, mut f: F) -> Result<(), BchError>
    where
        F: for<'a> FnMut(&mut BtreeIter<'t>, BkeySC<'a>) -> Result<R, BchError>,
        R: LoopControl,
    {
        loop {
            let Some(k) = bkey_s_c_to_result(Self::peek_own_type(&mut self.raw, end))?
            else {
                return Ok(());
            };

            if f(self, k)?.stops() || !unsafe { c::bch2_btree_iter_advance(&mut self.raw) } {
                return Ok(());
            }
        }
    }

    /// As for_each_norestart(), backwards from the iterator's position down
    /// to @min: as C's for_each_btree_key_reverse_norestart(), with the
    /// bound.
    pub fn for_each_reverse_norestart<F, R>(&mut self, min: bpos, mut f: F) -> Result<(), BchError>
    where
        F: for<'a> FnMut(&mut BtreeIter<'t>, BkeySC<'a>) -> Result<R, BchError>,
        R: LoopControl,
    {
        loop {
            let Some(k) = bkey_s_c_to_result(unsafe { c::bch2_btree_iter_peek_prev_min(&mut self.raw, min) })?
            else {
                return Ok(());
            };

            if f(self, k)?.stops() || !unsafe { c::bch2_btree_iter_rewind(&mut self.raw) } {
                return Ok(());
            }
        }
    }

    pub fn for_each<F, R>(&mut self, trans: &BtreeTrans<'_>, f: F) -> Result<(), BchError>
    where
        F: for<'a> FnMut(&mut BtreeIter<'t>, BkeySC<'a>) -> Result<R, BchError>,
        R: LoopControl,
    {
        self.for_each_max(trans, SPOS_MAX, f)
    }

    /// As for_each_inner(), committing @f's updates after each key. A restart
    /// from @f or the commit retries the key.
    ///
    /// @f finishes a key in one of three ways:
    ///  - Ok(attempt): commit what it queued, and go on to the next key -
    ///    also how a body finishes a key early, keeping its repairs.
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
    fn for_each_commit_inner<P, S, F>(
        &mut self,
        trans:    &BtreeTrans<'t>,
        disk_res: Option<&DiskReservation<'_>>,
        flags:    CommitOpts,
        mut peek: P,
        mut step: S,
        mut f:    F,
    ) -> Result<(), BchError>
    where
        P: FnMut(*mut c::btree_iter) -> c::bkey_s_c,
        S: FnMut(*mut c::btree_iter) -> bool,
        F: for<'a, 'k> FnMut(
            TransAttempt<'a, 't>,
            &mut BtreeIter<'t>,
            BkeySC<'k>,
        ) -> TransRet<'a, 't>,
    {
        loop {
            let t = trans.begin();

            let k = match bkey_s_c_to_result(peek(&mut self.raw)) {
                Err(e) if e.matches(bch_errcode::BCH_ERR_transaction_restart) => continue,
                Err(e) => return Err(e),
                Ok(None) => return Ok(()),
                Ok(Some(k)) => k,
            };

            let restart_count = t.restart_count;

            let t = match f(t, self, k) {
                Ok(t) => t,
                Err(TransError::Restart(_)) => continue,
                Err(TransError::Error(e)) if e.matches(bch_errcode::BCH_ERR_fc_continue) => {
                    trans.verify_not_restarted(restart_count);
                    if !step(&mut self.raw) {
                        return Ok(());
                    }
                    continue;
                }
                Err(TransError::Error(e)) if e.matches(bch_errcode::BCH_ERR_fc_break) => {
                    trans.verify_not_restarted(restart_count);
                    return Ok(());
                }
                Err(TransError::Error(e)) => return Err(e),
            };

            let Some(t) = retry_restart(t.commit(disk_res, flags))? else {
                continue;
            };

            t.verify_not_restarted();

            if !step(&mut self.raw) {
                return Ok(());
            }
        }
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
            TransAttempt<'a, 't>,
            &mut BtreeIter<'t>,
            BkeySC<'k>,
        ) -> TransRet<'a, 't>,
    {
        self.for_each_commit_inner(trans, disk_res, flags.into(),
            |raw| unsafe { c::bch2_btree_iter_peek(raw) },
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
            TransAttempt<'a, 't>,
            &mut BtreeIter<'t>,
            BkeySC<'k>,
        ) -> TransRet<'a, 't>,
    {
        self.for_each_commit_inner(trans, disk_res, flags.into(),
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
            TransAttempt<'a, 't>,
            &mut BtreeIter<'t>,
            BkeySC<'k>,
        ) -> TransRet<'a, 't>,
    {
        self.for_each_commit_inner(trans, disk_res, flags.into(),
            |raw| unsafe {
                c::bch2_btree_iter_peek_max_type(
                    raw,
                    end,
                    c::btree_iter_update_trigger_flags(iter_flags.bits()),
                )
            },
            |raw| unsafe { c::bch2_btree_iter_advance(raw) },
            f)
    }

    pub fn for_each_reverse<F, R>(&mut self, trans: &BtreeTrans<'_>, min: bpos, f: F)
        -> Result<(), BchError>
    where
        F: for<'a> FnMut(&mut BtreeIter<'t>, BkeySC<'a>) -> Result<R, BchError>,
        R: LoopControl,
    {
        self.for_each_inner(trans,
            |raw| unsafe { c::bch2_btree_iter_peek_prev_min(raw, min) },
            |raw| unsafe { c::bch2_btree_iter_rewind(raw) },
            f)
    }

    pub fn for_each_reverse_flags<F, R>(
        &mut self,
        trans: &BtreeTrans<'_>,
        flags: BtreeIterFlags,
        f:     F,
    ) -> Result<(), BchError>
    where
        F: for<'a> FnMut(&mut BtreeIter<'t>, BkeySC<'a>) -> Result<R, BchError>,
        R: LoopControl,
    {
        self.for_each_inner(trans,
            |raw| unsafe {
                c::bch2_btree_iter_peek_prev_type(
                    raw,
                    c::btree_iter_update_trigger_flags(flags.bits()),
                )
            },
            |raw| unsafe { c::bch2_btree_iter_rewind(raw) },
            f)
    }

    pub fn advance(&mut self) {
        unsafe {
            c::bch2_btree_iter_advance(&mut self.raw);
        }
    }

    /// Step back past the key just returned, for walking backwards: false at
    /// the start of the btree.
    pub fn rewind(&mut self) -> bool {
        unsafe { c::bch2_btree_iter_rewind(&mut self.raw) }
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

pub struct BtreeNodeToText<'b, 'f> {
    b:  &'b c::btree,
    fs: &'f Fs,
}

impl<'b, 'f> fmt::Display for BtreeNodeToText<'b, 'f> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        printbuf_to_formatter(f, |buf| unsafe {
            c::bch2_btree_node_to_text(buf, self.fs.raw, self.b)
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
            c::bch2_btree_node_ondisk_to_text(buf, self.fs.raw, self.b)
        })
    }
}
