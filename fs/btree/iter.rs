#![allow(non_camel_case_types)]

/* DOC_LATEX(btree-programmer-interface)
 *
 * This section describes the btree interface from a programmer's perspective:
 * how to look up keys, iterate, and perform updates.
 *
 * \paragraph{Btrees and keys}
 *
 * Keys are indexed in a small number of btrees: one for extents, another for
 * inodes, another for directory entries, and so on. New btree types can be
 * added for new features without on-disk format changes---many features were
 * added this way (e.g.\ erasure coding stripes, backpointers, accounting).
 *
 * The btree interface is iteration-oriented rather than lookup-oriented.
 * Lookup is not exposed as a primitive because most usage involves iterating
 * from a given position. The fundamental operation is: position an iterator,
 * then peek at or advance through keys.
 *
 * \paragraph{Transactions}
 *
 * All btree operations go through a transaction (\texttt{struct btree\_trans}).
 * A transaction provides:
 *
 * \begin{itemize}
 * \item Atomic multi-key updates across any combination of btrees
 * \item Automatic lock management and deadlock avoidance
 * \item Restart handling when locks are contended
 * \end{itemize}
 *
 * Transactions are allocated with \texttt{bch2\_trans\_get()} and released with
 * \texttt{bch2\_trans\_put()}. Within a transaction, iterators are created with
 * \texttt{bch2\_trans\_iter\_init()} specifying the btree ID and starting
 * position.
 *
 * \paragraph{Basic iteration}
 *
 * The core iteration pattern:
 *
 * \begin{verbatim}
 *     struct btree_iter iter;
 *     struct bkey_s_c k;
 *
 *     bch2_trans_iter_init(trans, &iter, BTREE_ID_extents,
 *                          POS(inode, offset), 0);
 *
 *     while ((k = bch2_btree_iter_peek(&iter)).k) {
 *         // process key
 *         bch2_btree_iter_advance(&iter);
 *     }
 *
 *     bch2_trans_iter_exit(trans, &iter);
 * \end{verbatim}
 *
 * The convenience macro \texttt{for\_each\_btree\_key()} wraps this pattern:
 *
 * \begin{verbatim}
 *     for_each_btree_key(trans, iter, BTREE_ID_extents,
 *                        POS(inode, 0), 0, k, ret)
 *         printk("extent at %llu:%llu\n",
 *                k.k->p.inode, k.k->p.offset);
 * \end{verbatim}
 *
 * \paragraph{Lookup}
 *
 * Lookup can be implemented via iteration: position an iterator, peek, and
 * check if the returned key matches:
 *
 * \begin{verbatim}
 *     bch2_trans_iter_init(trans, &iter, BTREE_ID_inodes,
 *                          POS(inum, 0), 0);
 *     k = bch2_btree_iter_peek_slot(&iter);
 *     if (k.k && k.k->type == KEY_TYPE_inode)
 *         // found it
 * \end{verbatim}
 *
 * The \texttt{peek\_slot()} variant returns a key at exactly the requested
 * position, synthesizing a deleted key if no key exists there. This is useful
 * for checking whether a specific slot is occupied.
 *
 * \paragraph{Updates}
 *
 * Updates are staged in the transaction and committed atomically:
 *
 * \begin{verbatim}
 *     struct bkey_i_inode *inode = ...;
 *
 *     ret = bch2_trans_update(trans, &iter, &inode->k_i, 0);
 *     if (ret)
 *         return ret;
 *
 *     ret = bch2_trans_commit(trans, NULL, NULL, 0);
 * \end{verbatim}
 *
 * Multiple updates can be staged before a single commit, and they will all
 * be applied atomically. Updates to different btrees within the same
 * transaction are also atomic.
 *
 * \paragraph{Restarts}
 *
 * Transactions may need to restart due to lock contention or other conflicts.
 * The standard pattern uses \texttt{lockrestart\_do()}:
 *
 * \begin{verbatim}
 *     ret = lockrestart_do(trans,
 *         do_something(trans) ?:
 *         bch2_trans_commit(trans, NULL, NULL, 0));
 * \end{verbatim}
 *
 * This automatically retries the operation if a restart is needed. All code
 * within the retry block must be idempotent---it may execute multiple times.
 */

/* DOC(btree-iterators)
 *
 * A btree iterator (`struct btree_iter`) maintains a full path from the root
 * of a btree down to a specific key in a leaf node. At each level, the
 * iterator holds a pointer to the btree node, a node-level iterator (which
 * merges across the multiple sorted bsets within that node), and a lock
 * sequence number for efficient relock-after-restart.
 *
 * The fundamental invariant is that `iter.pos` must always be consistent with
 * the node iterators: at every level, the node iterator points to the first
 * key >= `iter.pos`, and the previous key compares strictly less. This is the
 * correct position for inserting a new key at `iter.pos`.
 *
 * Iterators support several modes: `peek()` returns the next key and advances
 * `iter.pos` to match; `peek_slot()` returns the key at exactly `iter.pos`
 * (synthesizing a deleted key for empty slots); `peek_prev()` iterates
 * backwards. Extent iterators have special semantics: extents are indexed by
 * their end position, so the first extent covering a range starting at
 * `iter.pos` is found by searching for the first key strictly greater than
 * `iter.pos`. When iterating over extents by slots, holes between extents are
 * synthesized, guaranteeing that the returned keys exactly cover the keyspace
 * with monotonically increasing positions.
 *
 * Multiple iterators within a transaction coexist without invalidating each
 * other. On transaction restart, each iterator's saved position allows it to
 * resume from where it left off.
 */

use super::bkey::*;
pub use super::types::{BtreeIter, BtreeTrans};
pub use super::for_each::*;
pub use super::trans_attempt::*;
use crate::btree::journal_overlay;
use crate::btree::node::BtreeNode;
use crate::btree::path::{btree_path_level, BtreePath, KeyVal, LockedNode, PathIdx, Paths};
use crate::c;
use crate::data::extents::{bch2_cut_back_s, bch2_cut_front_s};
use crate::debug::params::debug_check_iterators;
use crate::debug::trace::events;
use crate::errcode::{BchError, bch_errcode, err_ptr, ret_to_c};
use crate::fs::Fs;
use crate::prt;
use crate::snapshots::snapshot::is_ancestor;
use crate::util::{max_of, Printbuf};
use crate::SPOS_MAX;
use bitflags::bitflags;
use core::cell::Cell;
use core::ffi::{c_ulong, c_int, c_uint};
use core::marker::PhantomData;
use core::ptr::{self, NonNull};
use crate::data::ptrs::BtreePtrs;
use crate::types::{c_default, Lent};
use cstruct_macros::{CStruct, c_export, c_extern};
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
    fn bch2_btree_key_cache_find(c: *mut Fs, btree_id: Btree, pos: Bpos) -> *mut c::bkey_cached;
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
        let trans = self.trans_c();
        self.set_dontneed_in(&mut trans.paths());
    }

    /// Move to @pos - in the iterator's own snapshot, unless it's
    /// all_snapshots - with no key there yet: the next peek finds it. Drops the
    /// update path, which was for the old position. The static inline
    /// bch2_btree_iter_set_pos().
    pub fn set_pos(&mut self, pos: Bpos) {
        if self.update_path != 0 {
            let trans = self.trans_c();
            self.put_update_path(&mut trans.paths());
        }
        self.set_pos_noput(pos);
    }

    pub(super) fn set_pos_in(&mut self, paths: &mut Paths<'_>, pos: Bpos) {
        self.put_update_path(paths);
        self.set_pos_noput(pos);
    }

    fn put_update_path(&mut self, paths: &mut Paths<'_>) {
        if self.update_path != 0 {
            self.trans_c().path_put(paths, self.update_path, self.intent());
        }
        self.update_path = 0;
    }

    fn set_pos_noput(&mut self, mut pos: Bpos) {
        if !self.iter_flags().contains(BtreeIterFlags::ALL_SNAPSHOTS) {
            pos.snapshot = self.snapshot;
        }

        self.k.set_key_type(BkeyType::deleted);
        self.k.p     = pos;
        self.k.size  = 0;
        self.pos     = pos;
    }

    fn intent(&self) -> bool {
        self.iter_flags().contains(BtreeIterFlags::INTENT)
    }

    /// The next key up to @end, walking as @flags say: backwards with prev, a
    /// slot at a time - holes included - with slots. The static inline
    /// bch2_btree_iter_peek_max_type().
    pub(super) fn peek_max_type_in(&mut self, paths: &mut Paths<'_>, end: Bpos, flags: BtreeIterFlags) -> Peeked {
        let slots = flags.contains(BtreeIterFlags::SLOTS);
        let prev  = flags.contains(BtreeIterFlags::PREV);

        match (prev, slots) {
            (false, false) => self.peek_max_in(paths, end),
            (true,  false) => self.peek_prev_min_in(paths, end),
            (true,  true)  =>
                if bkey_lt(self.pos, end) { Ok(None) }
                else { self.peek_prev_type_in(paths, flags) },
            // trimming, a slot is cut to the window, and an extents window
            // ends where @end starts: the slot there is past it
            (false, true) if self.trims() =>
                if bkey_ge(self.pos, end) { Ok(None) }
                else { self.peek_slot_max_in(paths, end) },
            (false, true) =>
                if bkey_gt(self.pos, end) { Ok(None) }
                else { self.peek_slot_in(paths) },
        }
    }

    /// The previous key, or with slots the slot here: the static inline
    /// bch2_btree_iter_peek_prev_type().
    fn peek_prev_type_in(&mut self, paths: &mut Paths<'_>, flags: BtreeIterFlags) -> Peeked {
        if flags.contains(BtreeIterFlags::SLOTS) {
            // reverse slots aren't defined for extents: peek_slot() is the slot from @pos on
            assert!(!self.is_extents(), "reverse slots on an extents iterator");
            self.peek_slot_in(paths)
        } else {
            self.peek_prev_min_in(paths, POS_MIN)
        }
    }

    pub(crate) fn raw_mut(&mut self) -> *mut c::btree_iter {
        core::ptr::from_mut(self).cast()
    }
}

/// A key, none past the end, or an error.
pub(super) type Peeked = Result<Option<KeyVal>, BchError>;

/// # Safety
/// @v is @k's value - NULL for a hole - valid for 'k.
unsafe fn bkey_s_c<'k>(k: &'k c::bkey, v: *const c::bch_val) -> BkeySC<'k> {
    BkeySC {
        k,
        // a hole's NULL value: a dangling, aligned one instead, as val_bytes()
        // makes a slice of it - empty, but a slice's pointer is never null
        v:    if !v.is_null() { v } else { NonNull::dangling().as_ptr() },
        iter: PhantomData,
        val:  PhantomData,
    }
}

#[cold]
#[inline(never)]
fn peek_trace(out: &mut Printbuf, fs: &Fs, k: Result<Option<BkeySC<'_>>, BchError>) {
    match k {
        Err(e)      => prt!(out, "{}", e),
        Ok(Some(k)) => bkey_val_to_text(out, Some(fs), k),
        Ok(None)    => prt!(out, "(null)"),
    }
}

enum FilterSnapshots {
    /* accept *k, exit caller's loop */
    Ok,
    /* search_key updated, retry */
    Continue,
    /* out of range, terminate scan */
    End,
}

pub(crate) fn btree_id_cached(btree: Btree) -> bool {
    matches!(btree, Btree::alloc | Btree::inodes | Btree::logged_ops | Btree::stripes | Btree::subvolumes)
}

fn btree_iter_flags(trans: &BtreeTrans<'_>, btree: Btree, level: u32, mut flags: BtreeIterFlags) -> BtreeIterFlags {
    if level != 0 || !btree_id_cached(btree) {
        flags.remove(BtreeIterFlags::CACHED | BtreeIterFlags::KEY_CACHED);
    } else if !flags.contains(BtreeIterFlags::CACHED) {
        flags |= BtreeIterFlags::KEY_CACHED;
    }

    if !flags.intersects(BtreeIterFlags::ALL_SNAPSHOTS | BtreeIterFlags::NOT_EXTENTS) && btree.is_extents() {
        flags |= BtreeIterFlags::IS_EXTENTS;
    }

    if !flags.contains(BtreeIterFlags::SNAPSHOT_FIELD) && !btree.has_snapshot_field() {
        flags.remove(BtreeIterFlags::ALL_SNAPSHOTS);
    }

    /*
     * Without snapshot semantics a snapshot field is just part of the key:
     * advancing past a key at (inode, offset, snapshot) must not skip the
     * others at (inode, offset).
     */
    if btree.has_snapshot_field() && !btree.has_snapshots() {
        flags |= BtreeIterFlags::ALL_SNAPSHOTS;
    }

    if !flags.contains(BtreeIterFlags::ALL_SNAPSHOTS) && btree.has_snapshots() {
        flags |= BtreeIterFlags::FILTER_SNAPSHOTS;
    }

    if trans.journal_replay_not_finished() {
        flags |= BtreeIterFlags::WITH_JOURNAL;
    }

    flags
}

impl<'t> BtreeIter<'t> {
    fn trans_c(&self) -> &'t BtreeTrans<'t> {
        unsafe { &*self.trans.cast::<BtreeTrans<'t>>() }
    }

    fn trims(&self) -> bool {
        self.iter_flags().contains(BtreeIterFlags::TRIM | BtreeIterFlags::IS_EXTENTS)
    }

    fn ip_allocated(&self) -> c_ulong {
        #[cfg(any(CONFIG_BCACHEFS_LOCK_TIME_STATS, CONFIG_BCACHEFS_DEBUG))]
        return self.ip_allocated;
        #[cfg(not(any(CONFIG_BCACHEFS_LOCK_TIME_STATS, CONFIG_BCACHEFS_DEBUG)))]
        0
    }

    fn successor(&self, p: Bpos) -> Bpos {
        /* Are we iterating over keys in all snapshots? */
        if self.iter_flags().contains(BtreeIterFlags::ALL_SNAPSHOTS) {
            p.successor()
        } else {
            Bpos { snapshot: self.snapshot, ..p.nosnap_successor() }
        }
    }

    fn predecessor(&self, p: Bpos) -> Bpos {
        /* Are we iterating over keys in all snapshots? */
        if self.iter_flags().contains(BtreeIterFlags::ALL_SNAPSHOTS) {
            p.predecessor()
        } else {
            Bpos { snapshot: self.snapshot, ..p.nosnap_predecessor() }
        }
    }

    fn search_key(&self) -> Bpos {
        if self.is_extents() && !bkey_eq(self.pos, POS_MAX) {
            self.successor(self.pos)
        } else {
            self.pos
        }
    }

    /// The key @k found: iter->k, and its value in what the iterator's paths
    /// hold, or the transaction's.
    pub(super) fn key_of(&self, k: Option<KeyVal>) -> Option<BkeySC<'_>> {
        k.map(|KeyVal(v)| unsafe { bkey_s_c(&self.k, v) })
    }

    /// @k - a journal key, or an update's - is what's found: its header ours.
    fn found_i(&mut self, k: &BkeyI) -> KeyVal {
        self.k = k.k;
        KeyVal((&raw const k.v).cast())
    }

    /* Debug: */

    fn verify_now(&self, paths: &mut Paths<'_>) {
        let trans = self.trans_c();
        let flags = self.iter_flags();

        assert_eq!(flags.contains(BtreeIterFlags::CACHED), paths[self.path].cached(),
                   "iterator and its path disagree on cached");

        assert!(!flags.contains(BtreeIterFlags::IS_EXTENTS | BtreeIterFlags::ALL_SNAPSHOTS),
                "an extents iterator over all snapshots");

        assert!(flags.contains(BtreeIterFlags::SNAPSHOT_FIELD) ||
                !flags.contains(BtreeIterFlags::ALL_SNAPSHOTS) ||
                self.btree().has_snapshot_field(),
                "all_snapshots on {}, which has no snapshot field", self.btree());

        if self.update_path != 0 {
            trans.path_verify_now(paths, self.update_path);
        }
        trans.path_verify_now(paths, self.path);
    }

    fn verify_entry_exit_now(&self) {
        let flags = self.iter_flags();

        assert!(!flags.contains(BtreeIterFlags::FILTER_SNAPSHOTS) || self.pos.snapshot != 0,
                "filtering snapshots at {}, in snapshot 0", self.pos);

        assert!(flags.contains(BtreeIterFlags::ALL_SNAPSHOTS) || self.pos.snapshot == self.snapshot,
                "iterator at {}, outside its snapshot {}", self.pos, self.snapshot);

        assert!(if flags.contains(BtreeIterFlags::ALL_SNAPSHOTS) {
            self.pos == self.k.p
        } else if !flags.contains(BtreeIterFlags::IS_EXTENTS) {
            bkey_eq(self.pos, self.k.p)
        } else {
            !bkey_lt(self.pos, self.k.start_pos()) && !bkey_gt(self.pos, self.k.p)
        }, "iterator at {}, its key {}", self.pos, self.k);
    }

    fn verify_ret_now(&self, paths: &mut Paths<'_>, k: Peeked) -> Result<(), BchError> {
        let trans = self.trans_c();

        if !self.iter_flags().contains(BtreeIterFlags::FILTER_SNAPSHOTS) {
            return Ok(());
        }

        let Ok(Some(_)) = k else {
            return Ok(());
        };
        let k = self.k;
        let t = unsafe { trans.attempt_in_progress() };

        assert!(is_ancestor(&t, self.snapshot, k.p.snapshot),
                "iter snap {} returned {}, not visible in it", self.snapshot, k);

        let mut copy = BtreeIter::init_in(trans, paths, self.btree(), self.pos,
                                          BtreeIterFlags::NOPRESERVE | BtreeIterFlags::ALL_SNAPSHOTS,
                                          crate::ret_ip!());
        let ret = copy.prev_in(paths).map(|prev| {
            if prev.is_none() {
                return;
            }
            let prev = copy.k;

            if bkey_eq(prev.p, k.p) && is_ancestor(&t, self.snapshot, prev.p.snapshot) {
                panic!("iter snap {}\nk    {}\nprev {}\n", self.snapshot, k, prev);
            }
        });
        copy.exit_in(paths);
        ret
    }

    fn checks() -> bool {
        cfg!(CONFIG_BCACHEFS_DEBUG) && debug_check_iterators()
    }

    fn verify(&self, paths: &mut Paths<'_>) {
        if Self::checks() {
            self.verify_now(paths);
        }
    }

    fn verify_entry_exit(&self) {
        if Self::checks() {
            self.verify_entry_exit_now();
        }
    }

    fn verify_ret(&self, paths: &mut Paths<'_>, k: Peeked) -> Result<(), BchError> {
        if Self::checks() {
            self.verify_ret_now(paths, k)
        } else {
            Ok(())
        }
    }

    pub(super) fn set_dontneed_in(&mut self, paths: &mut Paths<'_>) {
        let trans = self.trans_c();

        if self.path == 0 || trans.restarted().0 != 0 {
            return;
        }

        let path = &mut paths[self.path];
        path.bits.set_preserve(false);
        if path.ref_ == 1 {
            path.bits.set_should_be_locked(false);
        }
    }

    /* Btree iterators: */

    pub(super) fn traverse_at_pos_in(&mut self, paths: &mut Paths<'_>) -> Result<(), BchError> {
        self.trans_c().path_traverse(paths, self.path, self.iter_flags())
    }

    pub(super) fn traverse_in(&mut self, paths: &mut Paths<'_>) -> Result<(), BchError> {
        let trans = self.trans_c();
        let search_key = self.search_key();

        trans.verify_not_unlocked_or_in_restart();

        self.path = trans.path_set_pos(paths, self.path, search_key, self.intent(), self.ip_allocated());

        trans.path_traverse(paths, self.path, self.iter_flags())?;

        let level = paths[self.path].level() as usize;
        if !paths[self.path].node(level).is_null() {
            trans.path_set_should_be_locked(paths, self.path);
        }
        Ok(())
    }

    /// The node, held by the iterator's path while the iterator's borrowed.
    pub(super) fn peek_node_in(&mut self, paths: &mut Paths<'_>)
        -> Result<Option<LockedNode<'_, BtreeNode>>, BchError>
    {
        let trans = self.trans_c();

        ebug_assert!(!paths[self.path].cached());
        self.verify(paths);

        let b = self.traverse_in(paths).and_then(|()| {
            let path = &paths[self.path];
            let b = path.l[path.level() as usize].b.get()?;
            Ok(b.map(|b| unsafe { LockedNode::new(b.cast::<BtreeNode>().as_ref()) }))
        });

        if let Ok(Some(n)) = &b {
            let max_key = n.contents().key_i().k.p;
            assert!(max_key >= self.pos, "node ending at {}, before the iterator's position {}", max_key, self.pos);

            let min_key = n.contents().data().min_key;
            self.k = c::bkey::new();
            self.k.p = min_key;
            self.pos = min_key;

            self.path = trans.path_set_pos(paths, self.path, max_key, self.intent(), self.ip_allocated());
            trans.path_set_should_be_locked(paths, self.path);
        }

        self.verify_entry_exit();
        self.verify(paths);
        b
    }

    /* Iterate across keys (in leaf nodes only) */

    pub(super) fn advance_in(&mut self, paths: &mut Paths<'_>) -> bool {
        let mut pos = self.k.p;
        let ret = !if self.iter_flags().contains(BtreeIterFlags::ALL_SNAPSHOTS) {
            pos == SPOS_MAX
        } else {
            bkey_eq(pos, SPOS_MAX)
        };

        if ret && !self.is_extents() {
            pos = self.successor(pos);
        }
        self.set_pos_in(paths, pos);
        ret
    }

    pub(super) fn rewind_in(&mut self, paths: &mut Paths<'_>) -> bool {
        let mut pos = if self.is_extents() { self.k.start_pos() } else { self.k.p };
        let ret = !if self.iter_flags().contains(BtreeIterFlags::ALL_SNAPSHOTS) {
            pos == POS_MIN
        } else {
            bkey_eq(pos, POS_MIN)
        };

        if ret && !self.is_extents() {
            pos = self.predecessor(pos);
        }
        self.set_pos_in(paths, pos);
        ret
    }

    #[inline(never)]
    fn trans_peek_prev_updates(&mut self, paths: &Paths<'_>, search_key: Bpos, k: &mut Option<KeyVal>) {
        let path = &paths[self.path];
        let b = path.locked_btree_node(path.level() as usize).expect("a path peeked is locked");
        let end = b.contents().data().min_key;
        let btree = self.btree();

        for (i, u) in paths.update_keys() {

            if !i.bits.key_cache_already_flushed() && i.bits.btree_id() == btree &&
               u.k.p <= search_key && u.k.p >= if k.is_some() { self.k.p } else { end } {
                *k = Some(self.found_i(u));
            }
        }
    }

    #[inline(never)]
    fn trans_peek_updates(&mut self, paths: &Paths<'_>, search_key: Bpos, k: &mut Option<KeyVal>) {
        let path = &paths[self.path];
        let b = path.locked_btree_node(path.level() as usize).expect("a path peeked is locked");
        let end = b.contents().key_i().k.p;
        let btree = self.btree();
        let level = u32::from(self.min_depth);

        for (i, u) in paths.update_keys() {

            if !i.bits.key_cache_already_flushed() && i.bits.btree_id() == btree &&
               u32::from(i.bits.level()) == level &&
               u.k.p >= search_key && u.k.p <= if k.is_some() { self.k.p } else { end } {
                *k = Some(self.found_i(u));
            }
        }
    }

    #[inline(never)]
    fn trans_peek_slot_updates(&mut self, paths: &Paths<'_>, k: &mut Option<KeyVal>) {
        let btree = self.btree();
        let level = u32::from(self.min_depth);

        for (i, u) in paths.update_keys() {

            if !i.bits.key_cache_already_flushed() && i.bits.btree_id() == btree &&
               u32::from(i.bits.level()) == level &&
               u.k.p == self.pos {
                *k = Some(self.found_i(u));
            }
        }
    }

    fn journal_peek(&mut self, paths: &Paths<'_>, search_pos: Bpos, end_pos: Bpos) -> Option<&'t BkeyI> {
        let trans = self.trans_c();

        /*
         * BTREE_ITER_with_journal is the iterator's from its init; trans_begin()
         * puts the transaction's ref on the journal keys once replay is done,
         * and can't clear the flag on iterators it can't see - so the
         * transaction's ref, not the iterator's flag, says the keys are there:
         */
        if !trans.journal_replay_not_finished() {
            return None;
        }

        let level = u32::from(paths[self.path].level());

        journal_overlay::peek_max(unsafe { trans.fs().journal_keys_held() }, self.btree(), level,
                                  search_pos, end_pos, &mut self.journal_idx)
    }

    #[inline(never)]
    fn trans_peek_slot_journal(&mut self, paths: &Paths<'_>) -> Option<KeyVal> {
        let pos = paths[self.path].pos;

        self.journal_peek(paths, pos, pos).map(|k| self.found_i(k))
    }

    #[inline(never)]
    fn trans_peek_journal(&mut self, paths: &Paths<'_>, search_key: Bpos, k: &mut Option<KeyVal>) {
        let end = if k.is_some() {
            self.k.p
        } else {
            let path = &paths[self.path];
            path.locked_btree_node(path.level() as usize).expect("a path peeked is locked").contents().key_i().k.p
        };
        if let Some(next_journal) = self.journal_peek(paths, search_key, end) {
            *k = Some(self.found_i(next_journal));
        }
    }

    fn journal_peek_prev(&mut self, paths: &Paths<'_>, search_key: Bpos, end_pos: Bpos) -> Option<&'t BkeyI> {
        let trans = self.trans_c();

        if !trans.journal_replay_not_finished() {
            return None;
        }

        let level = u32::from(paths[self.path].level());

        journal_overlay::peek_prev_min(unsafe { trans.fs().journal_keys_held() }, self.btree(), level,
                                       search_key, end_pos, &mut self.journal_idx)
    }

    #[inline(never)]
    fn trans_peek_prev_journal(&mut self, paths: &Paths<'_>, search_key: Bpos, k: &mut Option<KeyVal>) {
        let end = if k.is_some() {
            self.k.p
        } else {
            let path = &paths[self.path];
            path.locked_btree_node(path.level() as usize).expect("a path peeked is locked")
                .contents().data().min_key
        };
        if let Some(next_journal) = self.journal_peek_prev(paths, search_key, end) {
            *k = Some(self.found_i(next_journal));
        }
    }

    /*
     * Checks btree key cache for key at iter->pos and returns it if present, or
     * bkey_s_c_null:
     */
    #[inline(never)]
    fn trans_peek_key_cache(&mut self, paths: &mut Paths<'_>, k: &mut Option<KeyVal>) -> Result<(), BchError> {
        if k.is_none() || self.k.key_type().is_deleted() {
            return Ok(());
        }
        let kp = self.k.p;

        let trans = self.trans_c();
        let flags = self.iter_flags();

        trans.verify_not_unlocked_or_in_restart();

        if flags.contains(BtreeIterFlags::KEY_CACHE_FILL) && self.pos == kp {
            return Ok(());
        }

        if unsafe { bch2_btree_key_cache_find(trans.fs().as_ptr(), self.btree(), kp) }.is_null() {
            return Ok(());
        }

        if self.key_cache_path.get() == 0 {
            self.key_cache_path.set(trans.path_get(paths, self.btree(), kp, self.intent().into(), 0,
                                                   flags | BtreeIterFlags::CACHED | BtreeIterFlags::CACHED_NOFILL,
                                                   crate::ret_ip!()));
        }

        self.key_cache_path.set(trans.path_set_pos(paths, self.key_cache_path.get(), kp, self.intent(),
                                                   self.ip_allocated()));

        let ret = trans.path_traverse(paths, self.key_cache_path.get(), flags | BtreeIterFlags::CACHED)
            .and_then(|()| trans.path_relock(paths, self.path));
        if let Err(e) = ret {
            self.set_pos_in(paths, self.pos);
            return Err(e);
        }

        let mut u = c::bkey::new();
        let Some(k2) = paths[self.key_cache_path.get()].peek_slot(&mut u) else {
            return Ok(());
        };

        if flags.contains(BtreeIterFlags::ALL_SNAPSHOTS) && kp != u.p {
            return Ok(());
        }

        self.k = u;
        *k = Some(k2);
        trans.path_set_should_be_locked(paths, self.key_cache_path.get());
        Ok(())
    }

    fn __peek(&mut self, paths: &mut Paths<'_>, search_key: &mut Bpos, end: Bpos) -> Peeked {
        let trans = self.trans_c();
        let flags = self.iter_flags();

        ebug_assert!(!paths[self.path].cached());
        self.verify(paths);

        /*
         * The last leaf a key we want can be in: an extent is in the leaf its end
         * is in, and one starting before @end can end anywhere in @end's inode
         */
        let last_leaf = if flags.contains(BtreeIterFlags::IS_EXTENTS) {
            Bpos { inode: end.inode, offset: u64::MAX, snapshot: 0 }
        } else {
            end
        };

        let k = loop {
            self.path = trans.path_set_pos(paths, self.path, *search_key, self.intent(), self.ip_allocated());

            if let Err(e) = trans.path_traverse(paths, self.path, flags) {
                /* ensure that iter->k is consistent with iter->pos: */
                self.set_pos_in(paths, self.pos);
                break Err(e);
            }

            let level = paths[self.path].level() as usize;
            if paths[self.path].l[level].b.is_null() {
                /* No btree nodes at requested level: */
                self.set_pos_in(paths, SPOS_MAX);
                break Ok(None);
            }

            trans.path_set_should_be_locked(paths, self.path);

            let mut k = paths[self.path].level_peek_all(&mut self.k);

            if flags.contains(BtreeIterFlags::KEY_CACHED) {
                if let Err(e) = self.trans_peek_key_cache(paths, &mut k) {
                    break Err(e);
                }
            }

            if flags.contains(BtreeIterFlags::WITH_JOURNAL) {
                self.trans_peek_journal(paths, *search_key, &mut k);
            }

            if trans.nr_updates.get() != 0 && !flags.contains(BtreeIterFlags::COMMITTED) {
                self.trans_peek_updates(paths, *search_key, &mut k);
            }

            if k.is_some() {
                if !self.k.key_type().is_deleted() {
                    break Ok(k);
                }

                /*
                 * If we've got a whiteout, and it's after the search
                 * key, advance the search key to the whiteout instead
                 * of just after the whiteout - it might be a btree
                 * whiteout, with a real key at the same position, since
                 * in the btree deleted keys sort before non deleted.
                 */
                let p = self.k.p;
                *search_key = if *search_key != p { p } else { p.successor() };

                /* in BTREE_ITER_filter_snapshots mode, we don't want a
                 * precise comparison against @end */
            } else {
                let path = &paths[self.path];
                let node_max = path.locked_btree_node(level).expect("a path peeked is locked").contents().key_i().k.p;

                if bkey_le(node_max, last_leaf) && node_max < SPOS_MAX {
                    /* Advance to next leaf node: */
                    *search_key = node_max.successor();
                } else {
                    /* End of btree: */
                    self.set_pos_in(paths, SPOS_MAX);
                    break Ok(None);
                }
            }
        };

        self.verify(paths);

        let fs = trans.fs();
        fs.event_trace(events::__btree_iter_peek, |out| peek_trace(out, fs, k.map(|k| self.key_of(k))));

        k
    }

    #[inline(never)]
    fn filter_snapshots(&mut self, paths: &mut Paths<'_>, search_key: &mut Bpos, end: Bpos)
        -> Result<FilterSnapshots, BchError>
    {
        let trans = self.trans_c();
        let flags = self.iter_flags();
        let kk = self.k;

        /*
         * We need to check against @end before FILTER_SNAPSHOTS because
         * if we get to a different inode than requested we might be
         * seeing keys for a different snapshot tree that will all be
         * filtered out.
         *
         * But we can't do the full check here, because bkey_start_pos()
         * isn't monotonically increasing before FILTER_SNAPSHOTS, and
         * that's what we check against in extents mode:
         */
        if if !flags.contains(BtreeIterFlags::IS_EXTENTS) {
            bkey_gt(kk.p, end)
        } else {
            kk.p.inode > end.inode
        } {
            return Ok(FilterSnapshots::End);
        }

        /*
         * If we want an update path we want to do this up front; also more
         * efficient than letting !bch2_snapshot_is_ancestor() skip these:
         */
        if kk.p.snapshot < self.snapshot {
            *search_key = Bpos { snapshot: self.snapshot, ..kk.p };
            return Ok(FilterSnapshots::Continue);
        }

        if self.update_path != 0 && !bkey_eq(paths[self.update_path].pos, kk.p) {
            trans.path_put(paths, self.update_path, self.intent());
            self.update_path = 0;
        }

        if flags.contains(BtreeIterFlags::INTENT) &&
           !flags.contains(BtreeIterFlags::IS_EXTENTS) &&
           self.update_path == 0 {
            /*
             * We may have to skip over keys in unrelated snapshot branches
             * before finding the key to return; that will not be the
             * update position for iter->snapshot - cache that:
             */
            paths.get_ll(self.path, self.intent());
            self.update_path = self.path;

            let with_snapshot = Bpos { snapshot: self.snapshot, ..kk.p };
            self.update_path = trans.path_set_pos(paths, self.update_path, with_snapshot, self.intent(),
                                                  crate::ret_ip!());
            trans.path_traverse(paths, self.update_path, flags)?;
        }

        /*
         * We can never have a key in a leaf node at POS_MAX, so we don't have
         * to check these successor() calls:
         */
        if !is_ancestor(&unsafe { trans.attempt_in_progress() }, self.snapshot, kk.p.snapshot) {
            *search_key = kk.p.successor();
            return Ok(FilterSnapshots::Continue);
        }

        if !flags.contains(BtreeIterFlags::NOFILTER_WHITEOUTS) && kk.key_type().is_extent_whiteout() {
            /*
             * KEY_TYPE_extent_whiteout indicates that there are no extents
             * overlapping with this whiteout - meaning bkey_start_pos() is
             * monotonically increasing when including KEY_TYPE_extent_whiteout.
             * Without this @end wouldn't be able to terminate searches and
             * we'd have to scan through tons of whiteouts.
             */
            if kk.key_type() == BkeyType::extent_whiteout && bkey_ge(kk.p, end) {
                return Ok(FilterSnapshots::End);
            }

            /*
             * Advance iter->pos incrementally, so on transaction restart we
             * don't restart scanning over large numbers of whiteouts. For
             * extent iterators, callers use iter->pos to mean "remaining
             * range we are processing", so we only advance on
             * KEY_TYPE_extent_whiteout.
             */
            if !flags.contains(BtreeIterFlags::IS_EXTENTS) || kk.key_type() == BkeyType::extent_whiteout {
                self.pos = Bpos { snapshot: self.snapshot, ..kk.p };
            }

            *search_key = self.successor(kk.p);
            return Ok(FilterSnapshots::Continue);
        }

        Ok(FilterSnapshots::Ok)
    }

    /*
     * BTREE_ITER_trim: an extent straddling the window [@start, @end) is returned
     * cut to it, and iter->k is cut with it: stepping by it stops at the window's
     * edge, where the walk ends. A key with no value is only its header, cut in
     * iter->k; one with a value - pointers, a reflink_p's idx, inline data, which
     * move with a cut - is cut in a copy in transaction memory. An update of what's
     * returned replaces only the window.
     */
    #[inline(never)]
    fn trim(&mut self, paths: &mut Paths<'_>, v: KeyVal, start: Bpos, end: Bpos) -> Result<KeyVal, BchError> {
        let u = &mut self.k;

        if bkey_le(start, u.start_pos()) && bkey_le(u.p, end) {
            return Ok(v);
        }

        if u.val_bytes() == 0 {
            if bkey_lt(u.start_pos(), start) {
                u.size = (u.p.offset - start.offset) as u32;
            }
            if bkey_gt(u.p, end) {
                u.resize((end.offset - u.start_pos().offset) as u32);
            }
            return Ok(v);
        }

        let trans = self.trans_c();
        let t = trans.kmalloc_in(paths, self.k.bytes())?.cast::<c::bkey_i>();

        let t = unsafe {
            (*t).k = self.k;
            ptr::copy_nonoverlapping(v.0.cast::<u8>(), (&raw mut (*t).v).cast::<u8>(), self.k.val_bytes());
            &mut *BkeyI::from_raw_mut(t)
        };
        bch2_cut_front_s(trans.fs(), start, BkeyS::from(&mut *t));
        bch2_cut_back_s(end, BkeyS::from(&mut *t));
        Ok(self.found_i(t))
    }

    fn maybe_trim(&mut self, paths: &mut Paths<'_>, k: Peeked, start: Bpos, end: Bpos) -> Peeked {
        let v = match k {
            Ok(Some(v)) if self.trims() => v,
            k => return k,
        };

        let v = self.trim(paths, v, start, end)?;
        /* what trimming promises: all of it in the window, and something */
        ebug_assert!(self.k.size != 0 && !bkey_lt(self.k.start_pos(), start) && !bkey_gt(self.k.p, end),
                     "trimmed to {}, outside the window {}-{}", self.k, start, end);
        Ok(Some(v))
    }

    /// peek_max()'s search: None past @end.
    fn peek_max_search(&mut self, paths: &mut Paths<'_>, end: Bpos) -> Peeked {
        let trans = self.trans_c();
        let flags = self.iter_flags();
        let mut search_key = self.search_key();

        if self.update_path != 0 {
            trans.path_put(paths, self.update_path, self.intent());
            self.update_path = 0;
        }

        let v = loop {
            let Some(v) = self.__peek(paths, &mut search_key, end)? else {
                return Ok(None);
            };

            if flags.contains(BtreeIterFlags::FILTER_SNAPSHOTS) &&
               (self.k.p.snapshot != self.snapshot || self.k.key_type().is_extent_whiteout() ||
                self.update_path != 0) {
                match self.filter_snapshots(paths, &mut search_key, end)? {
                    FilterSnapshots::Ok       => {}
                    FilterSnapshots::Continue => continue,
                    FilterSnapshots::End      => return Ok(None),
                }
            }

            break v;
        };

        let kk = self.k;

        /*
         * iter->pos should be mononotically increasing, and always be
         * equal to the key we just returned - except extents can
         * straddle iter->pos:
         */
        if !flags.contains(BtreeIterFlags::IS_EXTENTS) {
            self.pos = kk.p;
        } else if !bkey_gt(self.pos, kk.start_pos()) {
            self.pos = kk.start_pos();
        }

        if !flags.contains(BtreeIterFlags::ALL_SNAPSHOTS) {
            self.pos.snapshot = self.snapshot;
        }

        if if flags.contains(BtreeIterFlags::ALL_SNAPSHOTS) {
            self.pos > end
        } else if flags.contains(BtreeIterFlags::IS_EXTENTS) {
            bkey_ge(self.pos, end)
        } else {
            bkey_gt(self.pos, end)
        } {
            return Ok(None);
        }

        self.path = trans.path_set_pos(paths, self.path, kk.p, self.intent(), self.ip_allocated());

        trans.path_set_should_be_locked(paths, self.path);
        Ok(Some(v))
    }

    pub(super) fn peek_max_in(&mut self, paths: &mut Paths<'_>, end: Bpos) -> Peeked {
        let trans = self.trans_c();
        let flags = self.iter_flags();

        trans.verify_not_unlocked_or_in_restart();
        self.verify_entry_exit();
        ebug_assert!(!flags.contains(BtreeIterFlags::FILTER_SNAPSHOTS) ||
                     flags.contains(BtreeIterFlags::NOFILTER_WHITEOUTS) ||
                     !bkey_eq(end, POS_MAX),
                     "filtering whiteouts to POS_MAX: the filter peeks past @end");

        let mut k = trans.maybe_inject_restart(crate::ret_ip!())
            .and_then(|()| self.peek_max_search(paths, end));
        if let Ok(None) = k {
            self.set_pos_in(paths, end);
        }

        if self.update_path != 0 {
            match trans.path_relock(paths, self.update_path) {
                Err(e) => k = Err(e),
                Ok(()) => trans.path_set_should_be_locked(paths, self.update_path),
            }
        }

        if let Err(e) = self.verify_ret(paths, k) {
            self.set_pos_in(paths, self.pos);
            k = Err(e);
        }

        self.verify_entry_exit();

        let k = self.maybe_trim(paths, k, self.pos, end);

        let fs = trans.fs();
        fs.event_trace(events::btree_iter_peek_max, |out| peek_trace(out, fs, k.map(|k| self.key_of(k))));

        k
    }

    fn __peek_prev(&mut self, paths: &mut Paths<'_>, mut search_key: Bpos) -> Peeked {
        let trans = self.trans_c();
        let flags = self.iter_flags();

        self.verify(paths);

        let k = loop {
            self.path = trans.path_set_pos(paths, self.path, search_key, self.intent(), self.ip_allocated());

            if let Err(e) = trans.path_traverse(paths, self.path, flags) {
                /* ensure that iter->k is consistent with iter->pos: */
                self.set_pos_in(paths, self.pos);
                break Err(e);
            }

            let level = paths[self.path].level() as usize;
            if paths[self.path].l[level].b.is_null() {
                /* No btree nodes at requested level: */
                self.set_pos_in(paths, SPOS_MAX);
                break Ok(None);
            }
            self.path = trans.path_make_mut(paths, self.path, self.intent(), self.ip_allocated());

            trans.path_set_should_be_locked(paths, self.path);

            let mut k = paths[self.path].level_peek_all(&mut self.k);

            if k.is_none() || self.k.p > search_key {
                k = trans.path_level_prev(paths, self.path, &mut self.k);

                ebug_assert!(k.is_none() || self.k.p <= search_key,
                             "stepped back to {}, past the search key {}", self.k.p, search_key);
            }

            if flags.contains(BtreeIterFlags::KEY_CACHED) {
                if let Err(e) = self.trans_peek_key_cache(paths, &mut k) {
                    break Err(e);
                }
            }

            if flags.contains(BtreeIterFlags::WITH_JOURNAL) {
                self.trans_peek_prev_journal(paths, search_key, &mut k);
            }

            if trans.nr_updates.get() != 0 && !flags.contains(BtreeIterFlags::COMMITTED) {
                self.trans_peek_prev_updates(paths, search_key, &mut k);
            }

            if k.is_some() && !self.k.key_type().is_deleted() {
                break Ok(k);
            } else if k.is_some() {
                search_key = self.k.p.predecessor();
            } else {
                let path = &paths[self.path];
                let min_key = path.locked_btree_node(level).expect("a path peeked is locked")
                    .contents().data().min_key;

                if min_key != POS_MIN {
                    /* Advance to previous leaf node: */
                    search_key = min_key.predecessor();
                } else {
                    /* Start of btree: */
                    self.set_pos_in(paths, POS_MIN);
                    break Ok(None);
                }
            }
        };

        /*
         * TODO: unlike bch2_btree_iter_peek_max(), which does a normalizing
         * bch2_btree_path_set_pos(&k.k->p) on return, peek_prev sets path->pos
         * directly (bch2_btree_path_level_prev / the k.k->p store above) and never
         * re-runs up_until_good_node - so on return path->pos may be
         * inconsistent with path->l[level].b above the leaf. The only guard is
         * bch2_btree_iter_verify (static-branch gated, runs rarely in CI), so
         * the inconsistency escapes to the always-on
         * bch2_trans_node_verify_not_in_iters() BUG in a later node split
         * (seen: copygc_torture_no_checksum, inject_transaction_restarts=1).
         * Fix likely mirrors peek_max's trailing set_pos, but the reverse-
         * iteration pos semantics need thought first.
         */
        self.verify(paths);
        k
    }

    /*
     * The key visible in @iter->snapshot at exactly @pos - for an extents
     * iterator, the extent ending at @pos - or bkey_s_c_null if nothing is: the
     * nearest version among iter->snapshot and its ancestors, unless that's a
     * whiteout.
     *
     * Resolved forwards, as peek_slot() does: our own snapshot's version comes
     * first, then each ancestor's in turn, so the first one visible is the
     * answer, journal keys and the transaction's updates included. Searching
     * backwards meets the furthest ancestor first and would have to remember
     * candidates.
     *
     * Done on a copy without is_extents, so the lookup is of exactly @pos: an
     * extents peek_slot() of the extent containing @pos - 1 would scan on to the
     * next visible extent, and a backward search calling this at each position
     * through a run of whiteouts would rescan it from each.
     *
     * On success iter->path is the copy's, at the key, and iter->k holds it.
     */
    fn peek_visible_at(&mut self, paths: &mut Paths<'_>, pos: Bpos) -> Peeked {
        let mut iter2 = self.copy_in(paths);
        iter2.clear_flags(BtreeIterFlags::IS_EXTENTS | BtreeIterFlags::NOFILTER_WHITEOUTS);

        iter2.set_pos_in(paths, Bpos { snapshot: self.snapshot, ..pos });

        let k = match iter2.peek_slot_in(paths) {
            /* A hole, or a whiteout - and it points into iter2: */
            Ok(Some(_)) if iter2.k.key_type().is_deleted() => Ok(None),
            Ok(Some(v)) => {
                core::mem::swap(&mut self.path, &mut iter2.path);
                self.key_cache_path.swap(&iter2.key_cache_path);
                self.k = iter2.k;
                Ok(Some(v))
            }
            k => k,
        };
        iter2.exit_in(paths);
        k
    }

    /// peek_prev_min()'s search: None past @end.
    fn peek_prev_min_search(&mut self, paths: &mut Paths<'_>, end: Bpos) -> Peeked {
        let trans = self.trans_c();
        let flags = self.iter_flags();
        let mut search_key = self.pos;

        /*
         * Every snapshot's key at this position: an ancestor's sorts after
         * iter->snapshot's, and searching back from iter->pos would skip it -
         * for extents, the one ending here:
         */
        if flags.contains(BtreeIterFlags::FILTER_SNAPSHOTS) {
            search_key.snapshot = u32::MAX;
        }

        let v = loop {
            let Some(mut v) = self.__peek_prev(paths, search_key)? else {
                return Ok(None);
            };

            if flags.contains(BtreeIterFlags::FILTER_SNAPSHOTS) {
                let pos = self.k.p;

                /*
                 * We need to check against @end before FILTER_SNAPSHOTS because
                 * if we get to a different inode that requested we might be
                 * seeing keys for a different snapshot tree that will all be
                 * filtered out.
                 */
                if bkey_lt(pos, end) {
                    return Ok(None);
                }

                if !is_ancestor(&unsafe { trans.attempt_in_progress() }, self.snapshot, pos.snapshot) {
                    search_key = pos.predecessor();
                    continue;
                }

                /*
                 * A version visible in iter->snapshot is at this
                 * position: which one we see - if any - is decided
                 * forwards, from iter->snapshot's own up:
                 */
                v = match self.peek_visible_at(paths, pos) {
                    Ok(Some(v)) => v,
                    Ok(None) => {
                        if !bkey_gt(pos, end) {
                            return Ok(None);
                        }

                        search_key = self.predecessor(pos);
                        search_key.snapshot = u32::MAX;
                        continue;
                    }
                    Err(e) => {
                        /*
                         * iter->k still holds the version
                         * __bch2_btree_iter_peek_prev() found: make it
                         * consistent with iter->pos, as its own error
                         * path does:
                         */
                        self.set_pos_in(paths, self.pos);
                        return Err(e);
                    }
                };
            }

            let kk = self.k;

            ebug_assert!(if flags.contains(BtreeIterFlags::ALL_SNAPSHOTS) {
                kk.p <= self.pos
            } else if flags.contains(BtreeIterFlags::IS_EXTENTS) {
                bkey_lt(kk.start_pos(), self.pos)
            } else {
                bkey_le(kk.p, self.pos)
            }, "peeking back from {}, found {}", self.pos, kk);

            if if flags.contains(BtreeIterFlags::ALL_SNAPSHOTS) {
                kk.p < end
            } else if flags.contains(BtreeIterFlags::IS_EXTENTS) {
                bkey_le(kk.p, end)
            } else {
                bkey_lt(kk.p, end)
            } {
                return Ok(None);
            }

            break v;
        };

        /* Extents can straddle iter->pos: */
        self.pos = core::cmp::min(self.pos, self.k.p);

        if flags.contains(BtreeIterFlags::FILTER_SNAPSHOTS) {
            self.pos.snapshot = self.snapshot;
        }

        self.maybe_trim(paths, Ok(Some(v)), end, self.pos)
    }

    pub(super) fn peek_prev_min_in(&mut self, paths: &mut Paths<'_>, end: Bpos) -> Peeked {
        let flags = self.iter_flags();

        /* trimming, the window is [@end, iter->pos): nothing in an empty one */
        if self.trims() && bkey_le(self.pos, end) {
            self.set_pos_in(paths, end);
            return Ok(None);
        }

        if flags.intersects(BtreeIterFlags::IS_EXTENTS | BtreeIterFlags::FILTER_SNAPSHOTS) &&
           !bkey_eq(self.pos, POS_MAX) &&
           !(flags.contains(BtreeIterFlags::IS_EXTENTS) && self.pos.offset == u64::MAX) {
            /*
             * bkey_start_pos(), for extents, is not monotonically
             * increasing until after filtering for snapshots:
             *
             * Thus, for extents we need to search forward until we find a
             * real visible extents - easiest to just use peek_slot() (which
             * internally uses peek() for extents) - untrimmed, as that would
             * cut it to the wrong side of iter->pos
             */
            let k = self.__peek_slot(paths, SPOS_MAX, crate::ret_ip!())?;

            if k.is_some() && !self.k.key_type().is_deleted() &&
               (!flags.contains(BtreeIterFlags::IS_EXTENTS) || bkey_lt(self.k.start_pos(), self.pos)) {
                return self.maybe_trim(paths, Ok(k), end, self.pos);
            }
        }

        let trans = self.trans_c();

        trans.verify_not_unlocked_or_in_restart();
        self.verify_entry_exit();
        ebug_assert!(!flags.contains(BtreeIterFlags::FILTER_SNAPSHOTS) || self.pos.inode == end.inode,
                     "filtered iteration back from {} to {}: one inode only", self.pos, end);

        let k = trans.maybe_inject_restart(crate::ret_ip!())
            .and_then(|()| self.peek_prev_min_search(paths, end));
        if let Ok(None) = k {
            self.set_pos_in(paths, end);
        }

        self.verify_entry_exit();
        self.verify(paths);

        let fs = trans.fs();
        fs.event_trace(events::btree_iter_peek_prev_min, |out| peek_trace(out, fs, k.map(|k| self.key_of(k))));

        k
    }

    pub(super) fn prev_in(&mut self, paths: &mut Paths<'_>) -> Peeked {
        if !self.rewind_in(paths) {
            return Ok(None);
        }

        self.peek_prev_min_in(paths, POS_MIN)
    }

    fn __peek_slot(&mut self, paths: &mut Paths<'_>, end: Bpos, ip: c_ulong) -> Peeked {
        let trans = self.trans_c();
        let flags = self.iter_flags();
        let mut k: Peeked;

        trans.verify_not_unlocked_or_in_restart();
        self.verify(paths);
        self.verify_entry_exit();
        ebug_assert!(paths[self.path].level() == 0 || !flags.contains(BtreeIterFlags::KEY_CACHED),
                     "a key cache iterator above the leaves");

        'out2: {
            'out: {
                if let Err(e) = trans.maybe_inject_restart(ip) {
                    k = Err(e);
                    break 'out;
                }

                /* extents can't span inode numbers: */
                if flags.contains(BtreeIterFlags::IS_EXTENTS) && self.pos.offset == u64::MAX {
                    if self.pos.inode == u64::MAX {
                        k = Ok(None);
                        break 'out2;
                    }

                    self.set_pos_in(paths, self.pos.nosnap_successor());
                }

                let search_key = self.search_key();
                self.path = trans.path_set_pos(paths, self.path, search_key, self.intent(), self.ip_allocated());

                if let Err(e) = trans.path_traverse(paths, self.path, flags) {
                    k = Err(e);
                    break 'out;
                }

                let level = paths[self.path].level() as usize;
                if paths[self.path].node(level).is_null() {
                    k = Ok(None);
                    break 'out2;
                }

                trans.path_set_should_be_locked(paths, self.path);

                if flags.contains(BtreeIterFlags::CACHED) ||
                   !flags.intersects(BtreeIterFlags::IS_EXTENTS | BtreeIterFlags::FILTER_SNAPSHOTS) {
                    /*
                     * Consult sources in the same order as bch2_btree_iter_peek_max():
                     * btree, then key cache, then journal, then the transaction's own
                     * updates - each overlaying the last, so an in-transaction update
                     * wins. Crucially the key cache is peeked before trans updates, so
                     * the key_cache_path is established even when we already have an
                     * update for this key; otherwise a second update to the same cached
                     * key in one transaction would skip the cache and livelock against
                     * a dirty cached entry in bch2_trans_update_get_key_cache().
                     */
                    let mut v = paths[self.path].peek_slot(&mut self.k);

                    if flags.contains(BtreeIterFlags::KEY_CACHED) {
                        if let Err(e) = self.trans_peek_key_cache(paths, &mut v) {
                            k = Err(e);
                            break 'out;
                        }
                    }

                    if flags.contains(BtreeIterFlags::WITH_JOURNAL) {
                        if let Some(j) = self.trans_peek_slot_journal(paths) {
                            v = Some(j);
                        }
                    }

                    if trans.nr_updates.get() != 0 && !flags.contains(BtreeIterFlags::COMMITTED) {
                        self.trans_peek_slot_updates(paths, &mut v);
                    }

                    if v.is_some() &&
                       self.k.key_type().is_extent_whiteout() &&
                       flags.contains(BtreeIterFlags::FILTER_SNAPSHOTS) &&
                       !flags.contains(BtreeIterFlags::NOFILTER_WHITEOUTS) {
                        self.k.set_key_type(BkeyType::deleted);
                    }
                    k = Ok(v);
                } else {
                    let mut search_end = self.pos;

                    /*
                     * To the inode's end, not @end: an extent is in the leaf its end is
                     * in, and one starting before @end can end past the leaf a peek to
                     * @end stops in
                     */
                    if flags.contains(BtreeIterFlags::IS_EXTENTS) {
                        search_end.offset = u64::MAX;
                    }

                    ebug_assert!(paths[self.path].level() == 0);

                    let mut iter2 = self.copy_in(paths);
                    iter2.set_flags(BtreeIterFlags::NOFILTER_WHITEOUTS);
                    /* k below is iter2's value under iter->k, a copy of iter2.k: untrimmed, they agree */
                    iter2.clear_flags(BtreeIterFlags::TRIM);

                    k = loop {
                        let k = iter2.peek_max_in(paths, search_end);
                        if iter2.is_extents() && matches!(k, Ok(Some(_))) &&
                           iter2.k.key_type() == BkeyType::whiteout {
                            iter2.set_pos_in(paths, iter2.k.p);
                            continue;
                        }

                        break k;
                    };

                    if let Ok(Some(_)) = k {
                        self.key_cache_path.swap(&iter2.key_cache_path);
                        self.k = iter2.k;
                    }
                    iter2.exit_in(paths);

                    let Ok(v) = k else {
                        break 'out;
                    };

                    if v.is_some() &&
                       self.k.key_type().is_extent_whiteout() &&
                       flags.contains(BtreeIterFlags::FILTER_SNAPSHOTS) &&
                       !flags.contains(BtreeIterFlags::NOFILTER_WHITEOUTS) {
                        self.k.set_key_type(BkeyType::deleted);
                    }

                    let mut next = if v.is_some() { self.k.start_pos() } else { POS_MAX };
                    /* a hole ends at @end at the latest */
                    if flags.contains(BtreeIterFlags::IS_EXTENTS) && bkey_lt(end, next) {
                        next = end;
                    }

                    if bkey_lt(self.pos, next) {
                        self.k = c::bkey::new();
                        self.k.p = self.pos;

                        if flags.contains(BtreeIterFlags::IS_EXTENTS) {
                            let to = if next.inode == self.pos.inode { next.offset } else { u64::MAX };
                            self.k.resize((to - self.pos.offset).min(u64::from(max_of(&self.k.size))) as u32);
                            ebug_assert!(self.k.size != 0);
                        }

                        k = Ok(Some(KeyVal(ptr::null())));
                    }
                }
            }

            self.verify_entry_exit();
            self.verify(paths);
            if let Err(e) = self.verify_ret(paths, k) {
                k = Err(e);
            }
        }

        let fs = trans.fs();
        fs.event_trace(events::btree_iter_peek_slot, |out| peek_trace(out, fs, k.map(|k| self.key_of(k))));

        k
    }

    pub(super) fn peek_slot_in(&mut self, paths: &mut Paths<'_>) -> Peeked {
        let k = self.__peek_slot(paths, SPOS_MAX, crate::ret_ip!());

        self.maybe_trim(paths, k, self.pos, SPOS_MAX)
    }

    pub(super) fn peek_slot_max_in(&mut self, paths: &mut Paths<'_>, end: Bpos) -> Peeked {
        ebug_assert!(!self.is_extents() || bkey_lt(self.pos, end),
                     "an extents slot at {}, not before its end {}", self.pos, end);

        let k = self.__peek_slot(paths, end, crate::ret_ip!());

        self.maybe_trim(paths, k, self.pos, end)
    }

    pub(super) fn prev_slot_in(&mut self, paths: &mut Paths<'_>) -> Peeked {
        /* reverse slots aren't defined for extents: peek_slot() is the slot from @pos on */
        assert!(!self.is_extents(), "reverse slots on an extents iterator");

        if !self.rewind_in(paths) {
            return Ok(None);
        }

        self.peek_slot_in(paths)
    }

    /// The key, the root node's, held by the iterator's path.
    pub(super) fn peek_root_in(&mut self, paths: &mut Paths<'_>, trans: &BtreeTrans<'_>, btree: Btree,
                               level: u32) -> Result<Option<NonNull<BkeyI>>, BchError> {
        let fs = trans.fs();

        while fs.btree.cache.roots.node(btree.0).is_some_and(|root| level == u32::from(root.c.level) + 1) {
            self.exit_in(paths);
            *self = BtreeIter::node_iter_init_in(trans, paths, btree, POS_MIN, 0, level - 1,
                                                 BtreeIterFlags::NOT_EXTENTS | BtreeIterFlags::ALL_SNAPSHOTS,
                                                 crate::ret_ip!());
            let b =self.peek_node_in(paths)?.expect("a root's level has a node");

            if fs.btree.cache.roots.node(btree.0).is_none_or(|r| !ptr::eq(r, b.node())) {
                continue;
            }

            if b.is_fake() {
                break;
            }

            return Ok(Some(NonNull::from(b.contents().key_i())));
        }

        Ok(None)
    }

    fn holds_paths(&self) -> bool {
        self.path != 0 || self.update_path != 0 || self.key_cache_path.get() != 0
    }

    pub(super) fn exit_in(&mut self, paths: &mut Paths<'_>) {
        if self.holds_paths() {
            let trans = self.trans_c();
            let intent = self.intent();

            if self.update_path != 0 {
                trans.path_put(paths, self.update_path, intent);
            }
            if self.path != 0 {
                trans.path_put(paths, self.path, intent);
            }
            if self.key_cache_path.get() != 0 {
                trans.path_put(paths, self.key_cache_path.get(), intent);
            }
        }
        self.path = 0;
        self.update_path = 0;
        self.key_cache_path.set(0);
        self.trans = ptr::null_mut();
    }

    #[allow(clippy::too_many_arguments)]
    fn init_common(trans: &BtreeTrans<'_>, paths: &mut Paths<'_>, btree: Btree, pos: Bpos,
                   locks_want: u32, depth: u32, flags: BtreeIterFlags, ip: c_ulong) -> Self {
        let mut iter = BtreeIter::uninit();

        iter.trans = ptr::from_ref(trans).cast_mut().cast();
        iter.set_btree(btree);
        iter.flags = flags.bits();
        iter.snapshot = pos.snapshot;
        iter.pos = pos;
        iter.k = c::bkey { p: pos, ..c::bkey::new() };
        #[cfg(any(CONFIG_BCACHEFS_LOCK_TIME_STATS, CONFIG_BCACHEFS_DEBUG))]
        {
            iter.ip_allocated = ip;
        }
        iter.path = trans.path_get(paths, btree, pos, locks_want, depth, flags, ip);
        iter
    }

    pub(super) fn init_in(trans: &BtreeTrans<'_>, paths: &mut Paths<'_>, btree: Btree, pos: Bpos,
                          flags: BtreeIterFlags, ip: c_ulong) -> Self {
        Self::init_common(trans, paths, btree, pos, 0, 0, btree_iter_flags(trans, btree, 0, flags), ip)
    }

    #[allow(clippy::too_many_arguments)]
    fn init_ll_in(trans: &BtreeTrans<'_>, paths: &mut Paths<'_>, btree: Btree, pos: Bpos,
                  locks_want: u32, depth: u32, flags: BtreeIterFlags, ip: c_ulong) -> Self {
        let mut iter = Self::init_common(trans, paths, btree, pos, locks_want, depth,
                                         btree_iter_flags(trans, btree, depth, flags), ip);

        iter.min_depth = depth as u8;
        #[cfg(CONFIG_BCACHEFS_DEBUG)]
        {
            let path = &paths[iter.path];
            assert!(u32::from(path.locks_want()) >= locks_want.min(c::BTREE_MAX_DEPTH));
            assert!(u32::from(path.level()) == depth);
            assert!(u32::from(iter.min_depth) == depth);
        }
        iter
    }

    #[allow(clippy::too_many_arguments)]
    fn node_iter_init_in(trans: &BtreeTrans<'_>, paths: &mut Paths<'_>, btree: Btree, pos: Bpos,
                         locks_want: u32, depth: u32, mut flags: BtreeIterFlags, ip: c_ulong) -> Self {
        flags |= BtreeIterFlags::NOT_EXTENTS;
        flags |= BtreeIterFlags::SNAPSHOT_FIELD;
        flags |= BtreeIterFlags::ALL_SNAPSHOTS;

        if depth == 0 && btree_id_cached(btree) {
            flags |= BtreeIterFlags::KEY_CACHED;
        }

        Self::init_ll_in(trans, paths, btree, pos, locks_want, depth, flags, ip)
    }

    pub(super) fn copy_in(&self, paths: &mut Paths<'_>) -> Self {
        let mut dst = unsafe { ptr::read(self) };

        #[cfg(any(CONFIG_BCACHEFS_LOCK_TIME_STATS, CONFIG_BCACHEFS_DEBUG))]
        {
            dst.ip_allocated = crate::ret_ip!();
        }
        if self.path != 0 {
            paths.get_ll(self.path, self.intent());
        }
        if self.update_path != 0 {
            paths.get_ll(self.update_path, self.intent());
        }
        dst.key_cache_path = Cell::new(0);
        dst
    }
}

fn btree_iter_params_valid(btree: Btree, level: u32, start: Bpos, end: Bpos, flags: BtreeIterFlags) -> bool {
    if btree.0 >= c::BTREE_ID_NR {
        return false;
    }

    if level >= c::BTREE_MAX_DEPTH {
        return false;
    }

    /* Interior nodes span snapshots; filtering only makes sense at level 0: */
    if level != 0 && !flags.contains(BtreeIterFlags::ALL_SNAPSHOTS) {
        return false;
    }

    if btree.has_snapshots() && !flags.contains(BtreeIterFlags::ALL_SNAPSHOTS) {
        /* No snapshot to filter against: */
        if start.snapshot == 0 {
            return false;
        }

        /* Whiteout filtering peeks ahead of the end pos: */
        if end == POS_MAX {
            return false;
        }

        /* Backwards filtered iteration requires a single inode: */
        if flags.contains(BtreeIterFlags::PREV) && start.inode != end.inode {
            return false;
        }
    }

    /* Reverse slots aren't defined for extents: */
    if flags.contains(BtreeIterFlags::PREV | BtreeIterFlags::SLOTS) &&
       !flags.intersects(BtreeIterFlags::ALL_SNAPSHOTS | BtreeIterFlags::NOT_EXTENTS) &&
       btree.is_extents() {
        return false;
    }

    true
}

// ── For C ───────────────────────────────────────────────────────────────────

/// # Safety
/// @iter's transaction lends its path table for the call.
unsafe fn with_paths<R>(iter: *mut c::btree_iter, f: impl FnOnce(&mut BtreeIter<'static>, &mut Paths<'_>) -> R) -> R {
    unsafe {
        let iter = &mut *iter.cast::<BtreeIter<'static>>();
        let mut paths = Paths::lent(&*iter.trans);
        f(iter, &mut paths)
    }
}

/// with_paths(), for a peek: its key as C has them.
///
/// # Safety
/// As with_paths()'s.
unsafe fn peek_c(iter: *mut c::btree_iter, f: impl FnOnce(&mut BtreeIter<'static>, &mut Paths<'_>) -> Peeked)
    -> c::bkey_s_c
{
    unsafe {
        with_paths(iter, |iter, paths| match f(iter, paths) {
            Ok(Some(KeyVal(v))) => c::bkey_s_c { k: &raw const iter.k, v },
            Ok(None)            => c::bkey_s_c::default(),
            Err(e)              => c::bkey_s_c { k: err_ptr(e), v: ptr::null() },
        })
    }
}

/// # Safety
/// @iter is an initialized iterator, which C hands over for the call.
#[c_export]
pub unsafe fn bch2_set_btree_iter_dontneed(iter: *mut c::btree_iter) {
    unsafe { with_paths(iter, |iter, paths| iter.set_dontneed_in(paths)) }
}

/// # Safety
/// As bch2_set_btree_iter_dontneed()'s.
#[c_export]
#[must_use]
pub unsafe fn bch2_btree_iter_traverse(iter: *mut c::btree_iter) -> c_int {
    unsafe { with_paths(iter, |iter, paths| ret_to_c(iter.traverse_in(paths))) }
}

/// # Safety
/// As bch2_set_btree_iter_dontneed()'s.
#[c_export]
pub unsafe fn bch2_btree_iter_peek_node(iter: *mut c::btree_iter) -> *mut BtreeNode {
    unsafe {
        with_paths(iter, |iter, paths| match iter.peek_node_in(paths) {
            Ok(b)  => b.map_or(ptr::null_mut(), |b| ptr::from_ref(b.node()).cast_mut()),
            Err(e) => err_ptr(e),
        })
    }
}

/// # Safety
/// As bch2_set_btree_iter_dontneed()'s.
#[c_export]
pub unsafe fn bch2_btree_iter_advance(iter: *mut c::btree_iter) -> bool {
    unsafe { with_paths(iter, |iter, paths| iter.advance_in(paths)) }
}

/// # Safety
/// As bch2_set_btree_iter_dontneed()'s.
#[c_export]
pub unsafe fn bch2_btree_iter_rewind(iter: *mut c::btree_iter) -> bool {
    unsafe { with_paths(iter, |iter, paths| iter.rewind_in(paths)) }
}

/**
 * bch2_btree_iter_peek_max() - returns first key greater than or equal to
 * iterator's current position
 * @iter:	iterator to peek from
 * @end:	search limit: returns keys less than or equal to @end
 *
 * Returns:	key if found, or an error extractable with bkey_err().
 */
/// # Safety
/// As bch2_set_btree_iter_dontneed()'s.
#[c_export]
pub unsafe fn bch2_btree_iter_peek_max(iter: *mut c::btree_iter, end: *const Bpos) -> c::bkey_s_c {
    unsafe { peek_c(iter, |iter, paths| iter.peek_max_in(paths, *end)) }
}

/**
 * bch2_btree_iter_peek_prev_min() - returns first key less than or equal to
 * iterator's current position
 * @iter:	iterator to peek from
 * @end:	search limit: returns keys greater than or equal to @end
 *
 * With BTREE_ITER_filter_snapshots: the greatest key visible in
 * iter->snapshot at or below iter->pos - for extents, starting before it -
 * and at or above @end. What's visible at a position is the version there of
 * the nearest of iter->snapshot and its ancestors, unless that's a whiteout:
 * then nothing is.
 *
 * Snapshot IDs decrease from the root down, so searching backwards meets a
 * position's versions furthest ancestor first; the nearest is found forwards
 * instead (btree_iter_peek_visible_at()), once per position that has one.
 *
 * With BTREE_ITER_trim, an extent is cut to [@end, iter->pos), and iter->pos
 * is left at the end of what's returned.
 *
 * Returns:	key if found, or an error extractable with bkey_err().
 */
/// # Safety
/// As bch2_set_btree_iter_dontneed()'s.
#[c_export]
pub unsafe fn bch2_btree_iter_peek_prev_min(iter: *mut c::btree_iter, end: Bpos) -> c::bkey_s_c {
    unsafe { peek_c(iter, |iter, paths| iter.peek_prev_min_in(paths, end)) }
}

/**
 * bch2_btree_iter_peek_slot() - the key at the iterator's position, or a hole
 * there - a deleted key - if there's none
 * @iter:	iterator to peek from
 *
 * Returns:	key if found, or an error extractable with bkey_err().
 */
/// # Safety
/// As bch2_set_btree_iter_dontneed()'s.
#[c_export]
pub unsafe fn bch2_btree_iter_peek_slot(iter: *mut c::btree_iter) -> c::bkey_s_c {
    unsafe { peek_c(iter, |iter, paths| iter.peek_slot_in(paths)) }
}

/**
 * bch2_btree_iter_peek_slot_max() - bch2_btree_iter_peek_slot() with an end:
 * for extents, a hole ends at @end at the latest, and with BTREE_ITER_trim the
 * key is cut to [iter->pos, @end)
 * @iter:	iterator to peek from
 * @end:	the end of the window, past the iterator's position
 *
 * Returns:	key if found, or an error extractable with bkey_err().
 */
/// # Safety
/// As bch2_set_btree_iter_dontneed()'s.
#[c_export]
pub unsafe fn bch2_btree_iter_peek_slot_max(iter: *mut c::btree_iter, end: Bpos) -> c::bkey_s_c {
    unsafe { peek_c(iter, |iter, paths| iter.peek_slot_max_in(paths, end)) }
}

/// # Safety
/// @iter is an initialized iterator, or zeroed, C's for the call.
#[c_export]
pub unsafe fn bch2_btree_iter_peek_root(trans: &BtreeTrans<'_>, #[lent(trans)] paths: &mut Paths<'_>,
                                        iter: *mut c::btree_iter, btree: Btree, level: c_uint) -> c::bkey_s_c {
    match unsafe { (*iter.cast::<BtreeIter<'static>>()).peek_root_in(paths, trans, btree, level) } {
        Ok(Some(k)) => BkeySC::from(unsafe { k.as_ref() }).into(),
        Ok(None)    => c::bkey_s_c::default(),
        Err(e)      => c::bkey_s_c { k: err_ptr(e), v: ptr::null() },
    }
}

/// # Safety
/// @iter is an initialized iterator, or one exit()ed, or zeroed.
#[c_export]
pub unsafe fn bch2_trans_iter_exit(iter: *mut c::btree_iter) {
    unsafe {
        let iter = &mut *iter.cast::<BtreeIter<'static>>();
        if iter.holds_paths() {
            iter.exit_in(&mut Paths::lent(&*iter.trans));
        } else {
            iter.trans = ptr::null_mut();
        }
    }
}

/// # Safety
/// @iter is valid for writes, and holds no paths.
#[c_export]
pub unsafe fn bch2_trans_iter_init_outlined(trans: &BtreeTrans<'_>, #[lent(trans)] paths: &mut Paths<'_>,
                                            iter: *mut c::btree_iter, btree_id: Btree, pos: Bpos,
                                            flags: c::btree_iter_update_trigger_flags, ip: c_ulong) {
    let flags = BtreeIterFlags::from_bits_retain(flags.bits());
    unsafe { iter.cast::<BtreeIter<'_>>().write(BtreeIter::init_in(trans, paths, btree_id, pos, flags, ip)) }
}

/*
 * Check iterator parameters from an untrusted source (e.g. an ioctl) against
 * the preconditions the iterator code enforces with assertions. Kernel
 * callers know these rules; user-controlled (btree, level, range, flags)
 * combinations must be validated first:
 */
pub fn bch2_btree_iter_params_valid(btree: Btree, level: c_uint, start: Bpos, end: Bpos,
                                    flags: c::btree_iter_update_trigger_flags) -> bool {
    btree_iter_params_valid(btree, level, start, end, BtreeIterFlags::from_bits_retain(flags.bits()))
}

/*
 * Low-level iterator init: takes level and flags exactly as given - no
 * defaulting of not_extents/snapshot_field/all_snapshots (the node iterator
 * conventions). For callers that need per-level iteration with caller-chosen
 * snapshot semantics, e.g. the query_btree_keys ioctl; validate untrusted
 * parameters with bch2_btree_iter_params_valid() first:
 */
/// # Safety
/// As bch2_trans_iter_init_outlined()'s.
#[c_export]
#[allow(clippy::too_many_arguments)]
pub unsafe fn __bch2_trans_iter_init_ll(trans: &BtreeTrans<'_>, #[lent(trans)] paths: &mut Paths<'_>,
                                        iter: *mut c::btree_iter, btree_id: Btree, pos: Bpos,
                                        locks_want: c_uint, depth: c_uint,
                                        flags: c::btree_iter_update_trigger_flags, ip: c_ulong) {
    let flags = BtreeIterFlags::from_bits_retain(flags.bits());
    unsafe {
        iter.cast::<BtreeIter<'_>>()
            .write(BtreeIter::init_ll_in(trans, paths, btree_id, pos, locks_want, depth, flags, ip))
    }
}

/// # Safety
/// As bch2_trans_iter_init_outlined()'s.
#[c_export]
pub unsafe fn __bch2_trans_node_iter_init(trans: &BtreeTrans<'_>, #[lent(trans)] paths: &mut Paths<'_>,
                                          iter: *mut c::btree_iter, btree_id: Btree, pos: Bpos,
                                          locks_want: c_uint, depth: c_uint,
                                          flags: c::btree_iter_update_trigger_flags) {
    let flags = BtreeIterFlags::from_bits_retain(flags.bits());
    unsafe {
        iter.cast::<BtreeIter<'_>>()
            .write(BtreeIter::node_iter_init_in(trans, paths, btree_id, pos, locks_want, depth, flags,
                                                crate::ret_ip!()))
    }
}

/// # Safety
/// @dst is valid for writes, and holds no paths; @src is an initialized
/// iterator.
#[c_export]
pub unsafe fn bch2_trans_copy_iter(dst: *mut c::btree_iter, src: *mut c::btree_iter) {
    unsafe {
        let src = &*src.cast::<BtreeIter<'static>>();
        let copy = src.copy_in(&mut Paths::lent(&*src.trans));
        dst.cast::<BtreeIter<'static>>().write(copy);
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
    t:       &'k TransAttempt<'_, 't>,
    mut iters: [&'k mut BtreeIter<'t>; N],
    to_walk:   impl Fn(usize, &BtreeIter<'t>) -> Bpos,
) -> Result<(Bpos, [(&'k BtreeIter<'t>, Option<BkeySC<'k>>); N]), BchError>
{
    // Stop at the first error: it may be a restart, and nothing may peek in a
    // transaction that's restarting
    let mut found = [None; N];
    {
        let mut paths = t.trans().paths();
        for (iter, k) in iters.iter_mut().zip(&mut found) {
            *k = iter.peek_max_in(&mut paths, SPOS_MAX)?;
        }
    }
    let iters = iters.map(|iter| -> &'k BtreeIter<'t> { iter });

    let pos: [Bpos; N] = core::array::from_fn(|i| to_walk(i, iters[i]));
    let at = pos.iter().copied().min().expect("an iterator to walk");

    Ok((at, core::array::from_fn(|i| (iters[i], if at < pos[i] { None } else { iters[i].key_of(found[i]) }))))
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
        let trans = t.trans();
        let k = self.peek_root_in(&mut trans.paths(), trans, btree, level)?;
        // SAFETY: the key, in the iterator's path, for as long as it's borrowed
        Ok(k.map(|k| (&*self, BkeySC::from(unsafe { k.as_ref() }))))
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
        let KeyVal(v) = paths[self.path].peek_slot(u).expect("a slot always has a key");
        unsafe { bkey_s_c(u, v) }
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
        let trans = self.trans_c();
        self.copy_in(&mut trans.paths())
    }

    pub fn new(
        trans: impl TransRef<'t>,
        btree: impl Into<u32>,
        pos: Bpos,
        flags: BtreeIterFlags,
    ) -> BtreeIter<'t> {
        let trans = unsafe { &*trans.trans_raw() };

        BtreeIter::init_in(trans, &mut trans.paths(), Btree::from_raw(btree.into()).expect("invalid btree id"),
                           pos, flags, 0)
    }

    pub fn new_level(
        trans: impl TransRef<'t>,
        btree: impl Into<u32>,
        pos: Bpos,
        level: u32,
        flags: BtreeIterFlags,
    ) -> BtreeIter<'t> {
        let trans = unsafe { &*trans.trans_raw() };

        BtreeIter::node_iter_init_in(trans, &mut trans.paths(), Btree::from_raw(btree.into()).expect("invalid btree id"),
                                     pos, 0, level, flags, crate::ret_ip!())
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

    /// Peek with @peek, under attempt @t.
    fn peek_in<'k>(&'k mut self, t: &'k TransAttempt<'_, 't>,
                   peek: impl FnOnce(&mut Self, &mut Paths<'_>) -> Peeked)
        -> Result<Option<BkeySC<'k>>, BchError>
    {
        let k = peek(self, &mut t.trans().paths())?;
        Ok(self.key_of(k))
    }

    pub fn peek_max<'k>(&'k mut self, t: &'k TransAttempt<'_, 't>, end: Bpos)
        -> Result<Option<BkeySC<'k>>, BchError>
    {
        self.peek_in(t, |iter, paths| iter.peek_max_in(paths, end))
    }

    /// At the iterator's level - new_level()'s; None if the btree isn't that deep.
    pub fn peek_node<'k>(&'k mut self, t: &'k TransAttempt<'_, 't>)
        -> Result<Option<LockedNode<'k, BtreeNode>>, BchError>
    {
        self.peek_node_in(&mut t.trans().paths())
    }

    /// The key at the iterator's position, or a deleted key for a hole.
    pub fn peek_slot<'k>(&'k mut self, t: &'k TransAttempt<'_, 't>)
        -> Result<Option<BkeySC<'k>>, BchError>
    {
        self.peek_in(t, |iter, paths| iter.peek_slot_in(paths))
    }

    /// peek_slot() with an end, past the position: for extents a hole ends
    /// there at the latest, and with TRIM the key is cut to [position, @end).
    pub fn peek_slot_max<'k>(&'k mut self, t: &'k TransAttempt<'_, 't>, end: Bpos)
        -> Result<Option<BkeySC<'k>>, BchError>
    {
        self.peek_in(t, |iter, paths| iter.peek_slot_max_in(paths, end))
    }

    /// Back one position, and the key there, or a deleted key for a hole -
    /// None if there's nowhere further back to go.
    pub fn prev_slot<'k>(&'k mut self, t: &'k TransAttempt<'_, 't>)
        -> Result<Option<BkeySC<'k>>, BchError>
    {
        self.peek_in(t, |iter, paths| iter.prev_slot_in(paths))
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
    pub fn peek_slot_shared<'k>(&'k mut self, t: &'k TransAttempt<'_, 't>)
        -> Result<(&'k Self, BkeySC<'k>), BchError>
    {
        let (this, k) = self.peek_slot_with(t)?.expect("a slot always has a key");
        Ok((this, k))
    }

    /// As peek_slot_typed(), for a key to update through this iterator: the
    /// iterator comes back shared, for as long as the key is used - as the
    /// for_each loops pass it - so the key can't be moved out from under.
    pub fn peek_slot_typed_shared<'k, K: BkeyFamily + ?Sized>(&'k mut self, t: &'k TransAttempt<'_, 't>)
        -> Result<(&'k Self, BkeySC<'k, K::Validated>), BchError>
    {
        let (this, k) = self.peek_slot_shared(t)?;
        Ok((this, bkey_arm::<K>(t.fs(), k)?))
    }

    pub fn peek_max_type<'k>(&'k mut self, t: &'k TransAttempt<'_, 't>, end: Bpos,
                                     flags: BtreeIterFlags)
        -> Result<Option<BkeySC<'k>>, BchError>
    {
        self.peek_in(t, |iter, paths| iter.peek_max_type_in(paths, end, flags))
    }

    /// Peek with @peek, under attempt @t: the key, and the iterator it
    /// borrows, shared - both from the one &mut borrow, so neither outlives
    /// it and nothing moves the iterator while they're in use.
    pub(super) fn peek_with<'k>(&'k mut self, t: &'k TransAttempt<'_, 't>,
                                peek: impl FnOnce(&mut Self, &mut Paths<'_>) -> Peeked)
        -> Result<Option<(&'k BtreeIter<'t>, BkeySC<'k>)>, BchError>
    {
        let k = peek(self, &mut t.trans().paths())?;
        let this = &*self;
        Ok(this.key_of(k).map(|k| (this, k)))
    }

    /// peek_slot(), and the iterator with the key, shared: for an update at
    /// the key's position.
    pub fn peek_slot_with<'k>(&'k mut self, t: &'k TransAttempt<'_, 't>)
        -> Result<Option<(&'k BtreeIter<'t>, BkeySC<'k>)>, BchError>
    {
        self.peek_with(t, |iter, paths| iter.peek_slot_in(paths))
    }

    pub fn peek<'k>(&'k mut self, t: &'k TransAttempt<'_, 't>)
        -> Result<Option<BkeySC<'k>>, BchError>
    {
        self.peek_max(t, SPOS_MAX)
    }

    pub fn peek_prev_min<'k>(&'k mut self, t: &'k TransAttempt<'_, 't>, min: Bpos)
        -> Result<Option<BkeySC<'k>>, BchError>
    {
        self.peek_in(t, |iter, paths| iter.peek_prev_min_in(paths, min))
    }

    pub fn peek_prev<'k>(&'k mut self, t: &'k TransAttempt<'_, 't>)
        -> Result<Option<BkeySC<'k>>, BchError>
    {
        self.peek_prev_min(t, Bpos { inode: 0, offset: 0, snapshot: 0 })
    }

    pub fn traverse(&mut self, t: &TransAttempt<'_, 't>) -> Result<(), BchError> {
        self.traverse_in(&mut t.trans().paths())
    }

    /// traverse(), its path at the iterator's position, not its search key -
    /// one past it, for extents.
    pub fn traverse_at_pos(&mut self, t: &TransAttempt<'_, 't>) -> Result<(), BchError> {
        self.traverse_at_pos_in(&mut t.trans().paths())
    }

    /// Step past the key just returned: false at the end of the btree.
    pub fn advance(&mut self) -> bool {
        let trans = self.trans_c();
        self.advance_in(&mut trans.paths())
    }

    /// Step back past the key just returned, for walking backwards: false at
    /// the start of the btree.
    pub fn rewind(&mut self) -> bool {
        let trans = self.trans_c();
        self.rewind_in(&mut trans.paths())
    }

    /// @f, under the write lock of the key cache entry at the iterator's
    /// position, if there is one: cycling it advances the entry's lock
    /// sequence, so the relock of anything that decided on the key fails.
    /// The iterator holds its leaf intent locked, so an entry isn't created
    /// meanwhile - the fill write locks the leaf.
    pub fn with_key_cache_write_lock<R>(&self, t: &TransAttempt<'_, '_>, f: impl FnOnce() -> R)
        -> Result<R, BchError>
    {
        let trans = t.trans();
        ebug_assert!(ptr::eq(self.trans_c(), trans), "an iterator of another transaction");
        assert!(trans.paths()[self.path].intent_locked(0),
                "cycling the key cache lock at {} without its leaf intent locked", self.pos);

        let mut ck_iter = BtreeIter::new(t, self.btree(), self.pos,
                                         BtreeIterFlags::INTENT | BtreeIterFlags::CACHED |
                                         BtreeIterFlags::CACHED_NOFILL);
        ck_iter.traverse(t)?;

        let mut paths = trans.paths();
        if paths[ck_iter.path].unlocked_node(0).is_none() {
            return Ok(f());
        }

        trans.node_lock_write(&mut paths, ck_iter.path, 0)?;
        let ret = f();
        trans.node_unlock_write(&mut paths, ck_iter.path, 0);
        Ok(ret)
    }
}

impl<'t> Drop for BtreeIter<'t> {
    fn drop(&mut self) {
        if self.holds_paths() {
            let trans = self.trans_c();
            self.exit_in(&mut trans.paths());
        }
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
        let trans = unsafe { &*trans.trans_raw() };
        let raw = BtreeIter::node_iter_init_in(trans, &mut trans.paths(),
                                               Btree::from_raw(btree.into()).expect("invalid btree id"),
                                               pos, locks_want, depth, flags, crate::ret_ip!());
        BtreeNodeIter { raw }
    }

    pub fn peek(&mut self) -> Result<Option<LockedNode<'_, BtreeNode>>, BchError> {
        let trans = self.raw.trans_c();
        self.raw.peek_node_in(&mut trans.paths())
    }

    /// peek(), the node held to update its key: btree_node_update_key()
    /// takes it, with iter().
    pub(crate) fn peek_ref(&mut self) -> Result<Option<BtreeNodeRef>, BchError> {
        Ok(self.peek()?.map(BtreeNodeRef::from))
    }

    pub(crate) fn iter(&mut self) -> &mut BtreeIter<'t> {
        &mut self.raw
    }

    pub fn peek_max_type<'i>(
        &'i mut self,
        end: Bpos,
        flags: BtreeIterFlags,
    ) -> Result<Option<BkeySC<'i>>, BchError> {
        let trans = self.raw.trans_c();
        let k = self.raw.peek_max_type_in(&mut trans.paths(), end, flags)?;
        Ok(self.raw.key_of(k))
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
