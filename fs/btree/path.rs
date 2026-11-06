// SPDX-License-Identifier: GPL-2.0

//! btree_path: the low level path to a btree node, which holds its locks.

#![allow(non_camel_case_types)]

use crate::btree::iter::BtreeTrans;
use crate::btree::node::iter::KeyIter;
use crate::btree::locking::btree_node_locked_type::{self, *};
use crate::btree::node::{BtreeNode, NodeContents};
use crate::c;
use crate::cstructs::c as cs;
use crate::errcode::{bch_errcode, err_ptr, errptr_or_null, BchError};
use crate::format::{Bpos, Btree};
use crate::types::{bits_to_longs, c_default, Lent};
use crate::util::alloc::kvzalloc_nofail;
use crate::util::rcu::rcu_assign_pointer;
#[cfg(CONFIG_BCACHEFS_LOCK_TIME_STATS)]
use crate::util::kernel::local_clock;
use cstruct_macros::{bitfield, c_export, c_name, c_typedef, CStruct};
use crate::locking::{Locked, LockedBy};
use crate::locking::six::SixHold;
use crate::c::six_lock_type;
use core::ffi::{c_int, c_ulong};
use core::marker::PhantomData;
use core::mem::{offset_of, ManuallyDrop};
use core::ops::{Deref, DerefMut, Index, IndexMut};
use core::ptr::{self, NonNull};

c_typedef! {
    #[c_name(btree_path_idx_t)]
    pub type PathIdx = u16;
}

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
#[derive(CStruct)]
pub struct btree_path_level<T: PathNode = cs::btree_bkey_cached_common> {
    /* a cached path's is a struct bkey_cached's */
    #[c("struct btree *b")]
    pub b: NodePtr<T>,
    pub iter: KeyIter,
    pub lock_seq: u32,
    #[cfg(CONFIG_BCACHEFS_LOCK_TIME_STATS)]
    pub lock_taken_time: u64,
}
c_default!(btree_path_level);
copy!([T: PathNode] btree_path_level<T>);

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
        PathSnapshot(unsafe { ptr::read(path) })
    }

    /// For C's bch2_btree_path_to_text().
    pub(crate) fn as_ptr(&self) -> *mut BtreePath {
        ptr::from_ref(&self.0).cast_mut()
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
}

/// The path table, for a C export: C's to lend for the call, its own caller
/// holding no reference into it - a Rust caller's borrow, if there is one, idle
/// across the call into C.
impl<'t> Lent<'t, BtreeTrans<'_>> for Paths<'t> {
    unsafe fn lent(trans: &'t BtreeTrans<'_>) -> Self {
        Paths { trans, took: false }
    }
}

impl Paths<'_> {
    /// Write over the intent path @idx holds at @level, which nodes_locked
    /// already says is write - marked before the wait, for the cycle detector
    /// to see - waited for with @wait, the transaction's own read holds on the
    /// node set aside meanwhile. On failure, six's error, and the level's back
    /// to intent.
    pub(crate) fn lock_write_marked(&mut self, idx: PathIdx, level: usize, wait: &c::six_lock_waiter,
                                    should_sleep_fn: impl FnMut() -> c_int, ip: c_ulong) -> Result<(), c_int> {
        ebug_assert!(self[idx].write_locked(level), "taking write at a level not marked for it");
        let node = self[idx].held_node(level);
        let readers = self.trans.node_lock_counts(self, None, node, level).n[c::SIX_LOCK_read as usize] as c_int;

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

/// Where a path table's parts are, in bytes from its start, for @nr paths:
/// the allocated bitmap, then the header's nr_paths right before the paths,
/// then sorted and updates. One block, so that what reads another thread's -
/// the cycle detector, bch2_btree_trans_to_text() - has all of it from the
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
        let header  = bits_to_longs(nr) * size_of::<c_ulong>();
        let paths   = header + offset_of!(c::btree_trans_paths, paths);
        let sorted  = paths + nr * size_of::<BtreePath>();
        let updates = sorted + (nr + 4) * size_of::<PathIdx>();
        TableLayout { header, paths, sorted, updates, size: updates + nr * size_of::<c::btree_insert_entry>() }
    }
}

/* the embedded table, laid out as any other */
const _: () = {
    let base = offset_of!(BtreeTrans<'static>, _paths_allocated);
    let l = TableLayout::of(cs::BTREE_ITER_INITIAL as usize);
    assert!(offset_of!(BtreeTrans<'static>, trans_paths) - base == l.header &&
            offset_of!(BtreeTrans<'static>, _paths) - base == l.paths &&
            offset_of!(BtreeTrans<'static>, _sorted) - base == l.sorted &&
            offset_of!(BtreeTrans<'static>, _updates) - base == l.updates);
};

/// The header of the table whose paths start at @paths: trans_paths_nr()'s.
pub(crate) fn table_header(paths: *mut BtreePath) -> *mut c::btree_trans_paths {
    paths.wrapping_byte_sub(offset_of!(c::btree_trans_paths, paths)).cast()
}

/// Its allocated bitmap, of @nr paths: trans_paths_allocated()'s.
pub(crate) fn table_allocated(paths: *mut BtreePath, nr: usize) -> *mut c_ulong {
    table_header(paths).cast::<c_ulong>().wrapping_sub(bits_to_longs(nr))
}

impl BtreeTrans<'_> {
    /// A table the transaction's done with, from its allocated bitmap: freed
    /// once readers are done with it, unless it's the embedded one.
    fn table_retire(&self, allocated: *mut c_ulong) {
        if allocated != self._paths_allocated.as_ptr().cast() {
            // kfree_rcu_mightsleep() is kvfree_rcu_mightsleep()
            unsafe { c::rust_kfree_rcu_mightsleep(allocated.cast()) };
        }
    }
}

impl Paths<'_> {
    /// The table at twice the size, what's in it copied over, and the old one
    /// retired. Nothing borrowed from it outlives that - this takes &mut self
    /// - but a pointer C kept into it can.
    pub(crate) fn grow(&mut self) {
        let t = self.trans;
        let (old_nr, nr) = (t.nr_paths.get() as usize, t.nr_paths.get() as usize * 2);
        let l = TableLayout::of(nr);
        let p = kvzalloc_nofail(l.size).as_ptr();
        let allocated = p.cast::<c_ulong>();
        let paths     = p.wrapping_add(l.paths).cast::<BtreePath>();
        let sorted    = p.wrapping_add(l.sorted).cast::<PathIdx>();
        let updates   = p.wrapping_add(l.updates).cast::<c::btree_insert_entry>();

        unsafe {
            ptr::copy_nonoverlapping(t.paths_allocated.get(), allocated, bits_to_longs(old_nr));
            (*table_header(paths)).nr_paths = nr as c_ulong;
            ptr::copy_nonoverlapping(t.paths.get(), paths, old_nr);
            ptr::copy_nonoverlapping(t.sorted.get(), sorted, t.nr_sorted as usize);
            ptr::copy_nonoverlapping(t.updates.get(), updates, old_nr);
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
         * paths. See bch2_trans_paths_init().
         */
        if cfg!(CONFIG_BCACHEFS_DEBUG) {
            for path in self.iter_mut() {
                for level in &mut path.l {
                    level.b = btree_path_poison();
                }
            }
        }

        let old = t.paths_allocated.get();
        rcu_assign_pointer(&t.paths_allocated, allocated);
        rcu_assign_pointer(&t.paths, paths);
        rcu_assign_pointer(&t.sorted, sorted);
        rcu_assign_pointer(&t.updates, updates);

        t.nr_paths.set(nr as PathIdx);

        t.table_retire(old);
    }
}

/// A transaction's path table, as it starts: its own, embedded.
#[c_export]
pub fn bch2_trans_paths_init(trans: &BtreeTrans<'_>) {
    trans.nr_paths.set(cs::BTREE_ITER_INITIAL as PathIdx);
    trans.paths_allocated.set(trans._paths_allocated.as_ptr().cast());
    trans.sorted.set(trans._sorted.as_ptr().cast());
    trans.paths.set(trans._paths.as_ptr().cast());
    trans.updates.set(trans._updates.as_ptr().cast());

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
        for path in trans.paths().iter_mut() {
            for level in &mut path.l {
                level.b = NodePtr::null();
            }
        }
    }

    unsafe {
        (*trans.trans_paths.as_ptr()).nr_paths = cs::BTREE_ITER_INITIAL.into();

        /* Reserve path 0 for our sentinal value */
        *trans.paths_allocated.get() = 1;
    }
}

#[c_export]
pub fn bch2_trans_paths_grow(_trans: &BtreeTrans<'_>, #[lent(_trans)] paths: &mut Paths<'_>) {
    paths.grow()
}

#[c_export]
pub fn bch2_trans_paths_exit(trans: &BtreeTrans<'_>) {
    let allocated = trans.paths_allocated.replace(ptr::null_mut());
    trans.paths.set(ptr::null_mut());
    trans.table_retire(allocated);
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

    /// What a common part is the start of, if it's one of these: by its
    /// cached flag.
    fn from_common(b: &c::btree_bkey_cached_common) -> Option<&Self> where Self: Sized {
        Self::CACHED.map_or(true, |cached| cached == b.cached)
            .then(|| unsafe { &*core::ptr::from_ref(b).cast() })
    }
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
copy!([T: PathNode] LockedNode<'_, T>);

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
    pub fn lock(self) -> &'p Locked<c::six_lock> {
        unsafe { &*self.node.as_ptr().cast::<Locked<c::six_lock>>() }
    }
}

/// What every level of a path table the transaction has reallocated away from
/// holds, in debug builds: for a path pointer kept across the realloc.
pub(crate) fn btree_path_poison<T>() -> NodePtr<T> {
    NodePtr::err(BchError::from(bch_errcode::BCH_ERR_no_btree_node_stale_paths))
}

impl btree_node_locked_type {
    /// The lock at @level in a path's nodes_locked: two bits a level, the six
    /// lock type plus one.
    pub fn of(nodes_locked: u8, level: usize) -> Self {
        Self::from_bits(BTREE_NODE_UNLOCKED as i32 + ((i32::from(nodes_locked) >> (level << 1)) & 3))
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
    /// Another hold on the level's node, as @want, if the level's is no
    /// weaker.
    pub fn increment_level(&self, level: usize, want: six_lock_type) -> Option<SixHold<'_>> {
        let locked = self.locked_type(level);
        (locked != BTREE_NODE_UNLOCKED && locked as i32 >= want as i32).then(|| {
            ManuallyDrop::new(unsafe { SixHold::from_raw(self.held_lock(level), locked.six()) }).increment(want)
        })
    }

    /// The level's node, relocked as @ty if nothing's written it since the
    /// level's lock_seq: if it's still the node the level had.
    pub fn relock_level(&mut self, level: usize, ty: six_lock_type) -> bool {
        let Some(node) = self.l[level].b.get().ok().flatten() else { return false };
        let node = unsafe { node.as_ref() };
        let Some(hold) = node.common().lock.relock_as(ty, self.l[level].lock_seq) else { return false };
        self.put_level(level, node, hold);
        true
    }

    /// The level's hold, through @f - which has what the level holds after:
    /// None, unlocked, its node and lock_seq kept for a relock.
    fn change_level(&mut self, level: usize, f: impl FnOnce(SixHold<'_>) -> Option<SixHold<'_>>) {
        let locked = self.locked_type(level);
        if locked == BTREE_NODE_UNLOCKED {
            return;
        }

        let locked = f(unsafe { SixHold::from_raw(self.held_lock(level), locked.six()) })
            .map_or(BTREE_NODE_UNLOCKED, |hold| btree_node_locked_type::from_six(hold.into_raw()));
        self.mark_locked_noreset(level, locked);
    }

    /// @hold on @node's lock, at a level holding none: the path's now, as of
    /// the lock's seq.
    pub fn put_level(&mut self, level: usize, node: &T, hold: SixHold<'_>) {
        ebug_assert!(!self.locked(level), "a level already locked");
        assert!(core::ptr::eq(hold.lock().six(), node.common().lock.six()), "a level given a hold on another node's lock");

        self.l[level].b = NodePtr::node(NonNull::from(node));
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

    /// Its lock.
    fn held_lock(&self, level: usize) -> &Locked<c::six_lock> {
        self.unlocked_node(level).expect("a locked level has its node").lock()
    }

    /// The level's node, if the path holds its lock: what makes it the path's,
    /// and keeps it while the path's borrowed.
    pub fn locked_node(&self, level: usize) -> Option<LockedNode<'_, T>> {
        self.locked(level).then(|| unsafe { LockedNode::new(self.held_node(level).as_ref()) })
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
        if level >= c::BTREE_MAX_DEPTH as usize {
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
        level < c::BTREE_MAX_DEPTH as usize && self.l[level].b.is_node()
    }

    pub(crate) fn locked_type(&self, level: usize) -> btree_node_locked_type {
        btree_node_locked_type::of(self.nodes_locked, level)
    }

    pub(crate) fn locked_type_nowrite(&self, level: usize) -> btree_node_locked_type {
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
    pub(crate) fn ip_allocated(&self) -> c_ulong {
        #[cfg(any(CONFIG_BCACHEFS_LOCK_TIME_STATS, CONFIG_BCACHEFS_DEBUG))]
        return self.ip_allocated;
        #[cfg(not(any(CONFIG_BCACHEFS_LOCK_TIME_STATS, CONFIG_BCACHEFS_DEBUG)))]
        0
    }
}
