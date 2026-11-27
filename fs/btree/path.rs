// SPDX-License-Identifier: GPL-2.0

//! btree_path: the low level path to a btree node, which holds its locks.

#![allow(non_camel_case_types)]

use crate::btree::bkey::{bkey_eq, bkey_ge, bkey_le, BkeyI, BkeyV, BKEY_U64S};
use crate::btree::cache::{btree_id_level_to_text, OwnedNode};
use crate::btree::iter::{BtreeIterFlags, BtreeNodeRef, BtreeTrans};
use crate::btree::key_cache::{EntryContents, BkeyCached};
use crate::btree::node::iter::KeyIter;
use crate::btree::node::keys::{Keys, KeysMut};
use crate::btree::locking::BtreeNodeLockedType::{self, *};
use crate::btree::node::{BtreeNode, NodeContents, NodeKey, PublishedKey};
use crate::btree::traverse::advance_to_pos;
use crate::btree::locking::BtreeBkeyCachedCommon;
use crate::btree::update::BtreeInsertEntry;
use crate::data::extents_format::BKEY_BTREE_PTR_VAL_U64s_MAX;
use crate::data_structures::{dst_room::DstRoom, bitmap::Bitmap};
use crate::debug::{params::debug_check_iterators, trace::{events, TraceEvent}};
use crate::prt;
use crate::errcode::{Errcode, err_ptr, errptr_or_null, BchError};
use crate::format::{BkeyType, Bpos, Btree, Val, Bkey, BTREE_MAX_DEPTH};
use crate::fs::Fs;
use crate::types::{bits_to_longs, Lent};
use crate::util::alloc::kvzalloc_nofail;
use crate::util::backtrace::prt_symbol;
use crate::util::log::LogMsg;
use crate::util::rcu::rcu_assign_pointer;
use crate::util::Printbuf;

/* Number of btree paths we preallocate, usually enough */
pub const BTREE_ITER_INITIAL: u32 = 64;

/* never exceed limit */
pub const BTREE_ITER_MAX: u32 = 1 << 10;

#[repr(C)]
#[derive(Default, MaybeZeroable)]
pub struct BtreeTransPaths {
    pub nr_paths: usize,
    pub paths: [BtreePath; 0],
}

#[cfg(CONFIG_BCACHEFS_LOCK_TIME_STATS)]
use crate::os::kernel::local_clock;
use cstruct_macros::{bitfield, MaybeZeroable, BitfieldMembers};
use crate::locking::{Alone, Guard, Locked, LockedBy};
use crate::locking::six::{six_trylock_ip, SixHold, SixLock, SixLockWaiter, SixLockType};
use core::cmp::Ordering;
use core::marker::PhantomData;
use core::mem::{offset_of, ManuallyDrop};
use core::ops::{Deref, DerefMut, Index, IndexMut};
use core::ptr::{self, NonNull};

pub type PathIdx = u16;

/// Clone and Copy, which #[derive] would bound on T: these have their T only
/// by pointer.
macro_rules! copy {
    ([$($g:tt)*] $t:ty) => {
        impl<$($g)*> Clone for $t {
            fn clone(&self) -> Self {
                *self
            }
        }

        impl<$($g)*> Copy for $t {}
    };
}

/// What a peek found: a key's value - NULL for a hole - its header where the
/// peek unpacked or copied it - for an iterator's peek, its own, iter->k.
#[derive(Clone, Copy)]
pub(crate) struct KeyVal(pub(crate) *const Val);

/// A path level's node, as C has it: NULL above the root, an error pointer
/// saying why there's no node, or the node - only had by decoding it. An
/// enum can't be one pointer, as Rust's only pointer niche is NULL.
#[repr(transparent)]
#[derive(::pin_init::Zeroable)]
pub struct NodePtr<T>(*mut T);
copy!([T] NodePtr<T>);

impl<T> PartialEq for NodePtr<T> {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl<T> NodePtr<T> {
    pub fn null() -> Self {
        NodePtr(core::ptr::null_mut())
    }

    pub fn err(e: BchError) -> Self {
        NodePtr(err_ptr(e))
    }

    pub fn node(node: NonNull<T>) -> Self {
        NodePtr(node.as_ptr())
    }

    pub fn get(self) -> Result<Option<NonNull<T>>, BchError> {
        errptr_or_null(self.0)
    }

    pub fn is_null(self) -> bool {
        self.0.is_null()
    }

    pub fn is_node(self) -> bool {
        matches!(self.get(), Ok(Some(_)))
    }

    /// Whether it's @b: by address, reading nothing.
    pub fn is(self, b: *const T) -> bool {
        core::ptr::eq(self.0, b)
    }

    pub fn as_ptr(self) -> *mut T {
        self.0
    }
}

#[repr(C)]
#[derive(MaybeZeroable)]
pub struct BtreePathLevel<T: PathNode = BtreeBkeyCachedCommon> {
    /* a cached path's is a struct bkey_cached's */
    pub b: NodePtr<T>,
    pub iter: KeyIter,
    pub lock_seq: u32,
    #[cfg(CONFIG_BCACHEFS_LOCK_TIME_STATS)]
    pub lock_taken_time: u64,
}
copy!([T: PathNode] BtreePathLevel<T>);

#[bitfield(u16)]
pub struct BtreePathBits {
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
#[derive(MaybeZeroable, BitfieldMembers)]
pub struct BtreePath<T: PathNode = BtreeBkeyCachedCommon> {
    pub sorted_idx: PathIdx,
    pub ref_: u8,
    pub intent_ref: u8,

    /* btree_iter_copy starts here: */
    pub pos: Bpos,

    #[members]
    pub bits: BtreePathBits,
    pub nodes_locked: u8,

    pub l: [BtreePathLevel<T>; BTREE_MAX_DEPTH as usize],
    /* C's TRACK_PATH_ALLOCATED, which btree/types_inline.h defines from
     * these: rustc sees only the configuration */
    #[cfg(any(CONFIG_BCACHEFS_LOCK_TIME_STATS, CONFIG_BCACHEFS_DEBUG))]
    pub ip_allocated: usize,
}

/// A path as it was, for a trace: a copy that can't be had as a path, so
/// nothing takes its record of locks for locks held.
pub(crate) struct PathSnapshot(BtreePath);

impl PathSnapshot {
    pub(crate) fn of(path: &BtreePath) -> Self {
        PathSnapshot(unsafe { ptr::read(path) })
    }

    pub(crate) fn to_text(&self, out: &mut Printbuf, idx: PathIdx) {
        self.0.to_text(out, idx)
    }
}

pub(crate) type DownKey = DstRoom<BkeyI, [u64; BKEY_U64S + BKEY_BTREE_PTR_VAL_U64s_MAX]>;

/// A transaction's path table, borrowed: one borrow at a time, by a bit in
/// the transaction's flags - a RefCell's - so a Rust entry into the btree code
/// made while it's in there panics rather than aliasing the table. Growing it
/// will take it &mut, so it can't grow under a path borrowed from it.
pub struct Paths<'t> {
    trans: &'t BtreeTrans<'t>,
    /// trans->paths, held for the borrow: growing it, the one thing that moves
    /// it, takes the borrow &mut, as does lending it to code that may grow it
    table: *mut [BtreePath],
    /// false for the borrow C lent an export
    took:  bool,
}

impl Paths<'_> {
    fn table_of(trans: &BtreeTrans<'_>) -> *mut [BtreePath] {
        ptr::slice_from_raw_parts_mut(trans.paths.get(), trans.nr_paths.get() as usize)
    }
}

impl BtreeTrans<'_> {
    /// The path table, for a Rust entry into the btree code.
    #[inline]
    pub(crate) fn paths(&self) -> Paths<'_> {
        if self.paths_borrowed() {
            self.paths_borrowed_twice();
        }
        self.set_paths_borrowed(true);
        Paths { trans: self, table: Paths::table_of(self), took: true }
    }

    #[cold]
    #[inline(never)]
    fn paths_borrowed_twice(&self) -> ! {
        panic!("{}: the path table borrowed twice - the btree code reentered", self.fn_name())
    }
}

/// The path table, for a C export: C's to lend for the call, its own caller
/// holding no reference into it - a Rust caller's borrow, if there is one, idle
/// across the call into C.
impl<'t> Lent<'t, BtreeTrans<'_>> for Paths<'t> {
    unsafe fn lent(trans: &'t BtreeTrans<'_>) -> Self {
        Paths { trans, table: Paths::table_of(trans), took: false }
    }
}

impl Paths<'_> {
    /// Write over the intent path @idx holds at @level, which nodes_locked
    /// already says is write - marked before the wait, for the cycle detector
    /// to see - waited for with @wait, the transaction's own read holds on the
    /// node set aside meanwhile. On failure, six's error: the lock's back to
    /// intent, and the caller marks the level so, after the wait.
    pub(crate) fn lock_write_marked(&self, idx: PathIdx, level: usize, wait: &SixLockWaiter,
                                    should_sleep_fn: impl FnMut() -> Result<(), BchError>, ip: usize)
        -> Result<(), BchError>
    {
        let (node, intent) = self[idx].hold_marked_write(level);
        let readers = self.trans.node_lock_counts(self, None, NonNull::from(node), level).n[SixLockType::SIX_LOCK_read as usize] as i32;

        match unsafe { ManuallyDrop::into_inner(intent).write_waiting_over(readers, wait, should_sleep_fn, ip) } {
            Ok(write) => {
                write.into_raw();
                Ok(())
            }
            Err((intent, e)) => {
                intent.into_raw();
                Err(e)
            }
        }
    }
}

impl Paths<'_> {
    /// @f, a call out that may enter the btree code again - C, through an
    /// export that takes the path table itself, or the btree's public side, as
    /// a walk's body - with this borrow idle across it.
    pub(crate) fn lend<R>(&mut self, f: impl FnOnce() -> R) -> R {
        /*
         * One call of @f: with two, LLVM outlines a large @f rather than
         * inline it twice
         */
        if self.took {
            self.trans.set_paths_borrowed(false);
        }
        let ret = f();
        if self.took {
            assert!(!self.trans.paths_borrowed(), "{}: the path table still borrowed after a call into C",
                    self.trans.fn_name());
            self.trans.set_paths_borrowed(true);
        }
        /* @f may have grown it: */
        self.table = Paths::table_of(self.trans);
        ret
    }
}

impl Drop for Paths<'_> {
    #[inline]
    fn drop(&mut self) {
        if self.took {
            self.trans.set_paths_borrowed(false);
        }
    }
}

impl Deref for Paths<'_> {
    type Target = [BtreePath];

    fn deref(&self) -> &[BtreePath] {
        unsafe { &*self.table }
    }
}

impl DerefMut for Paths<'_> {
    fn deref_mut(&mut self) -> &mut [BtreePath] {
        unsafe { &mut *self.table }
    }
}

impl Index<PathIdx> for Paths<'_> {
    type Output = BtreePath;

    #[inline(always)]
    fn index(&self, idx: PathIdx) -> &BtreePath {
        let paths = &**self;
        invariant!((idx as usize) < paths.len());
        &paths[idx as usize]
    }
}

impl IndexMut<PathIdx> for Paths<'_> {
    #[inline(always)]
    fn index_mut(&mut self, idx: PathIdx) -> &mut BtreePath {
        let paths = &mut **self;
        invariant!((idx as usize) < paths.len());
        &mut paths[idx as usize]
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

    /// Path @idx's node at @level, which it holds, and path @linked's iterator
    /// there: @idx's own, if it's @linked.
    pub(crate) fn node_and_iter_mut(&mut self, idx: PathIdx, level: usize, linked: PathIdx)
        -> Option<(LockedNode<'_, BtreeNode>, &mut KeyIter)>
    {
        if linked == idx {
            return self[idx].locked_level_mut(level);
        }
        let (src, dst) = self.pair(idx, linked);
        Some((src.locked_btree_node(level)?, &mut dst.l[level].iter))
    }

    /// The key of the node a path goes down to next: the transaction's, the
    /// path table's holder's to write.
    pub(crate) fn down_key(&self) -> &BkeyI {
        unsafe { self.trans.btree_path_down.protected() }
    }

    /// Path @idx's level @level, as locked_level_mut(), and the down key: the
    /// node a path goes down from, and the key it goes down by.
    pub(crate) fn level_and_down_key_mut(&mut self, idx: PathIdx, level: usize)
        -> Option<(LockedNode<'_, BtreeNode>, &mut KeyIter, &mut DownKey)>
    {
        let trans = self.trans;
        let (b, iter) = self[idx].locked_level_mut(level)?;
        Some((b, iter, unsafe { trans.btree_path_down.protected_mut() }))
    }

    fn check_live(&self, idx: PathIdx) {
        #[cfg(CONFIG_BCACHEFS_DEBUG)]
        {
            /*
             * Safe to read: the retired array is RCU-freed, so it's still mapped -
             * that readability is exactly what makes this bug class silent.
             */
            crate::warn_once!(self[idx].l[0].b == btree_path_poison(), no_fs,
                              "{}: btree path {:p} outlived trans->paths ({:p})",
                              self.trans.fn_name(), &self[idx], self.trans.paths.get());
        }
        #[cfg(not(CONFIG_BCACHEFS_DEBUG))]
        let _ = idx;
    }

    /// A ref on path @idx put - an intent ref, with @intent: whether it was
    /// the last.
    pub(crate) fn put_ll(&mut self, idx: PathIdx, intent: bool) -> bool {
        self.check_live(idx);

        let t = self.trans;
        ebug_assert!(idx < t.nr_paths.get());
        ebug_assert!(t.path_allocated(idx));
        ebug_assert!(self[idx].ref_ != 0);
        ebug_assert!(self[idx].intent_ref != 0 || !intent);
        #[cfg(CONFIG_BCACHEFS_DEBUG)]
        t.fs().event_trace(events::btree_path_put_ll, |out| {
            prt!(out, "{}: path {:3} ref {} btree {} pos {}",
                 t.fn_name(), idx, self[idx].ref_, self[idx].btree_id(), self[idx].pos);
        });

        let path = &mut self[idx];
        path.intent_ref -= u8::from(intent);
        path.ref_ -= 1;
        if path.ref_ != 0 {
            return false;
        }

        crate::warn_once!(path.intent_ref != 0, no_fs,
                          "path {} released with intent_ref {} - lock will never be dropped",
                          idx, path.intent_ref);
        true
    }
}

/// Where a path table's parts are, in bytes from its start, for @nr paths:
/// the allocated bitmap, then the header's nr_paths right before the paths,
/// then sorted and updates. One block, so that what reads another thread's -
/// the cycle detector, btree_trans_to_text() - has all of it from the
/// one pointer it rcu_dereference()s, trans->paths, stepping back from it as
/// C's trans_paths_nr() and trans_paths_allocated() do; and so that one free
/// retires it. A transaction's first table is embedded in it; past that, one
/// is kvzalloc()ed.
struct TableLayout {
    header:  usize,
    paths:   usize,
    sorted:  usize,
    updates: usize,
    size:    usize,
}

impl TableLayout {
    const fn of(nr: usize) -> Self {
        let header  = bits_to_longs(nr) * size_of::<usize>();
        let paths   = header + offset_of!(BtreeTransPaths, paths);
        let sorted  = paths + nr * size_of::<BtreePath>();
        let updates = sorted + (nr + 4) * size_of::<PathIdx>();
        TableLayout { header, paths, sorted, updates, size: updates + nr * size_of::<BtreeInsertEntry>() }
    }
}

/* the embedded table, laid out as any other */
const _: () = {
    let base = offset_of!(BtreeTrans<'static>, _paths_allocated);
    let l = TableLayout::of(BTREE_ITER_INITIAL as usize);
    assert!(offset_of!(BtreeTrans<'static>, trans_paths) - base == l.header &&
            offset_of!(BtreeTrans<'static>, _paths) - base == l.paths &&
            offset_of!(BtreeTrans<'static>, _sorted) - base == l.sorted &&
            offset_of!(BtreeTrans<'static>, _updates) - base == l.updates);
};

/// The header of the table whose paths start at @paths: trans_paths_nr()'s.
pub(crate) fn table_header(paths: *mut BtreePath) -> *mut BtreeTransPaths {
    paths.wrapping_byte_sub(offset_of!(BtreeTransPaths, paths)).cast()
}

/// The rest of the table whose @nr paths start at @paths: its allocated
/// bitmap - trans_paths_allocated()'s, its start - sorted and updates.
pub(crate) fn table_arrays(paths: *mut BtreePath, nr: usize)
    -> (*mut usize, *mut PathIdx, *mut BtreeInsertEntry)
{
    let l = TableLayout::of(nr);
    let start = paths.cast::<u8>().wrapping_sub(l.paths);
    (start.cast(), start.wrapping_add(l.sorted).cast(), start.wrapping_add(l.updates).cast())
}

impl BtreeTrans<'_> {
    /// A table of @nr paths the transaction's done with, from its allocated
    /// bitmap: freed once readers are done with it, unless it's the embedded
    /// one.
    fn table_retire(&self, allocated: *mut usize, nr: usize) {
        if allocated != self._paths_allocated.as_ptr().cast() {
            // kfree_rcu_mightsleep() is kvfree_rcu_mightsleep()
            unsafe { crate::util::alloc::kfree_rcu_mightsleep(allocated.cast(), TableLayout::of(nr).size) };
        }
    }
}

impl Paths<'_> {
    /// The table at twice the size, what's in it copied over, and the old one
    /// retired. Nothing borrowed from it outlives that - this takes &mut self
    /// - but a pointer C kept into it can.
    pub(crate) fn grow(&mut self) {
        let t = self.trans;
        let (old_paths, old_nr) = (t.paths.get(), t.nr_paths.get() as usize);
        let nr = old_nr * 2;
        let l = TableLayout::of(nr);
        let paths = kvzalloc_nofail(l.size).as_ptr().wrapping_add(l.paths).cast::<BtreePath>();
        let (old_allocated, old_sorted, old_updates) = table_arrays(old_paths, old_nr);
        let (allocated, sorted, updates) = table_arrays(paths, nr);

        unsafe {
            ptr::copy_nonoverlapping(old_allocated, allocated, bits_to_longs(old_nr));
            (*table_header(paths)).nr_paths = nr;
            ptr::copy_nonoverlapping(old_paths, paths, old_nr);
            ptr::copy_nonoverlapping(old_sorted, sorted, t.nr_sorted.get() as usize);
            ptr::copy_nonoverlapping(old_updates, updates, old_nr);
        }

        /*
         * Anything still holding a pointer into the old arrays is now stale,
         * and the RCU grace period means it stays readable for a while yet -
         * so a stale access reads a correct-looking copy and silently diverges
         * instead of faulting. That's the whole reason this bug class has been
         * so quiet: neither KASAN nor valgrind sees anything, because nothing
         * is freed at the point of the bad access.
         *
         * Poison the node pointers so a stale path is instead obviously wrong
         * the moment it's used. IS_ERR() paths already exist all over the
         * iterator code, so this lands as no_btree_node rather than as a wild
         * pointer, and the specific errcode names what happened.
         *
         * Note the first realloc retires trans->_paths, which is embedded in
         * the trans and never freed - so that poison has to be cleared when a
         * trans is initialized, or it shows up in the next transaction's live
         * paths. See trans_paths_init().
         */
        if cfg!(CONFIG_BCACHEFS_DEBUG) {
            for path in self.iter_mut() {
                for level in &mut path.l {
                    level.b = btree_path_poison();
                }
            }
        }

        rcu_assign_pointer(&t.paths, paths);
        t.nr_paths.set(nr as PathIdx);
        self.table = Paths::table_of(t);

        t.table_retire(old_allocated, old_nr);
    }
}

// ── Lock order, and the sorted list ─────────────────────────────────────────

impl<T: PathNode> BtreePath<T> {
    pub(crate) fn cmp_to(&self, btree: Btree, cached: bool, pos: Bpos, level: u8) -> Ordering {
        /*
         * Must match lock ordering as defined by __bch2_btree_node_lock:
         */
        self.btree_id().0.cmp(&btree.0)
            .then(self.cached().cmp(&cached))
            .then(self.pos.cmp(&pos))
            .then(level.cmp(&self.level()))
    }

    pub(crate) fn lock_order(&self, r: &BtreePath<T>) -> Ordering {
        self.cmp_to(r.btree_id(), r.cached(), r.pos, r.level())
    }
}

pub(in crate::btree) struct PathsUpdates<'a, 't> {
    pub(in crate::btree) paths:   &'a mut Paths<'t>,
    pub(in crate::btree) updates: &'a mut [BtreeInsertEntry],
}

impl<'t> Paths<'t> {
    /// The table's allocated bitmap's words and its sorted list's room, where
    /// TableLayout has them for as many paths as there are. Not its updates:
    /// paths_and_updates() lends those out beside the paths.
    fn lists(&self) -> (&[usize], &[PathIdx]) {
        let nr = self.table.len();
        let (allocated, sorted, _) = table_arrays(self.table.cast(), nr);
        unsafe {
            (core::slice::from_raw_parts(allocated.cast(), bits_to_longs(nr)),
             core::slice::from_raw_parts(sorted, nr + 4))
        }
    }

    /// lists(), to edit.
    fn lists_mut(&mut self) -> (&mut [usize], &mut [PathIdx]) {
        let nr = self.table.len();
        let (allocated, sorted, _) = table_arrays(self.table.cast(), nr);
        unsafe {
            (core::slice::from_raw_parts_mut(allocated.cast(), bits_to_longs(nr)),
             core::slice::from_raw_parts_mut(sorted, nr + 4))
        }
    }

    /// All the room for updates: one per path.
    fn updates_buf(&mut self) -> &mut [BtreeInsertEntry] {
        let (_, _, updates) = table_arrays(self.table.cast(), self.table.len());
        unsafe { core::slice::from_raw_parts_mut(updates, self.table.len()) }
    }

    pub(crate) fn sorted(&self) -> &[PathIdx] {
        &self.lists().1[..self.trans.nr_sorted.get() as usize]
    }

    /// All of the array: longer than the list, for list_add() to insert into.
    fn sorted_buf(&mut self) -> &mut [PathIdx] {
        self.lists_mut().1
    }

    /// The key updates queued: trans_for_each_update(). They're in the
    /// table's allocation, after the paths - growing moves them, and the
    /// transaction beginning drops them, both with the table borrowed mutably.
    pub(crate) fn updates(&self) -> &[BtreeInsertEntry] {
        unsafe { self.trans.updates_unguarded() }
    }

    /// updates(), each with its key: in the transaction's memory while it's
    /// queued.
    pub(crate) fn update_keys(&self) -> impl Iterator<Item = (&BtreeInsertEntry, &BkeyI)> {
        self.updates().iter().map(|i| (i, i.key()))
    }

    pub(crate) fn update_mut(&mut self, i: usize) -> &mut BtreeInsertEntry {
        let nr = self.trans.nr_updates.get() as usize;
        &mut self.updates_buf()[..nr][i]
    }

    /// @n queued at @i, the updates from @i on moved up one: there's room for
    /// one per path.
    pub(crate) fn insert_update(&mut self, i: usize, n: BtreeInsertEntry) -> &mut BtreeInsertEntry {
        let t = self.trans;
        let nr = t.nr_updates.get() as usize;
        assert!(i <= nr, "queueing update {i} of {nr}");
        assert!(nr < t.nr_paths.get() as usize, "{nr} updates queued, and {} paths", t.nr_paths.get());
        let updates = self.updates_buf();
        updates.copy_within(i..nr, i + 1);
        updates[i] = n;
        t.nr_updates.set(t.nr_updates.get() + 1);
        self.update_mut(i)
    }

    pub(in crate::btree) fn paths_and_updates(&mut self) -> PathsUpdates<'_, 't> {
        let (_, _, updates) = table_arrays(self.table.cast(), self.table.len());
        let nr = self.trans.nr_updates.get() as usize;
        PathsUpdates { paths: self, updates: unsafe { core::slice::from_raw_parts_mut(updates, nr) } }
    }

    fn allocated_mut(&mut self) -> &mut Bitmap {
        Bitmap::from_words_mut(self.lists_mut().0)
    }

    fn set_allocated(&mut self, idx: PathIdx, allocated: bool) {
        if allocated {
            self.allocated_mut().set(idx as usize);
        } else {
            self.allocated_mut().clear(idx as usize);
        }
    }

    fn sorted_renumber(&mut self, from: usize) {
        for i in from..self.sorted().len() {
            let idx = self.sorted()[i];
            self[idx].sorted_idx = i as PathIdx;
        }
    }

    fn list_remove(&mut self, idx: PathIdx) {
        let (i, nr) = (self[idx].sorted_idx as usize, self.sorted().len());
        ebug_assert!(i < nr);

        self.sorted_buf().copy_within(i + 1..nr, i);
        self.trans.nr_sorted.set(nr as PathIdx - 1);
        self.sorted_renumber(i);
    }

    fn list_add(&mut self, pos: PathIdx, idx: PathIdx) {
        let nr = self.sorted().len();
        let i = if pos != 0 { self[pos].sorted_idx as usize + 1 } else { nr };

        let sorted = self.sorted_buf();
        sorted.copy_within(i..nr, i + 1);
        sorted[i] = idx;
        self.trans.nr_sorted.set(nr as PathIdx + 1);
        self.sorted_renumber(i);

        self.verify_sorted_refs();
    }

    fn prev_sorted(&self, idx: PathIdx) -> Option<PathIdx> {
        let i = self[idx].sorted_idx as usize;
        (i != 0).then(|| self.sorted()[i - 1])
    }

    fn next_sorted(&self, idx: PathIdx) -> Option<PathIdx> {
        let i = self[idx].sorted_idx as usize + 1;
        ebug_assert!(i <= self.sorted().len());
        self.sorted().get(i).copied()
    }

    pub(crate) fn sort(&mut self) {
        if cfg!(CONFIG_BCACHEFS_DEBUG) || !self.trans.paths_sorted() {
            self.sort_now();
        }
    }

    fn sort_now(&mut self) {
        self.verify_sorted_refs();

        if !self.trans.paths_sorted() {
            /*
             * Cocktail shaker sort: this is efficient because iterators will be
             * mostly sorted.
             */
            let (mut l, mut r) = (0, self.sorted().len());
            loop {
                let mut swapped = false;
                for i in l..r.saturating_sub(1) {
                    swapped |= self.sort_swap(i);
                }
                r = r.saturating_sub(1);
                if !swapped {
                    break;
                }

                swapped = false;
                for i in (l..r.saturating_sub(1)).rev() {
                    swapped |= self.sort_swap(i);
                }
                l += 1;
                if !swapped {
                    break;
                }
            }

            self.trans.set_paths_sorted(true);
        }

        self.verify_sorted();
    }

    /// Entries @i and @i + 1 swapped if they're out of order: whether they were.
    fn sort_swap(&mut self, i: usize) -> bool {
        let (a, b) = (self.sorted()[i], self.sorted()[i + 1]);
        let out_of_order = self[a].lock_order(&self[b]).is_gt();
        if out_of_order {
            self.sorted_buf().swap(i, i + 1);
            self[b].sorted_idx = i as PathIdx;
            self[a].sorted_idx = i as PathIdx + 1;
        }
        out_of_order
    }

    fn verify_sorted_refs(&self) {
        if !cfg!(CONFIG_BCACHEFS_DEBUG) {
            return;
        }

        let t = self.trans;
        assert!(self.sorted().len() == t.nr_paths_allocated() - 1);

        for i in t.path_idxs() {
            assert!((self[i].sorted_idx as usize) < self.sorted().len());
            assert!(self.sorted()[self[i].sorted_idx as usize] == i);
        }

        for (i, &idx) in self.sorted().iter().enumerate() {
            assert!(t.path_allocated(idx));
            assert!(self[idx].sorted_idx as usize == i);
        }
    }

    fn verify_sorted(&self) {
        if !(cfg!(CONFIG_BCACHEFS_DEBUG) && debug_check_iterators()) {
            return;
        }

        if self.sorted().windows(2).any(|w| self[w[0]].lock_order(&self[w[1]]).is_gt()) {
            self.trans.dump_paths_updates_as_is(self);
            panic!("trans paths out of order!");
        }
    }

    pub(crate) fn get_ll(&mut self, idx: PathIdx, intent: bool) {
        self.check_live(idx);

        let t = self.trans;
        ebug_assert!(idx < t.nr_paths.get());
        ebug_assert!(t.path_allocated(idx));
        if self[idx].ref_ == u8::MAX {
            t.dump_paths_updates(self);
            panic!("path {idx} refcount overflow");
        }

        let path = &mut self[idx];
        path.ref_ += 1;
        path.intent_ref += u8::from(intent);
        #[cfg(CONFIG_BCACHEFS_DEBUG)]
        t.fs().event_trace(events::btree_path_get_ll, |out| {
            prt!(out, "{}: path {:3} ref {} btree {} pos {}",
                 t.fn_name(), idx, self[idx].ref_, self[idx].btree_id(), self[idx].pos);
        });
    }

    fn copy(&mut self, dst: PathIdx, src: PathIdx) {
        let (src, dst) = self.pair(src, dst);
        dst.pos          = src.pos;
        dst.bits         = src.bits;
        dst.nodes_locked = src.nodes_locked;

        /* do not use memcpy for path->l; we cannot have torn writes to path->l[0].b,
         * and we're guaranteed them since the memcpy starts misaligned */

        for (d, s) in dst.l.iter_mut().zip(&src.l) {
            unsafe { ptr::write_volatile(&raw mut d.b, s.b) };
            d.iter     = s.iter;
            d.lock_seq = s.lock_seq;
            #[cfg(CONFIG_BCACHEFS_LOCK_TIME_STATS)]
            {
                d.lock_taken_time = s.lock_taken_time;
            }
        }

        for i in 0..BTREE_MAX_DEPTH as usize {
            if let Some((_, hold)) = src.hold(i) {
                hold.increment(hold.ty()).into_raw();
            }
        }
    }
}

// ── Getting and putting ─────────────────────────────────────────────────────

impl BtreeTrans<'_> {
    pub(crate) fn path_free(&self, paths: &mut Paths<'_>, idx: PathIdx) {
        self.path_unlock(paths, idx);
        paths.list_remove(idx);
        paths.set_allocated(idx, false);
    }

    pub(crate) fn path_put(&self, paths: &mut Paths<'_>, idx: PathIdx, intent: bool) {
        if !paths.put_ll(idx, intent) {
            return;
        }

        let (preserve, should_be_locked) = (paths[idx].preserve(), paths[idx].should_be_locked());
        let mut dup = None;

        if preserve || should_be_locked {
            let level = paths[idx].level() as usize;
            let is_dup = |paths: &Paths<'_>, d: PathIdx| {
                let (path, d) = (&paths[idx], &paths[d]);
                if preserve {
                    d.lock_order(path).is_eq()
                } else {
                    d.level() == path.level() && d.l[level].b == path.l[level].b
                }
            };

            dup = paths.prev_sorted(idx).filter(|&d| is_dup(paths, d))
                .or_else(|| paths.next_sorted(idx).filter(|&d| is_dup(paths, d)));
            let Some(d) = dup else { return };

            /*
             * If we need this path locked, the duplicate also has te be locked
             * before we free this one:
             */
            if should_be_locked && !paths[d].should_be_locked() && self.restarted().is_none() {
                let relocked = if self.locked() {
                    self.path_relock_norestart(paths, d)
                } else {
                    paths[d].can_relock()
                };
                if !relocked {
                    return;
                }

                paths[d].bits.set_should_be_locked(true);
            }

            ebug_assert!(!(should_be_locked && self.restarted().is_none() && self.locked() &&
                           !paths[d].locked(paths[d].level() as usize)));

            paths[idx].bits.set_should_be_locked(false);
            let preserve = paths[d].preserve() | preserve;
            paths[d].bits.set_preserve(preserve);
        }

        self.fs().event_trace(events::btree_path_free, |out| {
            prt!(out, "{}: dup {}\n", self.fn_name(), dup.unwrap_or(0));
            self.path_to_text_short(out, paths, idx);
        });
        self.path_free(paths, idx);
    }

    fn path_alloc(&self, paths: &mut Paths<'_>, pos: PathIdx) -> PathIdx {
        /* trans->nr_paths is always a multiple of BITS_PER_LONG, we can improve
         * over find_first_zero_bit */
        let words = self.nr_paths.get() as usize / usize::BITS as usize;
        for w in 0..words {
            let word = self.paths_allocated().0.words()[w];
            if !word != 0 {
                let idx = (w * usize::BITS as usize + (!word).trailing_zeros() as usize) as PathIdx;

                /*
                 * Do this before marking the new path as allocated, since it won't be
                 * initialized yet:
                 */
                if idx > self.nr_paths_max.get() {
                    self.update_max_paths(paths);
                }

                paths.set_allocated(idx, true);
                self.path_init(paths, pos, idx);
                return idx;
            }
        }

        self.paths_realloc(paths, pos)
    }

    #[inline(never)]
    fn paths_realloc(&self, paths: &mut Paths<'_>, pos: PathIdx) -> PathIdx {
        if self.nr_paths.get() as u32 == BTREE_ITER_MAX {
            self.path_overflow(paths);
            return 0;
        }

        let idx = self.nr_paths.get();
        paths.grow();

        /*
         * Do this before marking the new path as allocated, since it won't be
         * initialized yet:
         */
        if idx > self.nr_paths_max.get() {
            self.update_max_paths(paths);
        }

        paths.set_allocated(idx, true);
        self.path_init(paths, pos, idx);
        idx
    }

    fn path_init(&self, paths: &mut Paths<'_>, pos: PathIdx, idx: PathIdx) {
        let path = &mut paths[idx];
        path.ref_         = 0;
        path.intent_ref   = 0;
        path.nodes_locked = 0;

        paths.list_add(pos, idx);
        self.set_paths_sorted(false);
    }

    #[cold]
    #[inline(never)]
    fn path_overflow(&self, paths: &mut Paths<'_>) {
        self.dump_paths_updates(paths);
        crate::bch_err!(self.fs(), "trans path overflow");
    }

    fn path_clone(&self, paths: &mut Paths<'_>, src: PathIdx, intent: bool, ip: usize) -> PathIdx {
        let new = self.path_alloc(paths, src);
        paths.copy(new, src);
        paths.get_ll(new, intent);
        #[cfg(any(CONFIG_BCACHEFS_LOCK_TIME_STATS, CONFIG_BCACHEFS_DEBUG))]
        {
            paths[new].ip_allocated = ip;
        }
        #[cfg(not(any(CONFIG_BCACHEFS_LOCK_TIME_STATS, CONFIG_BCACHEFS_DEBUG)))]
        let _ = ip;
        new
    }

    pub(crate) fn path_make_mut(&self, paths: &mut Paths<'_>, idx: PathIdx, intent: bool, ip: usize) -> PathIdx {
        let idx = if paths[idx].ref_ > 1 || paths[idx].preserve() {
            self.path_make_mut_clone(paths, idx, intent, ip)
        } else {
            idx
        };
        paths[idx].bits.set_should_be_locked(false);
        idx
    }

    pub(crate) fn path_make_mut_clone(&self, paths: &mut Paths<'_>, old: PathIdx, intent: bool, ip: usize)
        -> PathIdx
    {
        paths.put_ll(old, intent);
        let new = self.path_clone(paths, old, intent, ip);

        self.fs().event_trace(events::btree_path_clone, |out| {
            prt!(out, "{}: old {}\n", self.fn_name(), old);
            self.path_to_text_short(out, paths, new);
        });

        paths[new].bits.set_preserve(false);
        new
    }

    #[inline]
    pub(crate) fn path_set_pos(&self, paths: &mut Paths<'_>, idx: PathIdx, new_pos: Bpos, intent: bool,
                               ip: usize) -> PathIdx {
        if new_pos == paths[idx].pos {
            idx
        } else {
            self.__path_set_pos(paths, idx, new_pos, intent, ip)
        }
    }

    #[inline(never)]
    fn __path_set_pos(&self, paths: &mut Paths<'_>, idx: PathIdx, new_pos: Bpos, intent: bool,
                      ip: usize) -> PathIdx {
        if !<events::btree_path_set_pos as TraceEvent>::trace_enabled() {
            return self.path_set_pos_notrace(paths, idx, new_pos, intent, ip);
        }

        let mut out = Printbuf::new();
        out.indent_add_nextline(2);
        prt!(&mut out, "{} caller: ", self.fn_name());
        prt_symbol(&mut out, ip);
        prt!(&mut out, "\n\n");
        self.path_to_text(&mut out, paths, idx);

        let idx = self.path_set_pos_notrace(paths, idx, new_pos, intent, ip);

        prt!(&mut out, "\n");
        self.path_to_text(&mut out, paths, idx);
        prt!(&mut out, "\n");

        <events::btree_path_set_pos as TraceEvent>::trace(self.fs(), &out);
        idx
    }

    fn path_set_pos_notrace(&self, paths: &mut Paths<'_>, idx: PathIdx, new_pos: Bpos, intent: bool,
                            ip: usize) -> PathIdx {
        let cmp = new_pos.cmp(&paths[idx].pos);

        self.verify_not_unlocked_or_in_restart();
        ebug_assert!(paths[idx].ref_ != 0);

        let idx = self.path_make_mut(paths, idx, intent, ip);

        paths[idx].pos.assign_fieldwise_fixme_rustc(&new_pos);
        self.set_paths_sorted(false);

        if paths[idx].cached() {
            self.node_unlock(paths, idx, 0);
            paths[idx].l[0].b = NodePtr::err(Errcode::no_btree_node_up.into());
        } else {
            let level = self.path_up_until_good_node(paths, idx, cmp);

            if !paths[idx].node(level).is_null() {
                ebug_assert!(paths[idx].locked(level));

                let (b, iter) = paths[idx].locked_level_mut(level).expect("a good node is locked");
                let keys = b.keys(b.contents());

                /*
                 * We might have to skip over many keys, or just a few: try
                 * advancing the node iterator, and if we have to skip over too
                 * many keys just reinit it (or if we're rewinding, since that
                 * is expensive).
                 */
                if cmp.is_lt() || !advance_to_pos(iter, &keys, &new_pos, 8) {
                    iter.init(self.fs(), &keys, &new_pos);
                }

                /*
                 * Iterators to interior nodes should always be pointed at the first non
                 * whiteout:
                 */
                if level != 0 {
                    iter.peek(&keys);
                }
            }

            if level != paths[idx].level() as usize {
                self.path_unlock(paths, idx);
            }
        }

        self.path_verify(paths, idx);
        idx
    }

    pub(crate) fn path_get(&self, paths: &mut Paths<'_>, btree: Btree, pos: Bpos, mut locks_want: u32, level: u32,
                           flags: BtreeIterFlags, ip: usize) -> PathIdx {
        let cached = flags.contains(BtreeIterFlags::CACHED);
        let intent = flags.contains(BtreeIterFlags::INTENT);

        self.verify_not_unlocked_or_in_restart();
        self.verify_locks(paths);

        paths.sort();

        if intent {
            locks_want = locks_want.max(level + 1);
        }
        locks_want = locks_want.min(BTREE_MAX_DEPTH);

        let mut path_pos = 0;
        for i in 0..paths.sorted().len() {
            let idx = paths.sorted()[i];
            if paths[idx].cmp_to(btree, cached, pos, level as u8).is_gt() {
                break;
            }
            path_pos = idx;
        }

        let reuse = path_pos != 0 && {
            let path = &paths[path_pos];
            path.cached() == cached && path.btree_id() == btree && path.level() as u32 == level
        } && (locks_want <= paths[path_pos].locks_want() as u32 ||
              self.path_upgrade_norestart(paths, path_pos, locks_want as usize));

        let idx = if reuse {
            self.fs().event_trace(events::btree_path_get, |out| {
                prt!(out, "{}\n", self.fn_name());
                self.path_to_text_short(out, paths, path_pos);
                prt!(out, "new pos: {}", pos);
            });

            paths.get_ll(path_pos, intent);
            self.path_set_pos(paths, path_pos, pos, intent, ip)
        } else {
            let idx = self.path_alloc(paths, path_pos);
            paths.get_ll(idx, intent);

            let path = &mut paths[idx];
            path.pos = pos;
            path.bits.set_btree_id(btree);
            path.bits.set_cached(cached);
            path.bits.set_should_be_locked(false);
            path.bits.set_level(level as u8);
            path.bits.set_locks_want(locks_want as u8);
            path.nodes_locked = 0;
            for l in &mut path.l {
                l.b = NodePtr::err(Errcode::no_btree_node_init.into());
            }
            #[cfg(any(CONFIG_BCACHEFS_LOCK_TIME_STATS, CONFIG_BCACHEFS_DEBUG))]
            {
                path.ip_allocated = ip;
            }
            self.set_paths_sorted(false);

            self.fs().event_trace(events::btree_path_alloc, |out| {
                prt!(out, "{}\n", self.fn_name());
                self.path_to_text_short(out, paths, idx);
            });
            idx
        };

        if !flags.contains(BtreeIterFlags::NOPRESERVE) {
            paths[idx].bits.set_preserve(true);
        }

        /*
         * If the path has locks_want greater than requested, we don't downgrade
         * it here - on transaction restart because btree node split needs to
         * upgrade locks, we might be putting/getting the iterator again.
         * Downgrading iterators only happens via bch2_trans_downgrade(), after
         * a successful transaction commit.
         */

        idx
    }

    pub(crate) fn path_get_unlocked_mut(&self, paths: &mut Paths<'_>, btree: Btree, level: u32, pos: Bpos,
                                        cached: bool, ip: usize) -> PathIdx {
        let flags = BtreeIterFlags::NOPRESERVE | BtreeIterFlags::INTENT |
            if cached { BtreeIterFlags::CACHED } else { BtreeIterFlags::empty() };
        let idx = self.path_get(paths, btree, pos, level + 1, level, flags, ip);
        let idx = self.path_make_mut(paths, idx, true, ip);

        self.path_downgrade(paths, idx);
        self.path_unlock(paths, idx);
        idx
    }
}

impl BtreePath {
    fn can_relock(&self) -> bool {
        let mut l = self.level() as usize;

        loop {
            if self.node(l).is_null() {
                break;
            }

            if !self.is_btree_node(l) {
                return false;
            }

            if !self.lock_seq_matches(l) {
                return false;
            }

            l += 1;
            if l >= self.locks_want() as usize {
                break;
            }
        }

        true
    }

    /// The level's node, if it's a btree node the path holds locked.
    #[inline(always)]
    pub(crate) fn locked_btree_node(&self, level: usize) -> Option<LockedNode<'_, BtreeNode>> {
        self.downcast::<BtreeNode>()?.locked_node(level)
    }

    /// locked_btree_node()'s node, for as long as its filesystem: a path's
    /// nodes are the cache's, which it never frees while it runs.
    pub(crate) fn cached_btree_node<'f>(&self, fs: &'f Fs, level: usize) -> Option<&'f BtreeNode> {
        Some(BtreeNodeRef::from(self.locked_btree_node(level)?).node(fs))
    }

    /// Its key cache entry, if it's a cached path holding it locked.
    #[inline(always)]
    pub(crate) fn locked_key_cached(&self) -> Option<LockedNode<'_, BkeyCached>> {
        self.downcast::<BkeyCached>()?.locked_node(0)
    }

    /// The level's node, to write: a btree node the path holds write locked.
    #[inline(always)]
    pub(crate) fn locked_btree_node_mut(&mut self, level: usize) -> Option<LockedNode<'_, BtreeNode, Write>> {
        self.locked_node_mut(level)
    }

    /// Its key cache entry, to write: locked_btree_node_mut()'s, of a cached
    /// path.
    #[inline(always)]
    pub(crate) fn locked_key_cached_mut(&mut self) -> Option<LockedNode<'_, BkeyCached, Write>> {
        self.locked_node_mut(0)
    }

    /// The level, held for intent, write locked too if no one holds it for
    /// read: whether it was. It's marked write locked first - the cycle
    /// detector has to see that before the lock's taken - and left so if the
    /// lock wasn't had: node_lock_write_at()'s, which waits for it.
    #[inline(always)]
    pub(crate) fn try_lock_write(&mut self, level: usize, ip: usize) -> bool {
        assert!(self.locked(level), "write locking a node the path doesn't hold");
        ebug_assert!(self.intent_locked(level));
        self.mark_locked_noreset(level, BTREE_NODE_WRITE_LOCKED);

        let (node, _) = self.hold_marked_write(level);
        ebug_assert!(self.l[level].lock_seq == node.common().lock.six().seq.get());

        six_trylock_ip(node.common().lock.six(), SixLockType::SIX_LOCK_write, ip)
    }

    /// The level, held for intent, write locked too if that's had without
    /// waiting: its node's Write view. Marked write locked after - nothing
    /// waits on it, for the cycle detector to see.
    pub(crate) fn trylock_write<T: PathNode>(&mut self, level: usize, ip: usize) -> Option<LockedNode<'_, T, Write>> {
        assert!(self.locked_type(level) == BTREE_NODE_INTENT_LOCKED,
                "write locking a node the path doesn't hold for intent only");
        let node = self.downcast::<T>().expect("write locking a node as another type").locked_node(level)?.node();
        if !six_trylock_ip(node.common().lock.six(), SixLockType::SIX_LOCK_write, ip) {
            return None;
        }

        self.mark_locked_noreset(level, BTREE_NODE_WRITE_LOCKED);
        self.locked_node_mut(level)
    }

    #[inline(always)]
    fn locked_node_mut<T: PathNode>(&mut self, level: usize) -> Option<LockedNode<'_, T, Write>> {
        let node = self.downcast::<T>()?.locked_node(level)?.node();
        (self.locked_type(level) == BTREE_NODE_WRITE_LOCKED).then(|| LockedNode { node, _mode: PhantomData })
    }

    /// The level's locked node, and its iterator to move: the node isn't the
    /// path's memory, and with only the iterator borrowed it stays locked.
    #[inline(always)]
    pub(crate) fn locked_level_mut(&mut self, level: usize) -> Option<(LockedNode<'_, BtreeNode>, &mut KeyIter)> {
        self.locked_level_as(level)
    }

    /// locked_level_mut(), of a node the path holds write locked: its Write
    /// view.
    #[inline(always)]
    pub(crate) fn locked_level_write(&mut self, level: usize)
        -> Option<(LockedNode<'_, BtreeNode, Write>, &mut KeyIter)>
    {
        self.locked_level_as(level)
    }

    #[inline(always)]
    fn locked_level_as<M: NodeMode>(&mut self, level: usize) -> Option<(LockedNode<'_, BtreeNode, M>, &mut KeyIter)> {
        if !M::held_by(self.locked_type(level)) {
            return None;
        }
        let b = ptr::from_ref(self.locked_btree_node(level)?.node());
        Some((LockedNode { node: unsafe { &*b }, _mode: PhantomData }, &mut self.l[level].iter))
    }

    pub(crate) fn peek_slot(&self, u: &mut Bkey) -> Option<KeyVal> {
        let level = self.level() as usize;

        if self.l[level].b.is_null() {
            return None;
        }

        ebug_assert!(self.locked(level));

        if !self.cached() {
            let b = self.locked_btree_node(level).expect("a path's slot is in its locked node");
            let keys = b.keys(b.contents());

            if let Some(k) = self.l[level].iter.peek_all() {
                let k = keys.disassemble(k, u);
                let (ty, p) = (k.k.key_type(), k.k.p);

                ebug_assert!(!(ty == BkeyType::deleted && p == self.pos));

                if p == self.pos {
                    return Some(KeyVal(k.v));
                }
            }

            *u = Bkey::new();
            u.p = self.pos;
            Some(KeyVal(ptr::null()))
        } else {
            let ck = self.downcast::<BkeyCached>()?.locked_node(0)?;

            ebug_assert!({ let key = ck.key.get(); Btree(key.btree_id) == self.btree_id() && key.pos == self.pos });

            let k = ck.contents().key();
            *u = k.k;
            Some(KeyVal((&raw const k.v).cast()))
        }
    }

    #[inline(always)]
    pub(crate) fn pos_in_node(&self, b: LockedNode<'_, BtreeNode>) -> bool {
        u32::from(b.c.btree_id.get()) == self.btree_id().0 && !self.pos_before_node(b) && !self.pos_after_node(b)
    }

    #[inline(always)]
    pub(crate) fn pos_before_node(&self, b: LockedNode<'_, BtreeNode>) -> bool {
        self.pos < b.contents().data().min_key
    }

    #[inline(always)]
    pub(crate) fn pos_after_node(&self, b: LockedNode<'_, BtreeNode>) -> bool {
        self.pos > b.key().k.p
    }
}

// ── To text ─────────────────────────────────────────────────────────────────

fn btree_node_locked_str(t: BtreeNodeLockedType) -> &'static str {
    match t {
        BTREE_NODE_UNLOCKED      => "unlocked",
        BTREE_NODE_READ_LOCKED   => "read",
        BTREE_NODE_INTENT_LOCKED => "intent",
        BTREE_NODE_WRITE_LOCKED  => "write",
    }
}

impl BtreePath {
    pub(crate) fn to_text_short(&self, out: &mut Printbuf, idx: PathIdx) {
        prt!(out, "path: idx {:3} ref {}:{} {} {} {} ", idx, self.ref_, self.intent_ref,
             if self.preserve() { 'P' } else { ' ' },
             if self.should_be_locked() { 'S' } else { ' ' },
             if self.cached() { 'C' } else { 'B' });
        btree_id_level_to_text(out, self.btree_id(), self.level().into());
        prt!(out, " pos {}", self.pos);

        if let Some(b) = self.locked_btree_node(self.level() as usize) {
            prt!(out, " {}-{}", b.contents().data().min_key, b.key().k.p);
        }

        #[cfg(any(CONFIG_BCACHEFS_LOCK_TIME_STATS, CONFIG_BCACHEFS_DEBUG))]
        {
            prt!(out, " ");
            prt_symbol(out, self.ip_allocated);
        }
    }

    pub(crate) fn to_text(&self, out: &mut Printbuf, idx: PathIdx) {
        self.to_text_short(out, idx);
        prt!(out, "\n\nlocks_want {}\n", self.locks_want());

        let out = &mut *out.indent(2);
        for l in 0..BTREE_MAX_DEPTH as usize {
            prt!(out, "l={} locks {} seq {} node ", l, btree_node_locked_str(self.locked_type(l)), self.l[l].lock_seq);
            match self.l[l].b.get() {
                Err(e)  => prt!(out, "{}", e.msg()),
                Ok(b)   => prt!(out, "{:p}", b.map_or(ptr::null_mut(), NonNull::as_ptr)),
            }
            prt!(out, "\n");
        }
    }
}

impl BtreeTrans<'_> {
    pub(crate) fn path_to_text_short(&self, out: &mut Printbuf, paths: &Paths<'_>, idx: PathIdx) {
        paths[idx].to_text_short(out, idx)
    }

    pub(crate) fn path_to_text(&self, out: &mut Printbuf, paths: &Paths<'_>, idx: PathIdx) {
        paths[idx].to_text(out, idx)
    }

    fn paths_to_text_as_is(&self, out: &mut Printbuf, paths: &Paths<'_>) {
        for &idx in paths.sorted() {
            paths[idx].to_text_short(out, idx);
            prt!(out, "\n");
        }
    }

    pub(crate) fn paths_to_text(&self, out: &mut Printbuf, paths: &mut Paths<'_>) {
        paths.sort();
        self.paths_to_text_as_is(out, paths);
    }

    fn dump_paths_updates_as_is(&self, paths: &Paths<'_>) {
        let mut msg = LogMsg::new(self.fs());
        self.paths_to_text_as_is(&mut msg, paths);
        self.updates_to_text(&mut msg);
    }

    pub(crate) fn dump_paths_updates(&self, paths: &mut Paths<'_>) {
        paths.sort();
        self.dump_paths_updates_as_is(paths);
    }

    #[cold]
    #[inline(never)]
    fn update_max_paths(&self, paths: &mut Paths<'_>) {
        let s = &self.fs().btree.trans.stats[self.fn_idx.get() as usize];
        let nr = self.nr_paths_allocated();
        let mut buf = Printbuf::new();

        self.paths_to_text(&mut buf, paths);

        if !buf.allocation_failure() {
            let mut locked = s.lock.lock();
            if nr as u32 > s.lock.nr_max_paths.get() {
                locked.published().nr_max_paths.set(nr as u32);
                core::mem::swap(&mut locked.max_paths_text, &mut buf);
            }
        }

        self.nr_paths_max.set(nr as PathIdx);
    }
}

// ── Debug ───────────────────────────────────────────────────────────────────

impl BtreeTrans<'_> {
    pub(crate) fn verify_iterators(&self) -> bool {
        cfg!(CONFIG_BCACHEFS_DEBUG) && debug_check_iterators()
    }

    pub(crate) fn path_verify(&self, paths: &mut Paths<'_>, idx: PathIdx) {
        if self.verify_iterators() {
            self.path_verify_now(paths, idx);
        }
    }

    pub(crate) fn verify_paths(&self, paths: &mut Paths<'_>) {
        if self.verify_iterators() {
            self.verify_paths_now(paths);
        }
    }

    fn verify_paths_now(&self, paths: &mut Paths<'_>) {
        for idx in self.path_idxs() {
            self.path_verify_now(paths, idx);
        }
    }

    pub(crate) fn path_verify_level(&self, paths: &mut Paths<'_>, idx: PathIdx, level: usize) {
        if self.verify_iterators() {
            self.path_verify_level_now(paths, idx, level);
        }
    }

    pub(crate) fn path_verify_now(&self, paths: &mut Paths<'_>, idx: PathIdx) {
        let levels = if !paths[idx].cached() { BTREE_MAX_DEPTH as usize } else { 1 };

        for i in 0..levels {
            let path = &paths[idx];
            assert!(!(i < path.level() as usize && path.l[i].b.get().is_ok()));

            if path.l[i].b.is_null() {
                assert!(!(!path.cached() &&
                          self.fs().btree_id_root(path.btree_id().0).is_some_and(|r| r.c.level.get() as usize > i)));
                break;
            }

            self.path_verify_level_now(paths, idx, i);
        }

        self.verify_path_locks(paths, idx);
    }

    fn path_verify_cached(&self, paths: &mut Paths<'_>, idx: PathIdx) {
        let locked = paths[idx].locked(0);

        if !self.node_relock(paths, idx, 0) {
            return;
        }

        let path = &paths[idx];
        let key = path.locked_key_cached().expect("a relocked key cache path's entry").key.get();
        assert!(Btree(key.btree_id) == path.btree_id() && key.pos == path.pos);

        if !locked {
            self.node_unlock(paths, idx, 0);
        }
    }

    fn path_verify_level_now(&self, paths: &mut Paths<'_>, idx: PathIdx, level: usize) {
        let locked = paths[idx].locked(level);

        if paths[idx].cached() {
            if level == 0 {
                self.path_verify_cached(paths, idx);
            }
            return;
        }

        if paths[idx].node(level).is_null() {
            return;
        }

        if !self.node_relock_notrace(paths, idx, level) {
            return;
        }

        let path = &paths[idx];
        let b = path.locked_btree_node(level).expect("a relocked level is locked");

        /* had under READ_ERRORS: nothing read to check against */
        if b.flags.read_error() {
            if !locked {
                self.node_unlock(paths, idx, level);
            }
            return;
        }

        if !path.pos_in_node(b) {
            let mut buf = Printbuf::new();
            prt!(&mut buf, "level {} path->pos {}\n{}", level, path.pos, b.node().pos_to_text(self.fs()));
            panic!("{}", buf.as_str());
        }

        let keys = b.keys(b.contents());
        let iter = &path.l[level].iter;
        iter.verify(&keys);

        /* For interior nodes, the iterator may have skipped past deleted keys: */
        let mut tmp = *iter;
        let p = if level != 0 { tmp.prev(&keys) } else { tmp.prev_all(&keys) };
        let mut tmp = *iter;
        let k = if level != 0 { tmp.peek(&keys) } else { tmp.peek_all() };

        if !(level > path.level() as usize && self.journal_replay_not_finished()) {
            /*
             * We can't run these checks for interior nodes when we're still
             * using the journal overlay because there might be a key in
             * the interior node that points midway through the current leaf
             * node - which is deleted in the journal overlay, but set_pos()
             * will skip past it and cause the interior node iterators to be
             * inconsistent in a way that doesn't matter and it can't check
             * for.
             */

            if p.is_some_and(|p| keys.bkey_iter_pos_cmp(p, &path.pos).is_ge()) {
                path_verify_level_err(path, level, &keys, p, k, "before");
            }

            if k.is_some_and(|k| keys.bkey_iter_pos_cmp(k, &path.pos).is_lt()) {
                path_verify_level_err(path, level, &keys, p, k, "after");
            }
        }

        if !locked {
            self.node_unlock(paths, idx, level);
        }
    }

    pub(crate) fn assert_pos_locked_now(&self, paths: &mut Paths<'_>, btree: Btree, pos: Bpos) {
        self.verify_not_unlocked_or_in_restart();

        paths.sort();

        for &idx in paths.sorted() {
            let path = &paths[idx];
            if path.btree_id() != btree || !path.locked(0) || !path.should_be_locked() {
                continue;
            }

            if let Some(b) = path.locked_btree_node(0) {
                if bkey_ge(pos, b.contents().data().min_key) && bkey_le(pos, b.key().k.p) {
                    return;
                }
            } else if bkey_eq(pos, path.pos) {
                return;
            }
        }

        self.dump_paths_updates(paths);
        panic!("not locked: {} {}", btree, pos);
    }
}

#[cold]
fn path_verify_level_err(path: &BtreePath, level: usize, keys: &Keys<'_>, p: Option<u16>, k: Option<u16>,
                         msg: &str) -> ! {
    let mut buf = Printbuf::new();
    prt!(&mut buf, "path should be {} key at level {}", msg, level);
    prt!(&mut buf, "\npath pos {}", path.pos);

    let key = |buf: &mut Printbuf, k: Option<u16>| match k {
        Some(k) => prt!(buf, "{}", keys.key_at(k).unpack(keys.contents())),
        None    => prt!(buf, "(none)"),
    };
    prt!(&mut buf, "\nprev key ");
    key(&mut buf, p);
    prt!(&mut buf, "\ncur  key ");
    key(&mut buf, k);

    panic!("{}", buf.as_str());
}

/// A transaction's path table, as it starts: its own, embedded.
pub fn trans_paths_init(trans: &BtreeTrans<'_>) {
    trans.nr_paths.set(BTREE_ITER_INITIAL as PathIdx);
    trans.paths.set(trans._paths.as_ptr().cast());
    unsafe { (*trans.trans_paths.as_ptr()).nr_paths = BTREE_ITER_INITIAL as usize };
    let mut paths = trans.paths();

    /*
     * If this trans previously grew out of _paths, grow() poisoned it on the
     * way out - and _paths is embedded in the trans, so it isn't freed and
     * the poison outlives the transaction that retired it. A trans off the
     * percpu cache would then start with poison sitting in paths that are
     * live for this transaction. Clear it here rather than skip poisoning
     * _paths: growing out of _paths is the common realloc, so it's where
     * stale pointers most want catching.
     */
    if cfg!(CONFIG_BCACHEFS_DEBUG) {
        for path in paths.iter_mut() {
            for level in &mut path.l {
                level.b = NodePtr::null();
            }
        }
    }

    /* Reserve path 0 for our sentinal value */
    paths.lists_mut().0[0] = 1;
}

pub fn trans_paths_exit(trans: &BtreeTrans<'_>) {
    let nr = trans.nr_paths.get().into();
    let (allocated, _, _) = table_arrays(trans.paths.replace(ptr::null_mut()), nr);
    trans.table_retire(allocated, nr);
}

pub fn btree_path_to_text(out: &mut Printbuf, _trans: &BtreeTrans<'_>, path_idx: PathIdx, path: &BtreePath) {
    path.to_text(out, path_idx)
}

/// What a path's levels point at: a btree node, or - a cached path's - a key
/// cache entry, each starting with a BtreeBkeyCachedCommon. The common part
/// is one too, for code over any path: relocking, the cycle detector.
///
/// # Safety
/// Self starts with a BtreeBkeyCachedCommon, and a common part whose cached
/// flag is CACHED is the start of one. It's freed only by what's locked it for
/// intent and write: a hold on its lock keeps it.
pub unsafe trait PathNode: 'static {
    /// The cached flag of a common part that's one of these; None for any.
    const CACHED: Option<bool>;

    fn common(&self) -> &BtreeBkeyCachedCommon;

    /// What a common part is the start of, if it's one of these: by its
    /// cached flag.
    fn from_common(b: &BtreeBkeyCachedCommon) -> Option<&Self> where Self: Sized {
        Self::CACHED.map_or(true, |cached| cached == b.cached)
            .then(|| unsafe { &*core::ptr::from_ref(b).cast() })
    }
}

unsafe impl PathNode for BtreeBkeyCachedCommon {
    const CACHED: Option<bool> = None;

    fn common(&self) -> &BtreeBkeyCachedCommon {
        self
    }
}

unsafe impl PathNode for BtreeNode {
    const CACHED: Option<bool> = Some(false);

    fn common(&self) -> &BtreeBkeyCachedCommon {
        &self.c
    }
}

unsafe impl PathNode for BkeyCached {
    const CACHED: Option<bool> = Some(true);

    fn common(&self) -> &BtreeBkeyCachedCommon {
        &self.c
    }
}

const _: () = assert!(offset_of!(BtreeNode, c) == 0 && offset_of!(BkeyCached, c) == 0 &&
                      offset_of!(BtreeBkeyCachedCommon, lock) == 0);

impl BtreePath {
    /// As a path of Ts, if it is one: by its cached flag.
    pub fn downcast<T: PathNode>(&self) -> Option<&BtreePath<T>> {
        T::CACHED.map_or(true, |cached| cached == self.bits.cached())
            .then(|| unsafe { &*core::ptr::from_ref(self).cast() })
    }
}

/// A PathNode whose lock protects only part of it: the part only a hold on the
/// lock reaches.
pub trait PathNodeContents: PathNode + Sized {
    type Contents;
    type Published;

    fn locked_contents(&self) -> &LockedBy<Self::Contents, Self::Published, NodeLock<Self>>;
}

/// The lock a node's contents are under, as LockedBy names it: the node's
/// own, its common part's six lock - what a LockedNode of it holds.
pub struct NodeLock<T>(PhantomData<T>);

impl PathNodeContents for BtreeNode {
    type Contents = NodeContents;
    type Published = PublishedKey;

    fn locked_contents(&self) -> &LockedBy<NodeContents, PublishedKey, NodeLock<Self>> {
        &self.contents
    }
}

impl PathNodeContents for BkeyCached {
    type Contents = EntryContents;
    type Published = ();

    fn locked_contents(&self) -> &LockedBy<EntryContents, (), NodeLock<Self>> {
        &self.contents
    }
}

// a LockedNode is had only while its node's lock is held, as its mode says
impl<T: PathNodeContents, M> Guard for LockedNode<'_, T, M> {
    type Name = NodeLock<T>;
    type Root = T;

    fn root(&self) -> &T {
        self.node
    }
}

impl<T: PathNodeContents> Alone for LockedNode<'_, T, Write> {}

/// How a LockedNode's node is held: Held any way, its contents had by &; or
/// for Write, its contents by &mut.
pub struct Held;
pub struct Write;

/// A LockedNode mode, by what a path level has to hold for it.
trait NodeMode {
    fn held_by(locked: BtreeNodeLockedType) -> bool;
}

impl NodeMode for Held {
    fn held_by(locked: BtreeNodeLockedType) -> bool {
        locked != BTREE_NODE_UNLOCKED
    }
}

impl NodeMode for Write {
    fn held_by(locked: BtreeNodeLockedType) -> bool {
        locked == BTREE_NODE_WRITE_LOCKED
    }
}

/// A node a path has locked, borrowed from the path table - or from the hold on
/// its lock, before it's a path's - which keeps it locked while it's had.
// XXX: node() is the view's lifetime, though a btree node lives as long as its
// Fs: with the Fs's lifetime there, a view wouldn't be needed for the node's
// identity and flags alone (the key cache's flush and drop, node_evict())
pub struct LockedNode<'p, T: PathNode, M = Held> {
    node:  &'p T,
    _mode: PhantomData<M>,
}
copy!([T: PathNode] LockedNode<'_, T>);

impl<'p, T: PathNode> LockedNode<'p, T> {
    /// @node, by @hold on its lock.
    pub fn of_hold(node: &'p T, hold: &'p SixHold<'_>) -> Self {
        assert!(ptr::eq(hold.lock().six(), node.common().lock.six()), "a LockedNode by a hold on another node's lock");
        LockedNode { node, _mode: PhantomData }
    }

    pub fn contents(&self) -> &'p T::Contents
    where
        T: PathNodeContents,
    {
        unsafe { self.node.locked_contents().protected() }
    }
}

impl<'p, T: PathNode> LockedNode<'p, T, Write> {
    /// # Safety
    /// @node is locked for write, for 'p, and nothing else has it as Write.
    pub unsafe fn new_write(node: &'p T) -> Self {
        LockedNode { node, _mode: PhantomData }
    }

    pub fn contents(&self) -> &T::Contents
    where
        T: PathNodeContents,
    {
        self.get_of(self.node.locked_contents())
    }

    pub fn contents_mut(&mut self) -> &mut T::Contents
    where
        T: PathNodeContents,
    {
        let node = self.node;
        self.get_mut_of(node.locked_contents())
    }
}

impl<'p> LockedNode<'p, BtreeNode, Write> {
    pub(crate) fn of_owned(n: &'p mut OwnedNode) -> Self {
        LockedNode { node: n, _mode: PhantomData }
    }
}

impl<'p> LockedNode<'p, BtreeNode> {
    pub(crate) fn of_owned_held(n: &'p OwnedNode) -> Self {
        LockedNode { node: n, _mode: PhantomData }
    }
}

impl<'p> LockedNode<'p, BtreeNode> {
    pub fn key(&self) -> &'p BkeyI {
        self.node.key(self.contents())
    }
}

impl LockedNode<'_, BtreeNode, Write> {
    pub fn keys_mut(&mut self) -> KeysMut<'_> {
        let b = self.node;
        b.keys_mut(self.contents_mut())
    }

    pub fn key(&self) -> &BkeyI {
        self.node.key(self.contents())
    }

    pub fn set_key(&mut self, k: impl BkeyV) {
        let b = self.node;
        b.set_key(self.contents_mut(), k)
    }

    pub fn update_key<R>(&mut self, f: impl FnOnce(&mut NodeKey) -> R) -> R {
        let b = self.node;
        b.update_key(self.contents_mut(), f)
    }
}

impl<'p, T: PathNode, M> LockedNode<'p, T, M> {
    pub fn node(&self) -> &'p T {
        self.node
    }

    /// Held, for as long as this is borrowed.
    pub fn held(&self) -> LockedNode<'_, T> {
        LockedNode { node: self.node, _mode: PhantomData }
    }
}

impl<T: PathNode, M> Deref for LockedNode<'_, T, M> {
    type Target = T;

    fn deref(&self) -> &T {
        self.node
    }
}

/// A path level's node, its lock not had through this: see unlocked_node().
pub struct UnlockedNode<'p, T: PathNode> {
    node:  NonNull<T>,
    _path: PhantomData<&'p BtreePath<T>>,
}
copy!([T: PathNode] UnlockedNode<'_, T>);

impl<'p, T: PathNode> UnlockedNode<'p, T> {
    pub fn ptr(self) -> NonNull<T> {
        self.node
    }

    /// Its lock: of a node, what's had without it.
    pub fn lock(self) -> &'p Locked<SixLock> {
        unsafe { &*self.node.as_ptr().cast::<Locked<SixLock>>() }
    }
}

/// What every level of a path table the transaction has reallocated away from
/// holds, in debug builds: for a path pointer kept across the realloc.
pub(crate) fn btree_path_poison<T>() -> NodePtr<T> {
    NodePtr::err(BchError::from(Errcode::no_btree_node_stale_paths))
}

impl BtreeNodeLockedType {
    /// The lock at @level in a path's nodes_locked: two bits a level, the six
    /// lock type plus one.
    pub fn of(nodes_locked: u8, level: usize) -> Self {
        Self::from_bits(BTREE_NODE_UNLOCKED as i32 + ((i32::from(nodes_locked) >> (level << 1)) & 3))
    }

    /// Held as six lock type @ty: the two number them the same.
    pub fn from_six(ty: SixLockType) -> Self {
        Self::from_bits(ty as i32)
    }

    /// The six lock type it's held as, locked.
    pub fn six(self) -> SixLockType {
        SixLockType::from_bits(self as u32)
    }
}

impl<T: PathNode> BtreePath<T> {
    /// The level's node and its hold, still the path's: a guard never dropped,
    /// had for as long as the path is borrowed. What nodes_locked says is held
    /// is put_level()'s node and hold, till take_level() - but for a level
    /// marked write before it's had, hold_marked_write()'s.
    pub fn hold(&self, level: usize) -> Option<(&T, ManuallyDrop<SixHold<'_>>)> {
        let locked = self.locked_type(level);
        if locked == BTREE_NODE_UNLOCKED {
            return None;
        }
        unsafe {
            let node = self.held_node(level).as_ref();
            Some((node, ManuallyDrop::new(SixHold::from_raw(&node.common().lock, locked.six()))))
        }
    }

    /// hold(), of a level marked write before the write lock's had - for the
    /// cycle detector to see the wait: its intent hold, which is all it has.
    pub(crate) fn hold_marked_write(&self, level: usize) -> (&T, ManuallyDrop<SixHold<'_>>) {
        assert!(self.write_locked(level), "a level marked write, that isn't");
        unsafe {
            let node = self.held_node(level).as_ref();
            (node, ManuallyDrop::new(SixHold::from_raw(&node.common().lock, SixLockType::SIX_LOCK_intent)))
        }
    }
    /// The level's node, relocked as @ty if nothing's written it since the
    /// level's lock_seq: if it's still the node the level had.
    pub fn relock_level(&mut self, level: usize, ty: SixLockType) -> bool {
        let Some(node) = self.l[level].b.get().ok().flatten() else { return false };
        let node = unsafe { node.as_ref() };
        let Some(hold) = node.common().lock.relock_as(ty, self.l[level].lock_seq) else { return false };
        self.put_level(level, node, hold);
        true
    }

    /// The level's hold, through @f - which has what the level holds after:
    /// None, unlocked, its node and lock_seq kept for a relock.
    fn change_level(&mut self, level: usize, f: impl FnOnce(SixHold<'_>) -> Option<SixHold<'_>>) {
        let Some((_, hold)) = self.take_level(level) else { return };

        let locked = f(hold).map_or(BTREE_NODE_UNLOCKED, |hold| BtreeNodeLockedType::from_six(hold.into_raw()));
        self.mark_locked_noreset(level, locked);
    }

    /// @hold on @node's lock, at a level holding none: the path's now, as of
    /// the lock's seq.
    pub fn put_level(&mut self, level: usize, node: &T, hold: SixHold<'_>) {
        ebug_assert!(!self.locked(level), "a level already locked");
        assert!(core::ptr::eq(hold.lock().six(), node.common().lock.six()), "a level given a hold on another node's lock");

        self.l[level].b = NodePtr::node(NonNull::from(node));
        self.l[level].lock_seq = node.common().lock.six().seq.get();
        self.mark_locked_noreset(level, BtreeNodeLockedType::from_six(hold.into_raw()));
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

    /// The level's node and its hold, the path's no more: its node and lock_seq
    /// kept for a relock.
    pub fn take_level(&mut self, level: usize) -> Option<(&T, SixHold<'_>)> {
        let locked = self.locked_type(level);
        if locked == BTREE_NODE_UNLOCKED {
            return None;
        }

        self.mark_locked_noreset(level, BTREE_NODE_UNLOCKED);
        unsafe {
            let node = self.held_node(level).as_ref();
            Some((node, SixHold::from_raw(&node.common().lock, locked.six())))
        }
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
        if self.locked_type(level) == BTREE_NODE_INTENT_LOCKED {
            self.change_level(level, |hold| Some(hold.downgrade()));
        }
    }

    /// A level held for read, up to intent if it can be: whether it's held
    /// for intent.
    pub fn try_upgrade_level(&mut self, level: usize) -> bool {
        self.change_level(level, |hold| {
            let (Ok(hold) | Err(hold)) = hold.try_upgrade();
            Some(hold)
        });
        self.locked_type_nowrite(level) == BTREE_NODE_INTENT_LOCKED
    }

    /// The node of a level nodes_locked says is held.
    fn held_node(&self, level: usize) -> NonNull<T> {
        self.l[level].b.get().ok().flatten().expect("a locked level has its node")
    }

    /// The level's node, if the path holds its lock: what makes it the path's,
    /// and keeps it while the path's borrowed.
    pub fn locked_node(&self, level: usize) -> Option<LockedNode<'_, T>> {
        self.hold(level).map(|(node, _)| LockedNode { node, _mode: PhantomData })
    }

    /// The level's node, as far as it's known without its lock: its memory
    /// the transaction's SRCU read lock keeps, but whether it's still the
    /// path's node, only a relock against the level's lock_seq says.
    pub fn unlocked_node(&self, level: usize) -> Option<UnlockedNode<'_, T>> {
        let node = self.l[level].b.get().ok()??;
        Some(UnlockedNode { node, _path: PhantomData })
    }

    /// NULL above the root, or an error pointer: why there's no node. A
    /// cached path's is a key cache entry's.
    pub(crate) fn node(&self, level: usize) -> NodePtr<T> {
        if level >= BTREE_MAX_DEPTH as usize {
            return NodePtr::null();
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
            crate::warn_once!(b == btree_path_poison(), no_fs,
                              "using a btree path that outlived trans->paths");
        }
        b
    }

    /// Whether nothing's written the level's node since its lock_seq.
    pub(crate) fn lock_seq_matches(&self, level: usize) -> bool {
        self.unlocked_node(level).is_some_and(|b| self.l[level].lock_seq == b.lock().six().seq.get())
    }

    /// Whether the level has @b, as of its lock's seq @seq.
    pub(crate) fn has_node(&self, level: usize, b: *const T, seq: u32) -> bool {
        self.l[level].b.is(b) && self.l[level].lock_seq == seq
    }

    pub(crate) fn is_btree_node(&self, level: usize) -> bool {
        level < BTREE_MAX_DEPTH as usize && self.l[level].b.is_node()
    }

    pub(crate) fn locked_type(&self, level: usize) -> BtreeNodeLockedType {
        BtreeNodeLockedType::of(self.nodes_locked, level)
    }

    pub(crate) fn locked_type_nowrite(&self, level: usize) -> BtreeNodeLockedType {
        match self.locked_type(level) {
            BTREE_NODE_WRITE_LOCKED => BTREE_NODE_INTENT_LOCKED,
            have => have,
        }
    }

    pub(crate) fn write_locked(&self, level: usize) -> bool {
        self.locked_type(level) == BTREE_NODE_WRITE_LOCKED
    }

    pub(crate) fn intent_locked(&self, level: usize) -> bool {
        self.locked_type(level) == BTREE_NODE_INTENT_LOCKED
    }

    pub(crate) fn locked(&self, level: usize) -> bool {
        self.locked_type(level) != BTREE_NODE_UNLOCKED
    }

    pub(crate) fn mark_locked_noreset(&mut self, level: usize, ty: BtreeNodeLockedType) {
        /* relying on this to avoid a branch */
        const { assert!(SixLockType::SIX_LOCK_read as i32 == 0 && SixLockType::SIX_LOCK_intent as i32 == 1) };

        self.nodes_locked &= !(3 << (level << 1));
        self.nodes_locked |= ((ty as i32 + 1) as u8) << (level << 1);
    }

    pub(crate) fn lock_want_six(&self, level: usize) -> SixLockType {
        if level < self.locks_want() as usize {
            SixLockType::SIX_LOCK_intent
        } else {
            SixLockType::SIX_LOCK_read
        }
    }

    pub(crate) fn lock_want(&self, level: usize) -> BtreeNodeLockedType {
        let (path_level, locks_want) = (self.level() as usize, self.locks_want() as usize);

        if level < path_level {
            BTREE_NODE_UNLOCKED
        } else if level < locks_want {
            BTREE_NODE_INTENT_LOCKED
        } else if level == path_level {
            BTREE_NODE_READ_LOCKED
        } else {
            BTREE_NODE_UNLOCKED
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

    /// 0 where C has _THIS_IP_, which Rust can't name.
    pub(crate) fn ip_allocated(&self) -> usize {
        #[cfg(any(CONFIG_BCACHEFS_LOCK_TIME_STATS, CONFIG_BCACHEFS_DEBUG))]
        return self.ip_allocated;
        #[cfg(not(any(CONFIG_BCACHEFS_LOCK_TIME_STATS, CONFIG_BCACHEFS_DEBUG)))]
        0
    }
}
