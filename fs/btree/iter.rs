use super::bkey::*;
use crate::c;
use crate::errcode::{
    BchError,
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
use core::ops::{ControlFlow, Deref};

use c::bpos;

pub struct BtreeTrans<'f> {
    raw: *mut c::btree_trans,
    fs:  PhantomData<&'f Fs>,
}

impl<'f> BtreeTrans<'f> {
    pub fn new(fs: &'f Fs) -> BtreeTrans<'f> {
        unsafe {
            BtreeTrans {
                raw: &mut *c::__bch2_trans_get(fs.raw, 0),
                fs:  PhantomData,
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

    /// Commit the transaction.
    ///
    /// Equivalent to the static inline bch2_trans_commit() which sets
    /// disk_res/journal_seq then calls __bch2_trans_commit().
    pub fn commit(
        &self,
        disk_res: *mut c::disk_reservation,
        journal_seq: *mut u64,
        flags: c::bch_trans_commit_flags,
    ) -> Result<(), BchError> {
        unsafe {
            (*self.raw).disk_res = disk_res;
            (*self.raw).journal_seq = journal_seq;
        }
        let ret = unsafe {
            c::__bch2_trans_commit(self.raw, flags, false)
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

pub enum TransError<'a, 't> {
    Restart(BchError),
    Error {
        attempt: TransAttempt<'a, 't>,
        error:   BchError,
    },
}

pub type TransResult<'a, 't, T = ()> = Result<(TransAttempt<'a, 't>, T), TransError<'a, 't>>;

pub struct TransBkey<'a, 't> {
    ptr:      NonNull<c::bkey_i>,
    buf_u64s: u32,
    t:        PhantomData<&'a mut TransAttempt<'a, 't>>,
}

impl<'a, 't> TransAttempt<'a, 't> {
    pub fn trans(&self) -> &'a BtreeTrans<'t> {
        self.trans
    }

    /// Get the raw transaction pointer for passing to C functions.
    pub fn raw(&self) -> *mut c::btree_trans {
        self.trans.raw()
    }

    pub fn verify_not_restarted(&self) {
        self.trans.verify_not_restarted(self.restart_count);
    }

    pub fn commit(
        self,
        disk_res:    *mut c::disk_reservation,
        journal_seq: *mut u64,
        flags:       c::bch_trans_commit_flags,
    ) -> Result<Self, TransError<'a, 't>> {
        unsafe {
            (*self.raw()).disk_res = disk_res;
            (*self.raw()).journal_seq = journal_seq;
        }
        // `lazy = false`: this is the regular commit, mirroring the C
        // bch2_trans_commit() inline (the lazy variant is a separate path).
        let ret = unsafe { c::__bch2_trans_commit(self.raw(), flags, false) };
        self.result(ret)
    }

    pub fn result(self, ret: i32) -> Result<Self, TransError<'a, 't>> {
        match ret_to_result(ret) {
            Ok(()) => Ok(self),
            Err(e) if e.matches(bch_errcode::BCH_ERR_transaction_restart) => {
                Err(TransError::Restart(e))
            }
            Err(e) => Err(TransError::Error {
                attempt: self,
                error:   e,
            }),
        }
    }

    pub fn error(self, error: BchError) -> TransError<'a, 't> {
        if error.matches(bch_errcode::BCH_ERR_transaction_restart) {
            TransError::Restart(error)
        } else {
            TransError::Error {
                attempt: self,
                error,
            }
        }
    }

    pub fn result_value<T>(self, result: Result<T, BchError>) -> TransResult<'a, 't, T> {
        match result {
            Ok(v) => Ok((self, v)),
            Err(e) => Err(self.error(e)),
        }
    }

    pub fn try_do<F>(self, f: F) -> Result<Self, TransError<'a, 't>>
    where
        F: FnOnce(&BtreeTrans<'t>) -> Result<(), BchError>,
    {
        match f(self.trans) {
            Ok(()) => Ok(self),
            Err(e) => Err(self.error(e)),
        }
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

    pub fn bkey_clone(&self, k: &c::bkey_i) -> Result<TransBkey<'a, 't>, BchError> {
        let mut dst = self.bkey_alloc(k.k.u64s as u32)?;
        unsafe {
            core::ptr::copy_nonoverlapping(
                k as *const c::bkey_i as *const u8,
                dst.ptr.as_mut() as *mut c::bkey_i as *mut u8,
                k.k.u64s as usize * size_of::<u64>(),
            );
        }
        Ok(dst)
    }

    pub fn update(
        self,
        iter:  &mut BtreeIter<'t>,
        key:   TransBkey<'_, 't>,
        flags: c::btree_iter_update_trigger_flags,
    ) -> Result<Self, TransError<'a, 't>> {
        let ret = unsafe {
            c::bch2_trans_update_buf(
                self.raw(),
                &mut iter.raw,
                key.ptr.as_ptr(),
                key.buf_u64s,
                flags,
            )
        };
        self.result(ret)
    }
}

impl<'a, 't> TransBkey<'a, 't> {
    pub fn as_mut(&mut self) -> &mut c::bkey_i {
        unsafe { self.ptr.as_mut() }
    }

    pub fn as_ptr(&self) -> *mut c::bkey_i {
        self.ptr.as_ptr()
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
        const NOPRESERVE = c::btree_iter_update_trigger_flags::BTREE_ITER_nopreserve.0;
        const CACHED_NOFILL = c::btree_iter_update_trigger_flags::BTREE_ITER_cached_nofill.0;
        const KEY_CACHE_FILL = c::btree_iter_update_trigger_flags::BTREE_ITER_key_cache_fill.0;
    }
}

pub fn lockrestart_do<'t, T, F>(trans: &BtreeTrans<'t>, mut f: F) -> Result<T, BchError>
where
    F: for<'a> FnMut(TransAttempt<'a, 't>) -> TransResult<'a, 't, T>
{
    loop {
        let t = trans.begin();

        match f(t) {
            Err(TransError::Restart(_)) => continue,
            Err(TransError::Error { error, .. }) => return Err(error),
            Ok((t, v)) => {
                t.verify_not_restarted();
                return Ok(v);
            }
        }
    }
}

/// Run a closure inside a transaction commit loop.
///
/// Equivalent to the C `commit_do` macro: runs the closure, and if it
/// succeeds, commits the transaction. Retries on transaction restart.
pub fn commit_do<'t, F>(
    trans: &BtreeTrans<'t>,
    disk_res: *mut c::disk_reservation,
    journal_seq: *mut u64,
    flags: c::bch_trans_commit_flags,
    mut f: F,
) -> Result<(), BchError>
where
    F: for<'a> FnMut(TransAttempt<'a, 't>) -> Result<TransAttempt<'a, 't>, TransError<'a, 't>>,
{
    lockrestart_do(trans, |t| {
        let t = f(t)?;
        let t = t.commit(disk_res, journal_seq, flags)?;
        Ok((t, ()))
    })
}

/// Create a transaction and run a closure with commit retry.
///
/// Equivalent to the C `bch2_trans_commit_do` macro.
pub fn trans_commit_do<'t, F>(
    fs: &'t Fs,
    disk_res: *mut c::disk_reservation,
    journal_seq: *mut u64,
    flags: c::bch_trans_commit_flags,
    f: F,
) -> Result<(), BchError>
where
    F: for<'a> FnMut(TransAttempt<'a, 't>) -> Result<TransAttempt<'a, 't>, TransError<'a, 't>>,
{
    let trans = BtreeTrans::new(fs);
    commit_do(&trans, disk_res, journal_seq, flags, f)
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
                    v:    &*k.v,
                    iter: PhantomData,
                })
            }
        } else {
            None
        }
    })
}

impl<'t> BtreeIter<'t> {
    pub fn new(
        trans: &'t BtreeTrans<'t>,
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
        trans: &'t BtreeTrans<'t>,
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

    pub fn peek(&mut self) -> Result<Option<BkeySC<'_>>, BchError> {
        self.peek_max(SPOS_MAX)
    }

    pub fn peek_prev_min<'i>(&'i mut self, min: bpos) -> Result<Option<BkeySC<'i>>, BchError> {
        unsafe {
            bkey_s_c_to_result(c::bch2_btree_iter_peek_prev_min(&mut self.raw, min))
        }
    }

    pub fn peek_prev(&mut self) -> Result<Option<BkeySC<'_>>, BchError> {
        self.peek_prev_min(c::bpos { inode: 0, offset: 0, snapshot: 0 })
    }

    pub fn for_each_max<F>(&mut self, trans: &BtreeTrans, end: bpos, mut f: F)
        -> Result<(), BchError>
    where
        F: for<'a> FnMut(BkeySC<'a>) -> ControlFlow<()>,
    {
        let raw = &mut self.raw as *mut c::btree_iter;
        loop {
            let t = trans.begin();
            let k = unsafe { c::bch2_btree_iter_peek_max(raw, &end) };

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
            unsafe { c::bch2_btree_iter_advance(raw) };
        }
    }

    pub fn for_each<F>(&mut self, trans: &BtreeTrans, f: F) -> Result<(), BchError>
    where
        F: for<'a> FnMut(BkeySC<'a>) -> ControlFlow<()>,
    {
        self.for_each_max(trans, SPOS_MAX, f)
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
        trans: &'t BtreeTrans<'t>,
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

    pub fn for_each<F>(&mut self, trans: &BtreeTrans, mut f: F) -> Result<(), BchError>
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
