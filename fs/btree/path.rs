// SPDX-License-Identifier: GPL-2.0

//! btree_path: the low level path to a btree node, which holds its locks.

#![allow(non_camel_case_types)]

use crate::btree::iter::BtreeTrans;
use crate::btree::node::iter::KeyIter;
use crate::btree::locking::{btree_node_locked_type, BTREE_NODE_UNLOCKED};
use crate::btree::node::{BtreeNode, NodeContents};
use crate::c;
use crate::cstructs::c as cs;
use crate::errcode::{bch_errcode, err_ptr, errptr_or_null, BchError};
use crate::format::{Bpos, Btree};
use crate::types::c_default;
#[cfg(CONFIG_BCACHEFS_LOCK_TIME_STATS)]
use crate::util::kernel::local_clock;
use cstruct_macros::{bitfield, c_name, c_typedef, CStruct};
use crate::locking::{Locked, LockedBy};
use crate::locking::six::SixHold;
use crate::c::six_lock_type;
use crate::util::ffi::Opaque;
use core::ffi::{c_int, c_ulong};
use core::mem::{offset_of, ManuallyDrop};
use core::ops::{Deref, DerefMut, Index, IndexMut};
use core::ptr::NonNull;

c_typedef! {
    #[c_name(btree_path_idx_t)]
    pub type PathIdx = u16;
}

#[repr(C)]
#[derive(CStruct)]
pub struct btree_path_level<T: PathNode = cs::btree_bkey_cached_common> {
    /* a cached path's is a struct bkey_cached's */
    #[c("struct btree *b")]
    pub b: *mut T,
    pub iter: KeyIter,
    pub lock_seq: u32,
    #[cfg(CONFIG_BCACHEFS_LOCK_TIME_STATS)]
    pub lock_taken_time: u64,
}
c_default!(btree_path_level);

impl<T: PathNode> Clone for btree_path_level<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: PathNode> Copy for btree_path_level<T> {}

#[bitfield(u16, ty = u32)]
pub struct btree_path_bits {
    #[bits(7)] pub btree_id: Btree,
    cached:           bool,
    preserve:         bool,
    /*
     * When true, failing to relock this path will cause the transaction to
     * restart:
     */
    should_be_locked: bool,
    level:            u3,
    locks_want:       u3,
}

/// A path whose levels are Ts: BtreePath<BtreeNode>, a key cache path's
/// BtreePath<bkey_cached>, or - the default - any path, its levels seen as
/// their common parts. All are C's struct btree_path.
#[repr(C)]
#[c_name(btree_path)]
#[derive(CStruct)]
pub struct BtreePath<T: PathNode = cs::btree_bkey_cached_common> {
    pub sorted_idx: cs::PathIdx,
    pub ref_: u8,
    pub intent_ref: u8,

    /* btree_iter_copy starts here: */
    pub pos: Bpos,

    #[c_bitfield]
    pub bits: btree_path_bits,
    pub nodes_locked: u8,

    pub l: [cs::btree_path_level<T>; cs::BTREE_MAX_DEPTH as usize],
    /* C's TRACK_PATH_ALLOCATED, which btree/types_inline.h defines from
     * these: rustc sees only the configuration */
    #[cfg(any(CONFIG_BCACHEFS_LOCK_TIME_STATS, CONFIG_BCACHEFS_DEBUG))]
    pub ip_allocated: core::ffi::c_ulong,
}
c_default!(BtreePath);

/// A path as it was, for a trace: a copy that can't be had as a path, so
/// nothing takes its record of locks for locks held.
pub(crate) struct PathSnapshot(BtreePath);

impl PathSnapshot {
    pub(crate) fn of(path: &BtreePath) -> Self {
        PathSnapshot(unsafe { core::ptr::read(path) })
    }

    /// For C's bch2_btree_path_to_text().
    pub(crate) fn as_ptr(&self) -> *mut BtreePath {
        core::ptr::from_ref(&self.0).cast_mut()
    }
}

/// A transaction's path table, borrowed: one borrow at a time, by a bit in
/// the transaction's flags - a RefCell's - so a Rust entry into the btree code
/// made while it's in there panics rather than aliasing the table. Growing it
/// will take it &mut, so it can't grow under a path borrowed from it.
pub struct Paths<'t> {
    trans: &'t BtreeTrans<'t>,
    /// false for the borrow C lent an export
    took:  bool,
}

impl BtreeTrans<'_> {
    /// The path table, for a Rust entry into the btree code.
    pub(crate) fn paths(&self) -> Paths<'_> {
        assert!(!self.paths_borrowed(), "{}: the path table borrowed twice - the btree code reentered",
                self.fn_name());
        self.set_paths_borrowed(true);
        Paths { trans: self, took: true }
    }

    /// The path table, for a C export: C's to lend for the call, its own
    /// caller holding no reference into it - a Rust caller's borrow, if there
    /// is one, idle across the call into C.
    ///
    /// # Safety
    /// As above.
    pub(crate) unsafe fn paths_lent(&self) -> Paths<'_> {
        Paths { trans: self, took: false }
    }
}

impl Paths<'_> {
    /// Write over the intent path @idx holds at @level, which nodes_locked
    /// already says is write - marked before the wait, for the cycle detector
    /// to see - waited for with @wait, the transaction's own read holds on the
    /// node set aside meanwhile. On failure, six's error, and the level's back
    /// to intent.
    pub(crate) fn lock_write_marked(&mut self, idx: PathIdx, level: usize, wait: &Opaque<c::six_lock_waiter>,
                                    should_sleep_fn: c::six_lock_should_sleep_fn, ip: c_ulong) -> Result<(), c_int> {
        ebug_assert!(self[idx].write_locked(level), "taking write at a level not marked for it");
        let node = self[idx].held_node(level);
        let readers = self.trans.node_lock_counts(self, core::ptr::null(), node.as_ptr(), level)
            .n[c::SIX_LOCK_read as usize] as c_int;

        match unsafe {
            SixHold::from_raw(&node.as_ref().lock, six_lock_type::SIX_LOCK_intent)
                .write_waiting_over(readers, wait, should_sleep_fn, ip)
        } {
            Ok(write) => {
                write.into_raw();
                Ok(())
            }
            Err((intent, e)) => {
                self[idx].mark_locked_noreset(level, btree_node_locked_type::from_six(intent.into_raw()));
                Err(e)
            }
        }
    }
}

impl Drop for Paths<'_> {
    fn drop(&mut self) {
        if self.took {
            self.trans.set_paths_borrowed(false);
        }
    }
}

impl Deref for Paths<'_> {
    type Target = [BtreePath];

    fn deref(&self) -> &[BtreePath] {
        let t = self.trans;
        unsafe { core::slice::from_raw_parts(t.paths.get(), t.nr_paths.get() as usize) }
    }
}

impl DerefMut for Paths<'_> {
    fn deref_mut(&mut self) -> &mut [BtreePath] {
        let t = self.trans;
        unsafe { core::slice::from_raw_parts_mut(t.paths.get(), t.nr_paths.get() as usize) }
    }
}

impl Index<PathIdx> for Paths<'_> {
    type Output = BtreePath;

    fn index(&self, idx: PathIdx) -> &BtreePath {
        &(**self)[idx as usize]
    }
}

impl IndexMut<PathIdx> for Paths<'_> {
    fn index_mut(&mut self, idx: PathIdx) -> &mut BtreePath {
        &mut (**self)[idx as usize]
    }
}

impl Paths<'_> {
    /// Two different paths, the second mutably: for a hold from one's level
    /// to go to the other's.
    pub(crate) fn pair(&mut self, a: PathIdx, b: PathIdx) -> (&BtreePath, &mut BtreePath) {
        assert!(a != b, "a path paired with itself");
        let (a, b) = (a as usize, b as usize);
        if a < b {
            let (l, r) = self.split_at_mut(b);
            (&l[a], &mut r[0])
        } else {
            let (l, r) = self.split_at_mut(a);
            (&r[0], &mut l[b])
        }
    }
}

/// What a path's levels point at: a btree node, or - a cached path's - a key
/// cache entry, each starting with a btree_bkey_cached_common. The common part
/// is one too, for code over any path: relocking, the cycle detector.
///
/// # Safety
/// Self starts with a btree_bkey_cached_common, and a common part whose cached
/// flag is CACHED is the start of one. It's freed only by what's locked it for
/// intent and write: a hold on its lock keeps it.
pub unsafe trait PathNode: 'static {
    /// The cached flag of a common part that's one of these; None for any.
    const CACHED: Option<bool>;

    fn common(&self) -> &c::btree_bkey_cached_common;
}

unsafe impl PathNode for c::btree_bkey_cached_common {
    const CACHED: Option<bool> = None;

    fn common(&self) -> &c::btree_bkey_cached_common {
        self
    }
}

unsafe impl PathNode for BtreeNode {
    const CACHED: Option<bool> = Some(false);

    fn common(&self) -> &c::btree_bkey_cached_common {
        &self.c
    }
}

unsafe impl PathNode for c::bkey_cached {
    const CACHED: Option<bool> = Some(true);

    fn common(&self) -> &c::btree_bkey_cached_common {
        &self.c
    }
}

const _: () = assert!(offset_of!(BtreeNode, c) == 0 && offset_of!(c::bkey_cached, c) == 0 &&
                      offset_of!(c::btree_bkey_cached_common, lock) == 0);

impl BtreePath {
    /// As a path of Ts, if it is one: by its cached flag.
    pub fn downcast<T: PathNode>(&self) -> Option<&BtreePath<T>> {
        T::CACHED.map_or(true, |cached| cached == self.bits.cached())
            .then(|| unsafe { &*core::ptr::from_ref(self).cast() })
    }
}

/// A PathNode whose lock protects only part of it: the part only a hold on the
/// lock reaches.
pub trait PathNodeContents: PathNode {
    type Contents;

    fn locked_contents(&self) -> &LockedBy<Self::Contents>;
}

impl PathNodeContents for BtreeNode {
    type Contents = NodeContents;

    fn locked_contents(&self) -> &LockedBy<NodeContents> {
        &self.contents
    }
}

/// A node a path has locked, borrowed from the attempt as a key is: good until
/// the attempt's next unlock or error, which nothing checks (see TransRef).
pub struct LockedNode<'p, T: PathNode> {
    node: &'p T,
}

impl<T: PathNode> Clone for LockedNode<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: PathNode> Copy for LockedNode<'_, T> {}

impl<'p, T: PathNode> LockedNode<'p, T> {
    /// # Safety
    /// @node is locked, any way, for 'p.
    pub unsafe fn new(node: &'p T) -> Self {
        LockedNode { node }
    }

    pub fn node(&self) -> &'p T {
        self.node
    }
}

impl<'p, T: PathNodeContents> LockedNode<'p, T> {
    pub fn contents(&self) -> &'p T::Contents {
        unsafe { self.node.locked_contents().protected() }
    }
}

impl<T: PathNode> Deref for LockedNode<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.node
    }
}

impl<T: PathNode> BtreePath<T> {
    /// Another hold on the level's node, as @want, if the level's is no
    /// weaker.
    pub fn increment_level(&self, level: usize, want: btree_node_locked_type) -> Option<SixHold<'_>> {
        let locked = self.locked_type(level);
        (locked != btree_node_locked_type::BTREE_NODE_UNLOCKED && locked as i32 >= want as i32).then(|| unsafe {
            let lock = &self.held_node(level).as_ref().common().lock;
            ManuallyDrop::new(SixHold::from_raw(lock, locked.six())).increment(want.six())
        })
    }

    /// The level's node, relocked as @ty if nothing's written it since the
    /// level's lock_seq: if it's still the node the level had.
    pub fn relock_level(&mut self, level: usize, ty: six_lock_type) -> bool {
        let Some(node) = errptr_or_null(self.l[level].b).ok().flatten() else { return false };
        let node = unsafe { node.as_ref() };
        let Some(hold) = node.common().lock.relock_as(ty, self.l[level].lock_seq) else { return false };
        self.put_level(level, node, hold);
        true
    }

    /// The level's hold, through @f - which has what the level holds after:
    /// None, unlocked, its node and lock_seq kept for a relock.
    fn change_level(&mut self, level: usize, f: impl FnOnce(SixHold<'_>) -> Option<SixHold<'_>>) {
        let locked = self.locked_type(level);
        if locked == btree_node_locked_type::BTREE_NODE_UNLOCKED {
            return;
        }

        let node = self.held_node(level);
        let locked = f(unsafe { SixHold::from_raw(&node.as_ref().common().lock, locked.six()) })
            .map_or(btree_node_locked_type::BTREE_NODE_UNLOCKED, |hold| btree_node_locked_type::from_six(hold.into_raw()));
        self.mark_locked_noreset(level, locked);
    }

    /// @hold on @node's lock, at a level holding none: the path's now, as of
    /// the lock's seq.
    pub fn put_level(&mut self, level: usize, node: &T, hold: SixHold<'_>) {
        ebug_assert!(!self.locked(level), "a level already locked");
        assert!(core::ptr::eq(hold.lock().six(), node.common().lock.six()), "a level given a hold on another node's lock");

        self.l[level].b = core::ptr::from_ref(node).cast_mut();
        self.l[level].lock_seq = node.common().lock.six().seq.get();
        self.mark_locked_noreset(level, btree_node_locked_type::from_six(hold.into_raw()));
    }

    /// For a lock just taken - not one changed or handed between paths: its
    /// hold time starts.
    pub fn lock_taken(&mut self, level: usize) {
        #[cfg(CONFIG_BCACHEFS_LOCK_TIME_STATS)]
        {
            self.l[level].lock_taken_time = local_clock();
        }
        #[cfg(not(CONFIG_BCACHEFS_LOCK_TIME_STATS))]
        let _ = level;
    }

    /// Whether there was a lock to drop.
    pub fn unlock_level(&mut self, level: usize) -> bool {
        let locked = self.locked(level);
        self.change_level(level, |_| None);
        locked
    }

    /// A level held for write, back to intent.
    pub fn unlock_write_level(&mut self, level: usize) {
        assert!(self.write_locked(level), "unlocking a node for write that the path hasn't write locked");
        self.change_level(level, |hold| Some(hold.downgrade()));
    }

    /// A level held for intent, down to read.
    pub fn downgrade_level(&mut self, level: usize) {
        if self.locked_type(level) == btree_node_locked_type::BTREE_NODE_INTENT_LOCKED {
            self.change_level(level, |hold| Some(hold.downgrade()));
        }
    }

    /// A level held for read, up to intent if it can be: whether it's held
    /// for intent.
    pub fn try_upgrade_level(&mut self, level: usize) -> bool {
        match self.locked_type(level) {
            btree_node_locked_type::BTREE_NODE_UNLOCKED => false,
            btree_node_locked_type::BTREE_NODE_READ_LOCKED => {
                let mut upgraded = false;
                self.change_level(level, |hold| Some(match hold.try_upgrade() {
                    Ok(hold)  => {
                        upgraded = true;
                        hold
                    }
                    Err(hold) => hold,
                }));
                upgraded
            }
            _ => true,
        }
    }

    /// The node of a level nodes_locked says is held.
    fn held_node(&self, level: usize) -> NonNull<T> {
        NonNull::new(self.l[level].b).expect("a locked level has its node")
    }

    /// The level's node, if the path holds its lock: what makes it the path's,
    /// and keeps it while the path's borrowed.
    pub fn locked_node(&self, level: usize) -> Option<LockedNode<'_, T>> {
        self.locked(level).then(|| unsafe { LockedNode::new(self.held_node(level).as_ref()) })
    }
}

/// What every level of a path table the transaction has reallocated away from
/// holds, in debug builds: for a path pointer kept across the realloc.
pub(crate) fn btree_path_poison() -> *mut c::btree_bkey_cached_common {
    err_ptr(BchError::from(bch_errcode::BCH_ERR_no_btree_node_stale_paths))
}

impl<T: PathNode> BtreePath<T> {
    /// NULL above the root, or an error pointer: why there's no node. A
    /// cached path's is a key cache entry's.
    pub(crate) fn node(&self, level: usize) -> *mut T {
        if level >= c::BTREE_MAX_DEPTH as usize {
            return core::ptr::null_mut();
        }

        let b = self.l[level].b;

        /*
         * Spot check, deliberately not exhaustive: most level accesses go
         * direct to path->l[] rather than through here. Sites that dereference
         * b fault on the poison unaided; this catches some of the ones that
         * only compare it, which otherwise read as "not the node I wanted" and
         * take a plausible wrong branch.
         */
        if cfg!(CONFIG_BCACHEFS_DEBUG) {
            crate::warn_once!(b.cast() == btree_path_poison(), no_fs,
                              "using a btree path that outlived trans->paths");
        }
        b
    }

    pub(crate) fn lock_seq_matches(&self, lock: &Locked<c::six_lock>, level: usize) -> bool {
        self.l[level].lock_seq == lock.six().seq.get()
    }

    /// Whether the level has @b, as of its lock's seq @seq.
    pub(crate) fn has_node(&self, level: usize, b: *const T, seq: u32) -> bool {
        core::ptr::eq(self.l[level].b, b) && self.l[level].lock_seq == seq
    }
}

impl btree_node_locked_type {
    /// The lock at @level in a path's nodes_locked: two bits a level, the six
    /// lock type plus one.
    pub fn of(nodes_locked: u8, level: usize) -> Self {
        Self::from_bits(BTREE_NODE_UNLOCKED + ((i32::from(nodes_locked) >> (level << 1)) & 3))
    }

    /// Held as six lock type @ty: the two number them the same.
    pub fn from_six(ty: six_lock_type) -> Self {
        Self::from_bits(ty as i32)
    }

    /// The six lock type it's held as, locked.
    pub fn six(self) -> six_lock_type {
        six_lock_type::from_bits(self as u32)
    }
}

impl<T: PathNode> BtreePath<T> {
    pub(crate) fn is_btree_node(&self, level: usize) -> bool {
        level < c::BTREE_MAX_DEPTH as usize && matches!(errptr_or_null(self.l[level].b), Ok(Some(_)))
    }

    pub(crate) fn locked_type(&self, level: usize) -> btree_node_locked_type {
        btree_node_locked_type::of(self.nodes_locked, level)
    }

    pub(crate) fn locked_type_nowrite(&self, level: usize) -> btree_node_locked_type {
        match self.locked_type(level) {
            btree_node_locked_type::BTREE_NODE_WRITE_LOCKED => btree_node_locked_type::BTREE_NODE_INTENT_LOCKED,
            have => have,
        }
    }

    pub(crate) fn write_locked(&self, level: usize) -> bool {
        matches!(self.locked_type(level), btree_node_locked_type::BTREE_NODE_WRITE_LOCKED)
    }

    pub(crate) fn intent_locked(&self, level: usize) -> bool {
        matches!(self.locked_type(level), btree_node_locked_type::BTREE_NODE_INTENT_LOCKED)
    }

    pub(crate) fn locked(&self, level: usize) -> bool {
        !matches!(self.locked_type(level), btree_node_locked_type::BTREE_NODE_UNLOCKED)
    }

    fn mark_locked_noreset(&mut self, level: usize, ty: btree_node_locked_type) {
        /* relying on this to avoid a branch */
        const { assert!(c::SIX_LOCK_read as i32 == 0 && c::SIX_LOCK_intent as i32 == 1) };

        self.nodes_locked &= !(3 << (level << 1));
        self.nodes_locked |= ((ty as i32 + 1) as u8) << (level << 1);
    }

    pub(crate) fn lock_want_six(&self, level: usize) -> c::six_lock_type {
        if level < self.locks_want() as usize {
            c::six_lock_type::SIX_LOCK_intent
        } else {
            c::six_lock_type::SIX_LOCK_read
        }
    }

    pub(crate) fn lock_want(&self, level: usize) -> btree_node_locked_type {
        let (path_level, locks_want) = (self.level() as usize, self.locks_want() as usize);

        if level < path_level {
            btree_node_locked_type::BTREE_NODE_UNLOCKED
        } else if level < locks_want {
            btree_node_locked_type::BTREE_NODE_INTENT_LOCKED
        } else if level == path_level {
            btree_node_locked_type::BTREE_NODE_READ_LOCKED
        } else {
            btree_node_locked_type::BTREE_NODE_UNLOCKED
        }
    }

    /// Only with something locked.
    pub(crate) fn lowest_level_locked(&self) -> usize {
        self.nodes_locked.trailing_zeros() as usize >> 1
    }

    /// Only with something locked.
    pub(crate) fn highest_level_locked(&self) -> usize {
        (u8::BITS - 1 - self.nodes_locked.leading_zeros()) as usize >> 1
    }
}
