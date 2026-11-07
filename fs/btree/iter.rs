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
use crate::btree::path::{btree_path_level, BtreePath, LockedNode, PathIdx, Paths};
use crate::c;
use crate::data::extents::{bch2_cut_back_s, bch2_cut_front_s};
use crate::debug::params::debug_check_iterators;
use crate::debug::trace::events;
use crate::errcode::{BchError, bch_errcode, err_ptr, errptr_or_null, ret_to_c};
use crate::fs::Fs;
use crate::prt;
use crate::snapshots::snapshot::is_ancestor;
use crate::util::{max_of, Printbuf};
use crate::SPOS_MAX;
use bitflags::bitflags;
use core::cell::Cell;
use core::ffi::{c_ulong, c_int, c_uint};
use core::marker::PhantomData;
use core::mem::MaybeUninit;
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
        unsafe { c::bch2_set_btree_iter_dontneed(self.raw_mut()) }
    }

    /// Move to @pos - in the iterator's own snapshot, unless it's
    /// all_snapshots - with no key there yet: the next peek finds it. Drops the
    /// update path, which was for the old position. The static inline
    /// bch2_btree_iter_set_pos().
    pub fn set_pos(&mut self, pos: Bpos) {
        if self.update_path != 0 {
            let trans = unsafe { &*self.trans };
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
            let trans = unsafe { &*self.trans };
            trans.path_put(paths, self.update_path, self.intent());
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

fn bkey_s_c_null() -> c::bkey_s_c {
    c::bkey_s_c { k: ptr::null(), v: ptr::null() }
}

fn bkey_s_c_err(e: BchError) -> c::bkey_s_c {
    c::bkey_s_c { k: err_ptr(e), v: ptr::null() }
}

fn bkey_err(k: c::bkey_s_c) -> Result<(), BchError> {
    errptr_or_null(k.k.cast_mut()).map(|_| ())
}

fn bkey_i_to_s_c(k: *const c::bkey_i) -> c::bkey_s_c {
    unsafe { c::bkey_s_c { k: &raw const (*k).k, v: &raw const (*k).v } }
}

/// The header of @k, a key a peek returned: not an error, and not NULL.
fn key_of(k: c::bkey_s_c) -> c::bkey {
    assert!(!k.k.is_null());
    unsafe { *k.k }
}

#[cold]
#[inline(never)]
fn peek_trace(out: &mut Printbuf, fs: &Fs, k: c::bkey_s_c) {
    match bkey_err(k) {
        Err(e)                   => prt!(out, "{}", e),
        Ok(()) if !k.k.is_null() => bkey_val_to_text(out, Some(fs), unsafe { BkeySC::from_c(&k) }),
        Ok(())                   => prt!(out, "(null)"),
    }
}

#[derive(PartialEq, Eq)]
enum FilterSnapshots {
    /* accept *k, exit caller's loop */
    Ok,
    /* search_key updated, retry */
    Continue,
    /* out of range, terminate scan */
    End,
    /* *k set to err, exit cleanly */
    Err,
}

fn btree_id_cached(btree: Btree) -> bool {
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

    /* Debug: */

    fn verify_now(&self, paths: &mut Paths<'_>) {
        let trans = self.trans_c();
        let flags = self.iter_flags();

        assert!(flags.contains(BtreeIterFlags::CACHED) == paths[self.path].cached());

        assert!(!flags.contains(BtreeIterFlags::IS_EXTENTS | BtreeIterFlags::ALL_SNAPSHOTS));

        assert!(!(!flags.contains(BtreeIterFlags::SNAPSHOT_FIELD) &&
                  flags.contains(BtreeIterFlags::ALL_SNAPSHOTS) &&
                  !self.btree().has_snapshot_field()));

        if self.update_path != 0 {
            trans.path_verify_now(paths, self.update_path);
        }
        trans.path_verify_now(paths, self.path);
    }

    fn verify_entry_exit_now(&self) {
        let flags = self.iter_flags();

        assert!(!(flags.contains(BtreeIterFlags::FILTER_SNAPSHOTS) && self.pos.snapshot == 0));

        assert!(!(!flags.contains(BtreeIterFlags::ALL_SNAPSHOTS) && self.pos.snapshot != self.snapshot));

        assert!(!if flags.contains(BtreeIterFlags::ALL_SNAPSHOTS) {
            self.pos != self.k.p
        } else if !flags.contains(BtreeIterFlags::IS_EXTENTS) {
            !bkey_eq(self.pos, self.k.p)
        } else {
            bkey_lt(self.pos, self.k.start_pos()) || bkey_gt(self.pos, self.k.p)
        });
    }

    fn verify_ret_now(&self, paths: &mut Paths<'_>, k: c::bkey_s_c) -> Result<(), BchError> {
        let trans = self.trans_c();

        if !self.iter_flags().contains(BtreeIterFlags::FILTER_SNAPSHOTS) {
            return Ok(());
        }

        if bkey_err(k).is_err() || k.k.is_null() {
            return Ok(());
        }
        let k = key_of(k);
        let t = unsafe { trans.attempt_in_progress() };

        assert!(is_ancestor(&t, self.snapshot, k.p.snapshot));

        let mut copy = BtreeIter::init_in(trans, paths, self.btree(), self.pos,
                                          BtreeIterFlags::NOPRESERVE | BtreeIterFlags::ALL_SNAPSHOTS,
                                          crate::ret_ip!());
        let prev = copy.prev_in(paths);
        let ret = bkey_err(prev).map(|()| {
            if prev.k.is_null() {
                return;
            }
            let prev = key_of(prev);

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

    fn verify_ret(&self, paths: &mut Paths<'_>, k: c::bkey_s_c) -> Result<(), BchError> {
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

    pub(super) fn peek_node_in(&mut self, paths: &mut Paths<'_>) -> *mut BtreeNode {
        let trans = self.trans_c();

        ebug_assert!(!paths[self.path].cached());
        self.verify(paths);

        let b: *mut BtreeNode = match self.traverse_in(paths) {
            Err(e) => err_ptr(e),
            Ok(()) => {
                let path = &paths[self.path];
                path.l[path.level() as usize].b.as_ptr().cast()
            }
        };

        if let Ok(Some(n)) = errptr_or_null(b) {
            let n = unsafe { LockedNode::new(n.as_ref()) };
            let max_key = n.contents().key_i().k.p;
            assert!(!(max_key < self.pos));

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

    fn updates(&self) -> impl Iterator<Item = &c::btree_insert_entry> + '_ {
        let trans = self.trans_c();
        (0..usize::from(trans.nr_updates.get())).map(move |i| unsafe { &*trans.updates.get().add(i) })
    }

    #[inline(never)]
    fn trans_peek_prev_updates(&mut self, paths: &Paths<'_>, search_key: Bpos, k: &mut c::bkey_s_c) {
        let path = &paths[self.path];
        let b = path.locked_btree_node(path.level() as usize).expect("a path peeked is locked");
        let end = b.contents().data().min_key;
        let btree = self.btree();
        let mut found = None;

        for i in self.updates() {
            let p = unsafe { (*i.k).k.p };
            let from = if !k.k.is_null() { key_of(*k).p } else { end };

            if !i.bits.key_cache_already_flushed() && i.bits.btree_id() == btree &&
               p <= search_key && p >= from {
                found = Some(i.k);
                *k = bkey_i_to_s_c(i.k);
            }
        }
        if let Some(u) = found {
            self.k = unsafe { (*u).k };
        }
    }

    #[inline(never)]
    fn trans_peek_updates(&mut self, paths: &Paths<'_>, search_key: Bpos, k: &mut c::bkey_s_c) {
        let path = &paths[self.path];
        let b = path.locked_btree_node(path.level() as usize).expect("a path peeked is locked");
        let end = b.contents().key_i().k.p;
        let btree = self.btree();
        let level = u32::from(self.min_depth);
        let mut found = None;

        for i in self.updates() {
            let p = unsafe { (*i.k).k.p };
            let to = if !k.k.is_null() { key_of(*k).p } else { end };

            if !i.bits.key_cache_already_flushed() && i.bits.btree_id() == btree &&
               u32::from(i.bits.level()) == level &&
               p >= search_key && p <= to {
                found = Some(i.k);
                *k = bkey_i_to_s_c(i.k);
            }
        }
        if let Some(u) = found {
            self.k = unsafe { (*u).k };
        }
    }

    #[inline(never)]
    fn trans_peek_slot_updates(&mut self, k: &mut c::bkey_s_c) {
        let btree = self.btree();
        let level = u32::from(self.min_depth);
        let pos = self.pos;
        let mut found = None;

        for i in self.updates() {
            if !i.bits.key_cache_already_flushed() && i.bits.btree_id() == btree &&
               u32::from(i.bits.level()) == level &&
               unsafe { (*i.k).k.p } == pos {
                found = Some(i.k);
                *k = bkey_i_to_s_c(i.k);
            }
        }
        if let Some(u) = found {
            self.k = unsafe { (*u).k };
        }
    }

    fn journal_peek(&mut self, paths: &Paths<'_>, search_pos: Bpos, end_pos: Bpos) -> *const c::bkey_i {
        let trans = self.trans_c();

        /*
         * BTREE_ITER_with_journal is the iterator's from its init; trans_begin()
         * puts the transaction's ref on the journal keys once replay is done,
         * and can't clear the flag on iterators it can't see - so the
         * transaction's ref, not the iterator's flag, says the keys are there:
         */
        if !trans.journal_replay_not_finished() {
            return ptr::null();
        }

        let level = u32::from(paths[self.path].level());

        journal_overlay::peek_max(unsafe { trans.fs().journal_keys_held() }, self.btree(), level,
                                  search_pos, end_pos, &mut self.journal_idx)
    }

    #[inline(never)]
    fn trans_peek_slot_journal(&mut self, paths: &Paths<'_>) -> c::bkey_s_c {
        let pos = paths[self.path].pos;
        let k = self.journal_peek(paths, pos, pos);

        if !k.is_null() {
            self.k = unsafe { (*k).k };
            bkey_i_to_s_c(k)
        } else {
            bkey_s_c_null()
        }
    }

    #[inline(never)]
    fn trans_peek_journal(&mut self, paths: &Paths<'_>, search_key: Bpos, k: &mut c::bkey_s_c) {
        let end = if !k.k.is_null() {
            key_of(*k).p
        } else {
            let path = &paths[self.path];
            path.locked_btree_node(path.level() as usize).expect("a path peeked is locked").contents().key_i().k.p
        };
        let next_journal = self.journal_peek(paths, search_key, end);

        if !next_journal.is_null() {
            self.k = unsafe { (*next_journal).k };
            *k = bkey_i_to_s_c(next_journal);
        }
    }

    fn journal_peek_prev(&mut self, paths: &Paths<'_>, search_key: Bpos, end_pos: Bpos) -> *const c::bkey_i {
        let trans = self.trans_c();

        if !trans.journal_replay_not_finished() {
            return ptr::null();
        }

        let level = u32::from(paths[self.path].level());

        journal_overlay::peek_prev_min(unsafe { trans.fs().journal_keys_held() }, self.btree(), level,
                                       search_key, end_pos, &mut self.journal_idx)
    }

    #[inline(never)]
    fn trans_peek_prev_journal(&mut self, paths: &Paths<'_>, search_key: Bpos, k: &mut c::bkey_s_c) {
        let end = if !k.k.is_null() {
            key_of(*k).p
        } else {
            let path = &paths[self.path];
            path.locked_btree_node(path.level() as usize).expect("a path peeked is locked")
                .contents().data().min_key
        };
        let next_journal = self.journal_peek_prev(paths, search_key, end);

        if !next_journal.is_null() {
            self.k = unsafe { (*next_journal).k };
            *k = bkey_i_to_s_c(next_journal);
        }
    }

    /*
     * Checks btree key cache for key at iter->pos and returns it if present, or
     * bkey_s_c_null:
     */
    #[inline(never)]
    fn trans_peek_key_cache(&mut self, paths: &mut Paths<'_>, k: &mut c::bkey_s_c) -> bool {
        if k.k.is_null() || key_of(*k).key_type().is_deleted() {
            return false;
        }
        let kp = key_of(*k).p;

        let trans = self.trans_c();
        let flags = self.iter_flags();

        trans.verify_not_unlocked_or_in_restart();

        if flags.contains(BtreeIterFlags::KEY_CACHE_FILL) && self.pos == kp {
            return false;
        }

        if unsafe { bch2_btree_key_cache_find(trans.fs().as_ptr(), self.btree(), kp) }.is_null() {
            return false;
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
            *k = bkey_s_c_err(e);
            return true;
        }

        let mut u = c::bkey::new();
        let k2 = paths[self.key_cache_path.get()].peek_slot(&mut u);
        if k2.k.is_null() {
            return false;
        }

        if flags.contains(BtreeIterFlags::ALL_SNAPSHOTS) && kp != key_of(k2).p {
            return false;
        }

        self.k = u;
        k.v = k2.v;
        trans.path_set_should_be_locked(paths, self.key_cache_path.get());
        false
    }

    fn __peek(&mut self, paths: &mut Paths<'_>, search_key: &mut Bpos, end: Bpos) -> c::bkey_s_c {
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
                break bkey_s_c_err(e);
            }

            let level = paths[self.path].level() as usize;
            if paths[self.path].l[level].b.is_null() {
                /* No btree nodes at requested level: */
                self.set_pos_in(paths, SPOS_MAX);
                break bkey_s_c_null();
            }

            trans.path_set_should_be_locked(paths, self.path);

            let mut k = paths[self.path].level_peek_all(&mut self.k);

            if flags.contains(BtreeIterFlags::KEY_CACHED) && self.trans_peek_key_cache(paths, &mut k) {
                break k;
            }

            if flags.contains(BtreeIterFlags::WITH_JOURNAL) {
                self.trans_peek_journal(paths, *search_key, &mut k);
            }

            if trans.nr_updates.get() != 0 && !flags.contains(BtreeIterFlags::COMMITTED) {
                self.trans_peek_updates(paths, *search_key, &mut k);
            }

            if !k.k.is_null() {
                let kk = key_of(k);
                if !kk.key_type().is_deleted() {
                    break k;
                }

                /*
                 * If we've got a whiteout, and it's after the search
                 * key, advance the search key to the whiteout instead
                 * of just after the whiteout - it might be a btree
                 * whiteout, with a real key at the same position, since
                 * in the btree deleted keys sort before non deleted.
                 */
                *search_key = if *search_key != kk.p { kk.p } else { kk.p.successor() };

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
                    break bkey_s_c_null();
                }
            }
        };

        self.verify(paths);

        let fs = trans.fs();
        fs.event_trace(events::__btree_iter_peek, |out| peek_trace(out, fs, k));

        k
    }

    #[inline(never)]
    fn filter_snapshots(&mut self, paths: &mut Paths<'_>, k: &mut c::bkey_s_c, search_key: &mut Bpos,
                        end: Bpos) -> FilterSnapshots {
        let trans = self.trans_c();
        let flags = self.iter_flags();
        let kk = key_of(*k);

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
            return FilterSnapshots::End;
        }

        /*
         * If we want an update path we want to do this up front; also more
         * efficient than letting !bch2_snapshot_is_ancestor() skip these:
         */
        if kk.p.snapshot < self.snapshot {
            *search_key = Bpos { snapshot: self.snapshot, ..kk.p };
            return FilterSnapshots::Continue;
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
            if let Err(e) = trans.path_traverse(paths, self.update_path, flags) {
                *k = bkey_s_c_err(e);
                return FilterSnapshots::Err;
            }
        }

        /*
         * We can never have a key in a leaf node at POS_MAX, so we don't have
         * to check these successor() calls:
         */
        if !is_ancestor(&unsafe { trans.attempt_in_progress() }, self.snapshot, kk.p.snapshot) {
            *search_key = kk.p.successor();
            return FilterSnapshots::Continue;
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
                return FilterSnapshots::End;
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
            return FilterSnapshots::Continue;
        }

        FilterSnapshots::Ok
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
    fn trim(&mut self, paths: &mut Paths<'_>, k: c::bkey_s_c, start: Bpos, end: Bpos) -> c::bkey_s_c {
        let kk = key_of(k);

        if bkey_le(start, kk.start_pos()) && bkey_le(kk.p, end) {
            return k;
        }

        if kk.val_bytes() == 0 {
            let mut u = kk;

            if bkey_lt(u.start_pos(), start) {
                u.size = (u.p.offset - start.offset) as u32;
            }
            if bkey_gt(u.p, end) {
                u.resize((end.offset - u.start_pos().offset) as u32);
            }
            self.k = u;
            return c::bkey_s_c { k: &raw const self.k, v: k.v };
        }

        let trans = self.trans_c();
        let t = match trans.kmalloc_in(paths, usize::from(kk.u64s) * size_of::<u64>()) {
            Ok(t)  => t.cast::<c::bkey_i>(),
            Err(e) => return bkey_s_c_err(e),
        };

        let t = unsafe {
            (*t).k = kk;
            ptr::copy_nonoverlapping(k.v.cast::<u8>(), (&raw mut (*t).v).cast::<u8>(), kk.val_bytes());
            &mut *BkeyI::from_raw_mut(t)
        };
        bch2_cut_front_s(trans.fs(), start, BkeyS::from(&mut *t));
        bch2_cut_back_s(end, BkeyS::from(&mut *t));
        self.k = t.k;
        bkey_i_to_s_c(t.as_ffi())
    }

    fn maybe_trim(&mut self, paths: &mut Paths<'_>, k: c::bkey_s_c, start: Bpos, end: Bpos) -> c::bkey_s_c {
        if !self.trims() || k.k.is_null() || bkey_err(k).is_err() {
            return k;
        }

        let k = self.trim(paths, k, start, end);
        /* what trimming promises: all of it in the window, and something */
        ebug_assert!(bkey_err(k).is_err() || {
            let kk = key_of(k);
            kk.size != 0 && !bkey_lt(kk.start_pos(), start) && !bkey_gt(kk.p, end)
        });
        k
    }

    pub(super) fn peek_max_in(&mut self, paths: &mut Paths<'_>, end: Bpos) -> c::bkey_s_c {
        let trans = self.trans_c();
        let flags = self.iter_flags();
        let mut search_key = self.search_key();
        let mut k;

        trans.verify_not_unlocked_or_in_restart();
        self.verify_entry_exit();
        ebug_assert!(!(flags.contains(BtreeIterFlags::FILTER_SNAPSHOTS) &&
                       !flags.contains(BtreeIterFlags::NOFILTER_WHITEOUTS) &&
                       bkey_eq(end, POS_MAX)));

        'out_no_locked: {
            if let Err(e) = trans.maybe_inject_restart(crate::ret_ip!()) {
                k = bkey_s_c_err(e);
                break 'out_no_locked;
            }

            if self.update_path != 0 {
                trans.path_put(paths, self.update_path, self.intent());
                self.update_path = 0;
            }

            'end: {
                loop {
                    k = self.__peek(paths, &mut search_key, end);
                    if k.k.is_null() {
                        break 'end;
                    }
                    if bkey_err(k).is_err() {
                        break 'out_no_locked;
                    }

                    if flags.contains(BtreeIterFlags::FILTER_SNAPSHOTS) && {
                        let kk = key_of(k);
                        kk.p.snapshot != self.snapshot || kk.key_type().is_extent_whiteout() ||
                            self.update_path != 0
                    } {
                        match self.filter_snapshots(paths, &mut k, &mut search_key, end) {
                            FilterSnapshots::Ok       => {}
                            FilterSnapshots::Continue => continue,
                            FilterSnapshots::End      => break 'end,
                            FilterSnapshots::Err      => break 'out_no_locked,
                        }
                    }

                    break;
                }

                let kk = key_of(k);

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
                    break 'end;
                }

                self.path = trans.path_set_pos(paths, self.path, kk.p, self.intent(), self.ip_allocated());

                trans.path_set_should_be_locked(paths, self.path);
                break 'out_no_locked;
            }

            self.set_pos_in(paths, end);
            k = bkey_s_c_null();
        }

        if self.update_path != 0 {
            match trans.path_relock(paths, self.update_path) {
                Err(e) => k = bkey_s_c_err(e),
                Ok(()) => trans.path_set_should_be_locked(paths, self.update_path),
            }
        }

        if let Err(e) = self.verify_ret(paths, k) {
            self.set_pos_in(paths, self.pos);
            k = bkey_s_c_err(e);
        }

        self.verify_entry_exit();

        let k = self.maybe_trim(paths, k, self.pos, end);

        let fs = trans.fs();
        fs.event_trace(events::btree_iter_peek_max, |out| peek_trace(out, fs, k));

        k
    }

    pub(super) fn next_in(&mut self, paths: &mut Paths<'_>) -> c::bkey_s_c {
        if !self.advance_in(paths) {
            return bkey_s_c_null();
        }

        self.peek_max_in(paths, SPOS_MAX)
    }

    fn __peek_prev(&mut self, paths: &mut Paths<'_>, mut search_key: Bpos) -> c::bkey_s_c {
        let trans = self.trans_c();
        let flags = self.iter_flags();

        self.verify(paths);

        let k = loop {
            self.path = trans.path_set_pos(paths, self.path, search_key, self.intent(), self.ip_allocated());

            if let Err(e) = trans.path_traverse(paths, self.path, flags) {
                /* ensure that iter->k is consistent with iter->pos: */
                self.set_pos_in(paths, self.pos);
                break bkey_s_c_err(e);
            }

            let level = paths[self.path].level() as usize;
            if paths[self.path].l[level].b.is_null() {
                /* No btree nodes at requested level: */
                self.set_pos_in(paths, SPOS_MAX);
                break bkey_s_c_null();
            }
            self.path = trans.path_make_mut(paths, self.path, self.intent(), self.ip_allocated());

            trans.path_set_should_be_locked(paths, self.path);

            let mut k = paths[self.path].level_peek_all(&mut self.k);

            if k.k.is_null() || key_of(k).p > search_key {
                k = trans.path_level_prev(paths, self.path, &mut self.k);

                ebug_assert!(k.k.is_null() || !(key_of(k).p > search_key));
            }

            if flags.contains(BtreeIterFlags::KEY_CACHED) && self.trans_peek_key_cache(paths, &mut k) {
                break k;
            }

            if flags.contains(BtreeIterFlags::WITH_JOURNAL) {
                self.trans_peek_prev_journal(paths, search_key, &mut k);
            }

            if trans.nr_updates.get() != 0 && !flags.contains(BtreeIterFlags::COMMITTED) {
                self.trans_peek_prev_updates(paths, search_key, &mut k);
            }

            if !k.k.is_null() && !key_of(k).key_type().is_deleted() {
                break k;
            } else if !k.k.is_null() {
                search_key = key_of(k).p.predecessor();
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
                    break bkey_s_c_null();
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
    fn peek_visible_at(&mut self, paths: &mut Paths<'_>, pos: Bpos) -> c::bkey_s_c {
        let mut iter2 = self.copy_in(paths);
        iter2.clear_flags(BtreeIterFlags::IS_EXTENTS | BtreeIterFlags::NOFILTER_WHITEOUTS);

        iter2.set_pos_in(paths, Bpos { snapshot: self.snapshot, ..pos });

        let mut k = iter2.peek_slot_in(paths);
        let k = 'k: {
            if bkey_err(k).is_err() {
                break 'k k;
            }
            /* A hole, or a whiteout - and it points into iter2: */
            if k.k.is_null() || key_of(k).key_type().is_deleted() {
                break 'k bkey_s_c_null();
            }

            core::mem::swap(&mut self.path, &mut iter2.path);
            self.key_cache_path.swap(&iter2.key_cache_path);
            self.k = iter2.k;
            k.k = &raw const self.k;
            k
        };
        iter2.exit_in(paths);
        k
    }

    pub(super) fn peek_prev_min_in(&mut self, paths: &mut Paths<'_>, end: Bpos) -> c::bkey_s_c {
        let flags = self.iter_flags();

        /* trimming, the window is [@end, iter->pos): nothing in an empty one */
        if self.trims() && bkey_le(self.pos, end) {
            self.set_pos_in(paths, end);
            return bkey_s_c_null();
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
            let k = self.__peek_slot(paths, SPOS_MAX, crate::ret_ip!());
            if bkey_err(k).is_err() {
                return k;
            }

            let kk = key_of(k);
            if !kk.key_type().is_deleted() &&
               (!flags.contains(BtreeIterFlags::IS_EXTENTS) || bkey_lt(kk.start_pos(), self.pos)) {
                return self.maybe_trim(paths, k, end, self.pos);
            }
        }

        let trans = self.trans_c();
        let mut search_key = self.pos;
        let mut k;

        /*
         * Every snapshot's key at this position: an ancestor's sorts after
         * iter->snapshot's, and searching back from iter->pos would skip it -
         * for extents, the one ending here:
         */
        if flags.contains(BtreeIterFlags::FILTER_SNAPSHOTS) {
            search_key.snapshot = u32::MAX;
        }

        trans.verify_not_unlocked_or_in_restart();
        self.verify_entry_exit();
        ebug_assert!(!(flags.contains(BtreeIterFlags::FILTER_SNAPSHOTS) && self.pos.inode != end.inode));

        'out_no_locked: {
            if let Err(e) = trans.maybe_inject_restart(crate::ret_ip!()) {
                k = bkey_s_c_err(e);
                break 'out_no_locked;
            }

            'end: {
                loop {
                    k = self.__peek_prev(paths, search_key);
                    if k.k.is_null() {
                        break 'end;
                    }
                    if bkey_err(k).is_err() {
                        break 'out_no_locked;
                    }

                    if flags.contains(BtreeIterFlags::FILTER_SNAPSHOTS) {
                        let kp = key_of(k).p;

                        /*
                         * We need to check against @end before FILTER_SNAPSHOTS because
                         * if we get to a different inode that requested we might be
                         * seeing keys for a different snapshot tree that will all be
                         * filtered out.
                         */
                        if bkey_lt(kp, end) {
                            break 'end;
                        }

                        if !is_ancestor(&unsafe { trans.attempt_in_progress() }, self.snapshot, kp.snapshot) {
                            search_key = kp.predecessor();
                            continue;
                        }

                        /*
                         * A version visible in iter->snapshot is at this
                         * position: which one we see - if any - is decided
                         * forwards, from iter->snapshot's own up:
                         */
                        let pos = kp;

                        k = self.peek_visible_at(paths, pos);
                        if bkey_err(k).is_err() {
                            /*
                             * iter->k still holds the version
                             * __bch2_btree_iter_peek_prev() found: make it
                             * consistent with iter->pos, as its own error
                             * path does:
                             */
                            self.set_pos_in(paths, self.pos);
                            break 'out_no_locked;
                        }

                        if k.k.is_null() {
                            if !bkey_gt(pos, end) {
                                break 'end;
                            }

                            search_key = self.predecessor(pos);
                            search_key.snapshot = u32::MAX;
                            continue;
                        }
                    }

                    let kk = key_of(k);

                    ebug_assert!(!if flags.contains(BtreeIterFlags::ALL_SNAPSHOTS) {
                        kk.p > self.pos
                    } else if flags.contains(BtreeIterFlags::IS_EXTENTS) {
                        bkey_ge(kk.start_pos(), self.pos)
                    } else {
                        bkey_gt(kk.p, self.pos)
                    });

                    if if flags.contains(BtreeIterFlags::ALL_SNAPSHOTS) {
                        kk.p < end
                    } else if flags.contains(BtreeIterFlags::IS_EXTENTS) {
                        bkey_le(kk.p, end)
                    } else {
                        bkey_lt(kk.p, end)
                    } {
                        break 'end;
                    }

                    break;
                }

                /* Extents can straddle iter->pos: */
                self.pos = core::cmp::min(self.pos, key_of(k).p);

                if flags.contains(BtreeIterFlags::FILTER_SNAPSHOTS) {
                    self.pos.snapshot = self.snapshot;
                }

                k = self.maybe_trim(paths, k, end, self.pos);
                break 'out_no_locked;
            }

            self.set_pos_in(paths, end);
            k = bkey_s_c_null();
        }

        self.verify_entry_exit();
        self.verify(paths);

        let fs = trans.fs();
        fs.event_trace(events::btree_iter_peek_prev_min, |out| peek_trace(out, fs, k));

        k
    }

    pub(super) fn prev_in(&mut self, paths: &mut Paths<'_>) -> c::bkey_s_c {
        if !self.rewind_in(paths) {
            return bkey_s_c_null();
        }

        self.peek_prev_min_in(paths, POS_MIN)
    }

    fn __peek_slot(&mut self, paths: &mut Paths<'_>, end: Bpos, ip: c_ulong) -> c::bkey_s_c {
        let trans = self.trans_c();
        let flags = self.iter_flags();
        let mut k;

        trans.verify_not_unlocked_or_in_restart();
        self.verify(paths);
        self.verify_entry_exit();
        ebug_assert!(!(paths[self.path].level() != 0 && flags.contains(BtreeIterFlags::KEY_CACHED)));

        'out2: {
            'out: {
                if let Err(e) = trans.maybe_inject_restart(ip) {
                    k = bkey_s_c_err(e);
                    break 'out;
                }

                /* extents can't span inode numbers: */
                if flags.contains(BtreeIterFlags::IS_EXTENTS) && self.pos.offset == u64::MAX {
                    if self.pos.inode == u64::MAX {
                        k = bkey_s_c_null();
                        break 'out2;
                    }

                    self.set_pos_in(paths, self.pos.nosnap_successor());
                }

                let search_key = self.search_key();
                self.path = trans.path_set_pos(paths, self.path, search_key, self.intent(), self.ip_allocated());

                if let Err(e) = trans.path_traverse(paths, self.path, flags) {
                    k = bkey_s_c_err(e);
                    break 'out;
                }

                let level = paths[self.path].level() as usize;
                if paths[self.path].node(level).is_null() {
                    k = bkey_s_c_null();
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
                    k = paths[self.path].peek_slot(&mut self.k);

                    if flags.contains(BtreeIterFlags::KEY_CACHED) && self.trans_peek_key_cache(paths, &mut k) {
                        break 'out;
                    }

                    if flags.contains(BtreeIterFlags::WITH_JOURNAL) {
                        let j = self.trans_peek_slot_journal(paths);
                        if !j.k.is_null() {
                            k = j;
                        }
                    }

                    if trans.nr_updates.get() != 0 && !flags.contains(BtreeIterFlags::COMMITTED) {
                        self.trans_peek_slot_updates(&mut k);
                    }

                    if k.k.is_null() {
                        break 'out;
                    }

                    if key_of(k).key_type().is_extent_whiteout() &&
                       flags.contains(BtreeIterFlags::FILTER_SNAPSHOTS) &&
                       !flags.contains(BtreeIterFlags::NOFILTER_WHITEOUTS) {
                        self.k.set_key_type(BkeyType::deleted);
                    }
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

                    let err = 'iter2: {
                        loop {
                            k = iter2.peek_max_in(paths, search_end);
                            if iter2.is_extents() && !k.k.is_null() && bkey_err(k).is_ok() &&
                               key_of(k).key_type() == BkeyType::whiteout {
                                iter2.set_pos_in(paths, key_of(k).p);
                                continue;
                            }

                            break;
                        }

                        if !k.k.is_null() && bkey_err(k).is_ok() {
                            self.key_cache_path.swap(&iter2.key_cache_path);
                            self.k = iter2.k;
                            k.k = &raw const self.k;
                        }

                        if bkey_err(k).is_err() {
                            break 'iter2 true;
                        }

                        if !k.k.is_null() &&
                           key_of(k).key_type().is_extent_whiteout() &&
                           flags.contains(BtreeIterFlags::FILTER_SNAPSHOTS) &&
                           !flags.contains(BtreeIterFlags::NOFILTER_WHITEOUTS) {
                            self.k.set_key_type(BkeyType::deleted);
                        }

                        let mut next = if !k.k.is_null() { key_of(k).start_pos() } else { POS_MAX };
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

                            k = c::bkey_s_c { k: &raw const self.k, v: ptr::null() };
                        }
                        false
                    };
                    iter2.exit_in(paths);
                    if err {
                        break 'out;
                    }
                }
            }

            self.verify_entry_exit();
            self.verify(paths);
            if let Err(e) = self.verify_ret(paths, k) {
                k = bkey_s_c_err(e);
            }
        }

        let fs = trans.fs();
        fs.event_trace(events::btree_iter_peek_slot, |out| peek_trace(out, fs, k));

        k
    }

    pub(super) fn peek_slot_in(&mut self, paths: &mut Paths<'_>) -> c::bkey_s_c {
        let k = self.__peek_slot(paths, SPOS_MAX, crate::ret_ip!());

        self.maybe_trim(paths, k, self.pos, SPOS_MAX)
    }

    pub(super) fn peek_slot_max_in(&mut self, paths: &mut Paths<'_>, end: Bpos) -> c::bkey_s_c {
        ebug_assert!(!(self.is_extents() && !bkey_lt(self.pos, end)));

        let k = self.__peek_slot(paths, end, crate::ret_ip!());

        self.maybe_trim(paths, k, self.pos, end)
    }

    pub(super) fn next_slot_in(&mut self, paths: &mut Paths<'_>) -> c::bkey_s_c {
        if !self.advance_in(paths) {
            return bkey_s_c_null();
        }

        self.peek_slot_in(paths)
    }

    pub(super) fn prev_slot_in(&mut self, paths: &mut Paths<'_>) -> c::bkey_s_c {
        /* reverse slots aren't defined for extents: peek_slot() is the slot from @pos on */
        assert!(!self.is_extents());

        if !self.rewind_in(paths) {
            return bkey_s_c_null();
        }

        self.peek_slot_in(paths)
    }

    pub(super) fn peek_root_in(&mut self, paths: &mut Paths<'_>, trans: &BtreeTrans<'_>, btree: Btree,
                               level: u32) -> c::bkey_s_c {
        let fs = trans.fs();

        while fs.btree.cache.roots.node(btree.0).is_some_and(|root| level == u32::from(root.c.level) + 1) {
            self.exit_in(paths);
            unsafe {
                BtreeIter::node_iter_init_in(self, trans, paths, btree, POS_MIN, 0, level - 1,
                                             BtreeIterFlags::NOT_EXTENTS | BtreeIterFlags::ALL_SNAPSHOTS,
                                             crate::ret_ip!());
            }
            let b = self.peek_node_in(paths);
            let b = match errptr_or_null(b) {
                Err(e)      => return bkey_s_c_err(e),
                Ok(b)       => b,
            };
            let b = unsafe { LockedNode::new(b.expect("a root's level has a node").as_ref()) };

            if fs.btree.cache.roots.node(btree.0).is_none_or(|r| !ptr::eq(r, b.node())) {
                continue;
            }

            if b.is_fake() {
                break;
            }

            return bkey_i_to_s_c(b.contents().key_i().as_ffi());
        }

        bkey_s_c_null()
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

    /// # Safety
    /// @iter is valid for writes; what it held, if anything, is put.
    #[allow(clippy::too_many_arguments)]
    unsafe fn init_common(iter: *mut Self, trans: &BtreeTrans<'_>, paths: &mut Paths<'_>, btree: Btree,
                          pos: Bpos, locks_want: u32, depth: u32, flags: BtreeIterFlags, ip: c_ulong) {
        unsafe {
            iter.write(BtreeIter::uninit());
            let iter = &mut *iter;

            iter.trans = ptr::from_ref(trans).cast_mut().cast();
            iter.set_btree(btree);
            iter.flags = flags.bits();
            iter.snapshot = pos.snapshot;
            iter.pos = pos;
            iter.k = c::bkey { p: pos, ..c::bkey::new() };
            #[cfg(CONFIG_BCACHEFS_DEBUG)]
            {
                iter.ip_allocated = ip;
            }
            iter.path = trans.path_get(paths, btree, pos, locks_want, depth, flags, ip);
        }
    }

    pub(super) fn init_in(trans: &BtreeTrans<'_>, paths: &mut Paths<'_>, btree: Btree, pos: Bpos,
                          flags: BtreeIterFlags, ip: c_ulong) -> Self {
        let mut iter = MaybeUninit::<Self>::uninit();
        unsafe {
            Self::init_common(iter.as_mut_ptr(), trans, paths, btree, pos, 0, 0,
                              btree_iter_flags(trans, btree, 0, flags), ip);
            iter.assume_init()
        }
    }

    /// # Safety
    /// As init_common()'s.
    #[allow(clippy::too_many_arguments)]
    unsafe fn init_ll_in(iter: *mut Self, trans: &BtreeTrans<'_>, paths: &mut Paths<'_>, btree: Btree,
                         pos: Bpos, locks_want: u32, depth: u32, flags: BtreeIterFlags, ip: c_ulong) {
        unsafe {
            Self::init_common(iter, trans, paths, btree, pos, locks_want, depth,
                              btree_iter_flags(trans, btree, depth, flags), ip);

            let iter = &mut *iter;
            iter.min_depth = depth as u8;
            #[cfg(CONFIG_BCACHEFS_DEBUG)]
            {
                let path = &paths[iter.path];
                assert!(u32::from(path.locks_want()) >= locks_want.min(c::BTREE_MAX_DEPTH));
                assert!(u32::from(path.level()) == depth);
                assert!(u32::from(iter.min_depth) == depth);
            }
        }
    }

    /// # Safety
    /// As init_common()'s.
    #[allow(clippy::too_many_arguments)]
    unsafe fn node_iter_init_in(iter: *mut Self, trans: &BtreeTrans<'_>, paths: &mut Paths<'_>, btree: Btree,
                                pos: Bpos, locks_want: u32, depth: u32, mut flags: BtreeIterFlags, ip: c_ulong) {
        flags |= BtreeIterFlags::NOT_EXTENTS;
        flags |= BtreeIterFlags::SNAPSHOT_FIELD;
        flags |= BtreeIterFlags::ALL_SNAPSHOTS;

        if depth == 0 && btree_id_cached(btree) {
            flags |= BtreeIterFlags::KEY_CACHED;
        }

        unsafe { Self::init_ll_in(iter, trans, paths, btree, pos, locks_want, depth, flags, ip) }
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
pub unsafe fn __bch2_btree_iter_traverse(iter: *mut c::btree_iter) -> c_int {
    unsafe { with_paths(iter, |iter, paths| ret_to_c(iter.traverse_at_pos_in(paths))) }
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
    unsafe { with_paths(iter, |iter, paths| iter.peek_node_in(paths)) }
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
    unsafe { with_paths(iter, |iter, paths| iter.peek_max_in(paths, *end)) }
}

/**
 * bch2_btree_iter_next() - returns first key greater than iterator's current
 * position
 * @iter:	iterator to peek from
 *
 * Returns:	key if found, or an error extractable with bkey_err().
 */
/// # Safety
/// As bch2_set_btree_iter_dontneed()'s.
#[c_export]
pub unsafe fn bch2_btree_iter_next(iter: *mut c::btree_iter) -> c::bkey_s_c {
    unsafe { with_paths(iter, |iter, paths| iter.next_in(paths)) }
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
    unsafe { with_paths(iter, |iter, paths| iter.peek_prev_min_in(paths, end)) }
}

/**
 * bch2_btree_iter_prev() - returns first key less than iterator's current
 * position
 * @iter:	iterator to peek from
 *
 * Returns:	key if found, or an error extractable with bkey_err().
 */
/// # Safety
/// As bch2_set_btree_iter_dontneed()'s.
#[c_export]
pub unsafe fn bch2_btree_iter_prev(iter: *mut c::btree_iter) -> c::bkey_s_c {
    unsafe { with_paths(iter, |iter, paths| iter.prev_in(paths)) }
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
    unsafe { with_paths(iter, |iter, paths| iter.peek_slot_in(paths)) }
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
    unsafe { with_paths(iter, |iter, paths| iter.peek_slot_max_in(paths, end)) }
}

/// # Safety
/// As bch2_set_btree_iter_dontneed()'s.
#[c_export]
pub unsafe fn bch2_btree_iter_next_slot(iter: *mut c::btree_iter) -> c::bkey_s_c {
    unsafe { with_paths(iter, |iter, paths| iter.next_slot_in(paths)) }
}

/// # Safety
/// As bch2_set_btree_iter_dontneed()'s.
#[c_export]
pub unsafe fn bch2_btree_iter_prev_slot(iter: *mut c::btree_iter) -> c::bkey_s_c {
    unsafe { with_paths(iter, |iter, paths| iter.prev_slot_in(paths)) }
}

/// # Safety
/// @iter is an initialized iterator, or zeroed, C's for the call.
#[c_export]
pub unsafe fn bch2_btree_iter_peek_root(trans: &BtreeTrans<'_>, #[lent(trans)] paths: &mut Paths<'_>,
                                        iter: *mut c::btree_iter, btree: Btree, level: c_uint) -> c::bkey_s_c {
    unsafe {
        (*iter.cast::<BtreeIter<'static>>()).peek_root_in(paths, trans, btree, level)
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
    unsafe {
        BtreeIter::init_common(iter.cast(), trans, paths, btree_id, pos, 0, 0,
                               btree_iter_flags(trans, btree_id, 0, BtreeIterFlags::from_bits_retain(flags.bits())),
                               ip);
    }
}

/*
 * Check iterator parameters from an untrusted source (e.g. an ioctl) against
 * the preconditions the iterator code enforces with assertions. Kernel
 * callers know these rules; user-controlled (btree, level, range, flags)
 * combinations must be validated first:
 */
#[c_export]
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
    unsafe {
        BtreeIter::init_ll_in(iter.cast(), trans, paths, btree_id, pos, locks_want, depth,
                              BtreeIterFlags::from_bits_retain(flags.bits()), ip);
    }
}

/// # Safety
/// As bch2_trans_iter_init_outlined()'s.
#[c_export]
pub unsafe fn __bch2_trans_node_iter_init(trans: &BtreeTrans<'_>, #[lent(trans)] paths: &mut Paths<'_>,
                                          iter: *mut c::btree_iter, btree_id: Btree, pos: Bpos,
                                          locks_want: c_uint, depth: c_uint,
                                          flags: c::btree_iter_update_trigger_flags) {
    unsafe {
        BtreeIter::node_iter_init_in(iter.cast(), trans, paths, btree_id, pos, locks_want, depth,
                                     BtreeIterFlags::from_bits_retain(flags.bits()), crate::ret_ip!());
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
        let trans = t.trans();
        let k = unsafe { bkey_s_c_to_result::<'i>(self.peek_root_in(&mut trans.paths(), trans, btree, level)) }?;
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
        let mut iter = MaybeUninit::<BtreeIter<'t>>::uninit();

        unsafe {
            BtreeIter::node_iter_init_in(iter.as_mut_ptr(), trans, &mut trans.paths(),
                                         Btree::from_raw(btree.into()).expect("invalid btree id"),
                                         pos, 0, level, flags, crate::ret_ip!());
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
        let k = unsafe { c::bch2_btree_iter_peek_slot(self.raw_mut()) };
        let this: &'k Self = self;
        // 'k: the key is valid while the iterator is borrowed, within the
        // attempt - shared, it can't be moved or peeked again
        let k = unsafe { bkey_s_c_to_result::<'k>(k)? };
        Ok((this, k.expect("a slot always has a key")))
    }

    /// As peek_slot_typed(), for a key to update through this iterator: the
    /// iterator comes back shared, for as long as the key is used - as the
    /// for_each loops pass it - so the key can't be moved out from under.
    pub fn peek_slot_typed_shared<'k, K: BkeyFamily + ?Sized>(&'k mut self, t: &'k TransAttempt<'_, 't>)
        -> Result<(&'k Self, BkeySC<'k, K::Validated>), BchError>
    {
        let k = unsafe { c::bch2_btree_iter_peek_slot(self.raw_mut()) };
        let this: &'k Self = self;
        // 'k: the key is valid while the iterator is borrowed, within the
        // attempt - shared, it can't be moved or peeked again
        let k = unsafe { bkey_s_c_to_result::<'k>(k)? };
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
        let trans = unsafe { &*trans.trans_raw() };
        let mut iter = MaybeUninit::<BtreeIter<'t>>::uninit();

        unsafe {
            BtreeIter::node_iter_init_in(iter.as_mut_ptr(), trans, &mut trans.paths(),
                                         Btree::from_raw(btree.into()).expect("invalid btree id"),
                                         pos, locks_want, depth, flags, crate::ret_ip!());
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
