#![allow(non_camel_case_types)]

use super::bkey::*;
pub use super::types::{BtreeIter, BtreeTrans};
pub use super::for_each::*;
pub use super::trans_attempt::*;
use crate::btree::node::BtreeNode;
use crate::btree::path::{btree_path_level, BtreePath, LockedNode, PathIdx};
use crate::c;
use crate::errcode::{BchError, bch_errcode, errptr_or_null};
use crate::fs::Fs;
use crate::util::Printbuf;
use crate::SPOS_MAX;
use bitflags::bitflags;
use core::ffi::{c_ulong, c_int, c_uint};
use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::ptr::NonNull;
use crate::data::ptrs::BtreePtrs;
use crate::types::c_default;
use cstruct_macros::{CStruct, c_extern};
use crate::format::{Bpos, Btree, BkeyType};

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct trans_for_each_path_inorder_iter {
    pub sorted_idx: PathIdx,
    pub path_idx: PathIdx,
}

/*
 * Exempt bch2_trans_begin() from the dropped-updates warning for a scope (see
 * begin_may_drop_updates), restoring the previous value on exit, so exemptions
 * nest: an inner one ending doesn't end the outer.
 */
#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct trans_may_drop_updates {
    pub trans: *mut BtreeTrans<'static>,
    pub old: bool,
}
c_default!(trans_may_drop_updates);

c_extern! {
    pub fn bch2_btree_increase_depth(trans: *mut BtreeTrans<'_>, path: PathIdx, flags: c_uint) -> c_int;
    pub fn bch2_btree_iter_peek_root(trans: *mut BtreeTrans<'_>, iter: *mut c::btree_iter, btree: Btree,
                                     level: c_uint) -> c::bkey_s_c;
    #[must_use]
    pub fn bch2_btree_iter_traverse(iter: *mut c::btree_iter) -> c_int;
    #[must_use]
    pub fn __bch2_btree_iter_traverse(iter: *mut c::btree_iter) -> c_int;
    pub fn bch2_btree_iter_peek_node(iter: *mut c::btree_iter) -> *mut BtreeNode;
    pub fn bch2_btree_iter_peek_max(iter: *mut c::btree_iter, end: *const Bpos) -> c::bkey_s_c;
    pub fn bch2_btree_iter_peek_prev_min(iter: *mut c::btree_iter, end: Bpos) -> c::bkey_s_c;
    pub fn bch2_btree_iter_peek_slot(iter: *mut c::btree_iter) -> c::bkey_s_c;
    pub fn bch2_btree_iter_peek_slot_max(iter: *mut c::btree_iter, end: Bpos) -> c::bkey_s_c;
    pub fn bch2_set_btree_iter_dontneed(iter: *mut c::btree_iter);
    pub fn bch2_btree_iter_prev_slot(iter: *mut c::btree_iter) -> c::bkey_s_c;
    pub fn bch2_btree_iter_advance(iter: *mut c::btree_iter) -> bool;
    pub fn bch2_btree_iter_params_valid(btree: Btree, level: c_uint, start: Bpos, end: Bpos,
                                        flags: c::btree_iter_update_trigger_flags) -> bool;
    pub fn bch2_btree_iter_rewind(iter: *mut c::btree_iter) -> bool;
    pub fn bch2_trans_iter_exit(iter: *mut c::btree_iter);
    pub fn bch2_trans_iter_init_outlined(trans: *mut BtreeTrans<'_>, iter: *mut c::btree_iter,
                                         btree_id: Btree, pos: Bpos,
                                         flags: c::btree_iter_update_trigger_flags, ip: c_ulong);
    pub fn bch2_trans_copy_iter(dst: *mut c::btree_iter, src: *mut c::btree_iter);
    pub fn __bch2_trans_node_iter_init(trans: *mut BtreeTrans<'_>, iter: *mut c::btree_iter,
                                       btree_id: Btree, pos: Bpos, locks_want: c_uint,
                                       depth: c_uint, flags: c::btree_iter_update_trigger_flags);
}

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct BtreeIterFlags: u32 {
        const SLOTS = c::btree_iter_update_trigger_flags::slots.bits();
        const PREV = c::btree_iter_update_trigger_flags::prev.bits();
        const INTENT = c::btree_iter_update_trigger_flags::intent.bits();
        const PREFETCH = c::btree_iter_update_trigger_flags::prefetch.bits();
        const IS_EXTENTS = c::btree_iter_update_trigger_flags::is_extents.bits();
        const NOT_EXTENTS = c::btree_iter_update_trigger_flags::not_extents.bits();
        const CACHED = c::btree_iter_update_trigger_flags::cached.bits();
        const KEY_CACHED = c::btree_iter_update_trigger_flags::with_key_cache.bits();
        const WITH_JOURNAL = c::btree_iter_update_trigger_flags::with_journal.bits();
        const SNAPSHOT_FIELD = c::btree_iter_update_trigger_flags::snapshot_field.bits();
        const ALL_SNAPSHOTS = c::btree_iter_update_trigger_flags::all_snapshots.bits();
        const FILTER_SNAPSHOTS = c::btree_iter_update_trigger_flags::filter_snapshots.bits();
        const NOFILTER_WHITEOUTS = c::btree_iter_update_trigger_flags::nofilter_whiteouts.bits();
        const NOPRESERVE = c::btree_iter_update_trigger_flags::nopreserve.bits();
        const NOFILL = c::btree_iter_update_trigger_flags::nofill.bits();
        const CACHED_NOFILL = c::btree_iter_update_trigger_flags::cached_nofill.bits();
        const KEY_CACHE_FILL = c::btree_iter_update_trigger_flags::key_cache_fill.bits();
        const COMMITTED = c::btree_iter_update_trigger_flags::committed.bits();
        /// Extents: a peek returns its key cut to the window - [position, end),
        /// or backwards [min, position) - the iterator's key with it, so a walk
        /// steps to the window's edge at most, where it ends. An update of
        /// what's returned replaces only the window.
        const TRIM = c::btree_iter_update_trigger_flags::trim.bits();
    }
}

bitflags! {
    /// The BTREE_UPDATE_* and BTREE_TRIGGER_* half of C's
    /// btree_iter_update_trigger_flags: how an update commits, and which
    /// triggers it runs.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct UpdateTriggerFlags: u32 {
        const INTERNAL_SNAPSHOT_NODE   = c::btree_iter_update_trigger_flags::internal_snapshot_node.bits();
        const NOJOURNAL                = c::btree_iter_update_trigger_flags::nojournal.bits();
        const KEY_CACHE_RECLAIM        = c::btree_iter_update_trigger_flags::key_cache_reclaim.bits();
        const OVERWRITE_TRIGGERED      = c::btree_iter_update_trigger_flags::overwrite_triggered.bits();
        const NORUN                   = c::btree_iter_update_trigger_flags::norun.bits();
        const TRANSACTIONAL            = c::btree_iter_update_trigger_flags::transactional.bits();
        const ATOMIC                   = c::btree_iter_update_trigger_flags::atomic.bits();
        const GC                       = c::btree_iter_update_trigger_flags::gc.bits();
        const INSERT                   = c::btree_iter_update_trigger_flags::BTREE_TRIGGER_insert.bits();
        const OVERWRITE                = c::btree_iter_update_trigger_flags::overwrite.bits();
        const IS_DISCARD               = c::btree_iter_update_trigger_flags::is_discard.bits();
        const SET_NEEDS_RECONCILE_DONE = c::btree_iter_update_trigger_flags::set_needs_reconcile_done.bits();
    }
}

bitflags! {
    /// The bits of bch_trans_commit_flags above the watermark.
    #[derive(Clone, Copy)]
    pub struct CommitFlags: u32 {
        const NO_ENOSPC             = c::bch_trans_commit_flags::no_enospc.bits();
        const NO_CHECK_RW           = c::bch_trans_commit_flags::no_check_rw.bits();
        const NO_JOURNAL_RES        = c::bch_trans_commit_flags::no_journal_res.bits();
        const NO_SKIP_NOOPS         = c::bch_trans_commit_flags::no_skip_noops.bits();
        const JOURNAL_RECLAIM       = c::bch_trans_commit_flags::journal_reclaim.bits();
        const JOURNAL_REPLAY        = c::bch_trans_commit_flags::journal_replay.bits();
        const SKIP_ACCOUNTING_APPLY = c::bch_trans_commit_flags::skip_accounting_apply.bits();
    }
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
    pub fn set_pos(&mut self, mut pos: Bpos) {
        if self.update_path != 0 {
            let intent = self.iter_flags().contains(BtreeIterFlags::INTENT);
            let trans = unsafe { &*self.trans };
            trans.path_put(&mut trans.paths(), self.update_path, intent);
        }
        self.update_path = 0;

        if !self.iter_flags().contains(BtreeIterFlags::ALL_SNAPSHOTS) {
            pos.snapshot = self.snapshot;
        }

        self.k.set_key_type(BkeyType::deleted);
        self.k.p     = pos;
        self.k.size  = 0;
        self.pos     = pos;
    }

    /// The next key up to @end, walking as @flags say: backwards with prev, a
    /// slot at a time - holes included - with slots. The static inline
    /// bch2_btree_iter_peek_max_type().
    pub(super) fn peek_max_type_raw(&mut self, end: Bpos, flags: BtreeIterFlags) -> c::bkey_s_c {
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
    to_walk:   impl Fn(usize, &BtreeIter<'t>) -> Bpos,
) -> Result<(Bpos, [(&'k BtreeIter<'t>, Option<BkeySC<'k>>); N]), BchError>
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

    let pos: [Bpos; N] = core::array::from_fn(|i| to_walk(i, iters[i]));
    let at = pos.iter().copied().min().expect("an iterator to walk");

    Ok((at, core::array::from_fn(|i| (iters[i], if at < pos[i] { None } else { keys[i] }))))
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

    /// The key pointing to @btree's root - none unless the root's just below
    /// @level - with this iterator initialized at it, and given back shared
    /// with the key: bch2_btree_iter_peek_root(), for BtreeLevels.
    pub(super) fn peek_root<'i>(&'i mut self, t: &TransAttempt<'_, 't>, btree: Btree, level: u32)
        -> Result<Option<(&'i BtreeIter<'t>, BkeySC<'i>)>, BchError>
    {
        // SAFETY: the key, in the iterator's path, for as long as it's borrowed
        let k = unsafe { bkey_s_c_to_result::<'i>(c::bch2_btree_iter_peek_root(t.raw(), self.raw_mut(), btree, level)) }?;
        Ok(k.map(|k| (&*self, k)))
    }

    /// Whether the iterator's path holds a node at its level - a btree node,
    /// or a cached path's key cache entry: btree_path_node().
    pub(crate) fn has_node_at_iter_level(&self, t: &TransAttempt<'_, 't>) -> bool {
        let paths = t.trans().paths();
        let path = &paths[self.path];
        matches!(path.l[path.level() as usize].b.get(), Ok(Some(_)))
    }

    /// The node the iterator's path holds at its level, if it has one - a
    /// level without one holds NULL or an error pointer, as IS_ERR_OR_NULL().
    pub(crate) fn node_at_iter_level<'a>(
        &self,
        t: &TransAttempt<'a, 't>,
    ) -> Option<BtreeNodeRef> {
        let paths = t.trans().paths();
        let path = paths[self.path].downcast::<BtreeNode>()
            .expect("node_at_iter_level() of a cached iterator, which holds no btree node");
        path.l[path.level() as usize].b.get().ok()?.map(|raw| BtreeNodeRef { raw })
    }

    /// The btree's key at the iterator's path's position - not the journal
    /// keys over it, which a peek sees - unpacked into @u if it's packed.
    pub(crate) fn path_peek_slot<'k>(&'k self, t: &TransAttempt<'_, 't>, u: &'k mut c::bkey) -> BkeySC<'k> {
        let paths = t.trans().paths();
        unsafe {
            bkey_s_c_to_result::<'k>(paths[self.path].peek_slot(u))
                .ok()
                .flatten()
                .expect("a slot always has a key")
        }
    }

    /// btree_iter_path()->l[0]: None with no node there.
    ///
    /// # Safety
    /// The transaction doesn't unlock while it's used - the borrow of @t
    /// doesn't see to that, as unlock() takes &self.
    pub unsafe fn leaf<'a>(&self, t: &'a TransAttempt<'_, 't>) -> Option<PathLeaf<'a>> {
        unsafe {
            // an iterator's path isn't cached: its key cache path is
            let path = t.trans().paths.get().add(self.path as usize).cast::<BtreePath<BtreeNode>>();
            let l = &raw mut (*path).l[0];
            // NULL above the root, or an error pointer
            (*l).b.get().ok()??;
            Some(PathLeaf { l, _trans: PhantomData })
        }
    }

    pub fn pos(&self) -> Bpos {
        self.pos
    }

    /// @k is a key at the iterator's level, above the leaves.
    pub fn prefetch_child(&self, t: &TransAttempt<'_, 't>, k: &BkeyI) -> Result<(), BchError> {
        let mut paths = t.trans().paths();
        let path = &mut paths[self.path];
        assert!(path.level() != 0, "prefetching below the leaves");
        let (btree, level) = (path.btree_id(), u32::from(path.level()));
        let ret = unsafe { c::bch2_btree_node_prefetch(t.raw(), path, k.as_ffi(), btree, level - 1) };
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
        self.set_pos(Bpos { snapshot, ..self.pos });
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
        pos: Bpos,
        flags: BtreeIterFlags,
    ) -> BtreeIter<'t> {
        unsafe {
            let mut iter = MaybeUninit::<BtreeIter<'t>>::uninit();

            c::bch2_trans_iter_init_outlined(
                trans.trans_raw(),
                iter.as_mut_ptr().cast(),
                Btree::from_raw(btree.into()).expect("invalid btree id"),
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
        pos: Bpos,
        level: u32,
        flags: BtreeIterFlags,
    ) -> BtreeIter<'t> {
        unsafe {
            let mut iter = MaybeUninit::<BtreeIter<'t>>::uninit();

            c::__bch2_trans_node_iter_init(
                trans.trans_raw(),
                iter.as_mut_ptr().cast(),
                Btree::from_raw(btree.into()).expect("invalid btree id"),
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

    pub fn peek_max<'k>(&'k mut self, t: &'k TransAttempt<'_, 't>, end: Bpos)
        -> Result<Option<BkeySC<'k>>, BchError>
    {
        self.peek_raw(t, |raw| unsafe { c::bch2_btree_iter_peek_max(raw, &end) })
    }

    /// At the iterator's level - new_level()'s; None if the btree isn't that deep.
    pub fn peek_node<'k>(&'k mut self, _t: &'k TransAttempt<'_, 't>)
        -> Result<Option<LockedNode<'k, BtreeNode>>, BchError>
    {
        // the node is locked by the iterator's path, while self is borrowed,
        // within the attempt
        unsafe {
            let b = c::bch2_btree_iter_peek_node(self.raw_mut());
            // NULL past the last
            Ok(errptr_or_null(b)?.map(|b| LockedNode::new(b.as_ref())))
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
    pub fn peek_slot_max<'k>(&'k mut self, t: &'k TransAttempt<'_, 't>, end: Bpos)
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

    pub fn peek_max_flags<'k>(&'k mut self, t: &'k TransAttempt<'_, 't>, end: Bpos,
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

    pub fn peek_max_type<'k>(&'k mut self, t: &'k TransAttempt<'_, 't>, end: Bpos,
                                     flags: BtreeIterFlags)
        -> Result<Option<BkeySC<'k>>, BchError>
    {
        self.peek_raw(t, |raw| unsafe { (*raw).peek_max_type_raw(end, flags) })
    }

    /// Peek with @peek, under attempt @t: the key, and the iterator it
    /// borrows, shared - both from the one &mut borrow, so neither outlives
    /// it and nothing moves the iterator while they're in use.
    pub(super) fn peek_with<'k, P>(&'k mut self, _t: &'k TransAttempt<'_, 't>, peek: &mut P)
        -> Result<Option<(&'k BtreeIter<'t>, BkeySC<'k>)>, BchError>
    where
        P: FnMut(*mut c::btree_iter) -> c::bkey_s_c,
    {
        // 'k: the key is valid while self is borrowed, shared, within the attempt
        let k = unsafe { bkey_s_c_to_result::<'k>(peek(self.raw_mut())) }?;
        Ok(k.map(|k| (&*self, k)))
    }

    /// peek_slot(), and the iterator with the key, shared: for an update at
    /// the key's position.
    pub fn peek_slot_with<'k>(&'k mut self, t: &'k TransAttempt<'_, 't>)
        -> Result<Option<(&'k BtreeIter<'t>, BkeySC<'k>)>, BchError>
    {
        self.peek_with(t, &mut |raw| unsafe { c::bch2_btree_iter_peek_slot(raw) })
    }

    pub fn peek<'k>(&'k mut self, t: &'k TransAttempt<'_, 't>)
        -> Result<Option<BkeySC<'k>>, BchError>
    {
        self.peek_max(t, SPOS_MAX)
    }

    pub fn peek_prev_min<'k>(&'k mut self, t: &'k TransAttempt<'_, 't>, min: Bpos)
        -> Result<Option<BkeySC<'k>>, BchError>
    {
        self.peek_raw(t, |raw| unsafe { c::bch2_btree_iter_peek_prev_min(raw, min) })
    }

    pub fn peek_prev<'k>(&'k mut self, t: &'k TransAttempt<'_, 't>)
        -> Result<Option<BkeySC<'k>>, BchError>
    {
        self.peek_prev_min(t, Bpos { inode: 0, offset: 0, snapshot: 0 })
    }

    pub fn traverse(&mut self, t: &TransAttempt<'_, 't>) -> Result<(), BchError> {
        let ret = unsafe { c::bch2_btree_iter_traverse(self.raw_mut()) };
        t.result(ret)
    }

    /// traverse(), its path at the iterator's position, not its search key -
    /// one past it, for extents: __bch2_btree_iter_traverse().
    pub fn traverse_at_pos(&mut self, t: &TransAttempt<'_, 't>) -> Result<(), BchError> {
        let ret = unsafe { __bch2_btree_iter_traverse(self.raw_mut()) };
        t.result(ret)
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
    pub(super) raw: BtreeIter<'t>,
}

impl<'t> BtreeNodeIter<'t> {
    pub fn new(
        trans: impl TransRef<'t>,
        btree: impl Into<u32>,
        pos: Bpos,
        locks_want: u32,
        depth: u32,
        flags: BtreeIterFlags,
    ) -> BtreeNodeIter<'t> {
        unsafe {
            let mut iter = MaybeUninit::<BtreeIter<'t>>::uninit();
            c::__bch2_trans_node_iter_init(
                trans.trans_raw(),
                iter.as_mut_ptr().cast(),
                Btree::from_raw(btree.into()).expect("invalid btree id"),
                pos,
                locks_want,
                depth,
                c::btree_iter_update_trigger_flags::from_bits_retain(flags.bits()),
            );

            BtreeNodeIter { raw: iter.assume_init() }
        }
    }

    pub fn peek(&mut self) -> Result<Option<LockedNode<'_, BtreeNode>>, BchError> {
        unsafe {
            let b = c::bch2_btree_iter_peek_node(self.raw.raw_mut());
            // NULL past the last
            Ok(errptr_or_null(b)?.map(|b| LockedNode::new(b.as_ref())))
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
        end: Bpos,
        flags: BtreeIterFlags,
    ) -> Result<Option<BkeySC<'i>>, BchError> {
        unsafe { bkey_s_c_to_result(self.raw.peek_max_type_raw(end, flags)) }
    }

}

pub(crate) struct BtreeNodeRef {
    raw: NonNull<BtreeNode>,
}

impl BtreeNodeRef {
    pub(crate) fn level(&self) -> u32 {
        unsafe { self.raw.as_ref().c.level.into() }
    }

    pub(crate) fn key(&self) -> &BkeyI {
        // SAFETY: a node we hold
        unsafe { self.raw.as_ref().contents() }.key_i()
    }

    pub(crate) fn key_sc(&self) -> BkeySC<'_, BtreePtrs> {
        BkeySC::from(self.key()).arm::<BtreePtrs>().expect("a btree node's key, its pointer")
    }

    pub(crate) fn btree_id(&self) -> Btree {
        Btree(unsafe { self.raw.as_ref().c.btree_id }.into())
    }

    /// The node's identity, for telling whether it's the same node as
    /// before - not for getting back to it.
    pub(crate) fn addr(&self) -> usize {
        self.raw.as_ptr() as usize
    }

    pub(crate) fn as_ptr(&self) -> *mut BtreeNode {
        self.raw.as_ptr()
    }
}

/// A node an iterator returned - backpointer_get_node()'s, say: held by that
/// iterator's path, as peek_ref()'s is.
impl From<LockedNode<'_, BtreeNode>> for BtreeNodeRef {
    fn from(b: LockedNode<'_, BtreeNode>) -> Self {
        Self { raw: NonNull::from(b.node()) }
    }
}

pub struct PathLeaf<'a> {
    l:      *mut btree_path_level<BtreeNode>,
    _trans: PhantomData<&'a ()>,
}

impl<'a> PathLeaf<'a> {
    pub fn node(&self) -> LockedNode<'a, BtreeNode> {
        unsafe { LockedNode::new((*self.l).b.get().ok().flatten().expect("a leaf with no node").as_ref()) }
    }

    /// Where the bset search tree failed at the path's key, if it did:
    /// the node's bfloat_to_text(). Peeks the node iterator, moving it past deleted
    /// keys.
    pub fn bfloat_to_text(&self, out: &mut Printbuf) {
        let b = self.node();
        unsafe { b.btree_node_iter_bfloat_to_text(b.contents(), out, &mut (*self.l).iter) }
    }
}
