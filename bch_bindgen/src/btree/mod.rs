use crate::bkey::*;
use crate::c;
use crate::errcode::{BchError, bch_errcode, errptr_to_result_c};
use crate::fs::Fs;
use crate::printbuf_to_formatter;
use crate::SPOS_MAX;
use bitflags::bitflags;
use std::fmt;
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::ops::ControlFlow;

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

    pub fn begin(&self) -> u32 {
        unsafe { c::bch2_trans_begin(self.raw) }
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
        let ret = unsafe { c::__bch2_trans_commit(self.raw, flags) };
        crate::errcode::ret_to_result(ret).map(|_| ())
    }
}

impl<'f> Drop for BtreeTrans<'f> {
    fn drop(&mut self) {
        unsafe {
            // Clear any pending restart state — bch2_trans_put() BUG_ONs
            // if the transaction is in restart, which can happen if Rust
            // code propagates a restart error via ? and unwinds.
            c::bch2_trans_begin(self.raw);
            c::bch2_trans_put(&mut *self.raw)
        }
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

pub fn lockrestart_do<T, F>(trans: &BtreeTrans, mut f: F) -> Result<T, BchError>
where
    F: FnMut() -> Result<T, BchError>
{
    loop {
        let restart_count = trans.begin();

        match f() {
            Err(e) if e.matches(bch_errcode::BCH_ERR_transaction_restart) => continue,
            Err(e) => return Err(e),
            Ok(v) => {
                trans.verify_not_restarted(restart_count);
                return Ok(v);
            }
        }
    }
}

/// Run a closure inside a transaction commit loop.
///
/// Equivalent to the C `commit_do` macro: runs the closure, and if it
/// succeeds, commits the transaction. Retries on transaction restart.
pub fn commit_do<F>(
    trans: &BtreeTrans,
    disk_res: *mut c::disk_reservation,
    journal_seq: *mut u64,
    flags: c::bch_trans_commit_flags,
    mut f: F,
) -> Result<(), BchError>
where
    F: FnMut(&BtreeTrans) -> Result<(), BchError>,
{
    lockrestart_do(trans, || {
        f(trans)?;
        trans.commit(disk_res, journal_seq, flags)
    })
}

/// Create a transaction and run a closure with commit retry.
///
/// Equivalent to the C `bch2_trans_commit_do` macro.
pub fn trans_commit_do<F>(
    fs: &Fs,
    disk_res: *mut c::disk_reservation,
    journal_seq: *mut u64,
    flags: c::bch_trans_commit_flags,
    f: F,
) -> Result<(), BchError>
where
    F: FnMut(&BtreeTrans) -> Result<(), BchError>,
{
    let trans = BtreeTrans::new(fs);
    commit_do(&trans, disk_res, journal_seq, flags, f)
}

/// Create a transaction and run a closure with restart retry (no commit).
///
/// Equivalent to the C `bch2_trans_run` macro.
pub fn trans_run<T, F>(fs: &Fs, f: F) -> Result<T, BchError>
where
    F: FnMut() -> Result<T, BchError>,
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
                std::mem::transmute::<u32, c::btree_id>(btree.into()),
                pos,
                c::btree_iter_update_trigger_flags(flags.bits),
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
                std::mem::transmute::<u32, c::btree_id>(btree.into()),
                pos,
                0,
                level,
                c::btree_iter_update_trigger_flags(flags.bits)
            );

            BtreeIter {
                raw:   iter.assume_init(),
                trans: PhantomData,
            }
        }
    }

    pub fn peek_max<'i>(&'i mut self, end: bpos) -> Result<Option<BkeySC<'i>>, BchError> {
        unsafe {
            bkey_s_c_to_result(c::bch2_btree_iter_peek_max(&mut self.raw, end))
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
                bkey_s_c_to_result(c::bch2_btree_iter_peek_max(&mut self.raw, end))
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
            let restart_count = trans.begin();
            let k = unsafe { c::bch2_btree_iter_peek_max(raw, end) };

            match bkey_s_c_to_result(k) {
                Err(e) if e.matches(bch_errcode::BCH_ERR_transaction_restart) => continue,
                Err(e) => return Err(e),
                Ok(None) => return Ok(()),
                Ok(Some(k)) => {
                    trans.verify_not_restarted(restart_count);
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
                std::mem::transmute::<u32, c::btree_id>(btree.into()),
                pos,
                locks_want,
                depth,
                c::btree_iter_update_trigger_flags(flags.bits),
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
            let restart_count = trans.begin();
            let b = unsafe { c::bch2_btree_iter_peek_node(raw) };

            match errptr_to_result_c(b) {
                Err(e) if e.matches(bch_errcode::BCH_ERR_transaction_restart) => continue,
                Err(e) => return Err(e),
                Ok(b) if b.is_null() => return Ok(()),
                Ok(b) => {
                    trans.verify_not_restarted(restart_count);
                    if let ControlFlow::Break(()) = f(unsafe { &*b }) {
                        return Ok(());
                    }
                }
            }
            unsafe { c::bch2_btree_iter_next_node(raw) };
        }
    }

    pub fn advance(&mut self) {
        unsafe {
            c::bch2_btree_iter_next_node(&mut self.raw);
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<&c::btree>, BchError> {
        unsafe {
            let b = c::bch2_btree_iter_next_node(&mut self.raw);
            errptr_to_result_c(b).map(|b| if !b.is_null() { Some(&*b) } else { None })
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
        let mut unpacked: c::bkey = unsafe { std::mem::zeroed() };

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
