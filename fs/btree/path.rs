// SPDX-License-Identifier: GPL-2.0

//! btree_path: the low level path to a btree node, which holds its locks.

#![allow(non_camel_case_types)]

use crate::btree::iter::BtreeTrans;
use crate::btree::node::iter::KeyIter;
use crate::btree::node::BtreeNode;
use crate::c;
use crate::cstructs::c as cs;
use crate::errcode::{bch_errcode, err_ptr, BchError};
use crate::format::{Bpos, Btree};
use crate::types::c_default;
use cstruct_macros::{bitfield, c_name, c_typedef, CStruct};
use core::ops::{Deref, DerefMut, Index, IndexMut};
use nestify::nest;

c_typedef! {
    #[c_name(btree_path_idx_t)]
    pub type PathIdx = u16;
}

#[repr(C)]
#[derive(Clone, Copy, CStruct)]
pub struct btree_path_level {
    pub b: *mut BtreeNode,
    pub iter: KeyIter,
    pub lock_seq: u32,
    #[cfg(CONFIG_BCACHEFS_LOCK_TIME_STATS)]
    pub lock_taken_time: u64,
}
c_default!(btree_path_level);

nest! {
    #[repr(C)]
    #[c_name(btree_path)]
    #[derive(Clone, Copy, CStruct)]
    pub struct BtreePath {
        pub sorted_idx: cs::PathIdx,
        pub ref_: u8,
        pub intent_ref: u8,

        /* btree_iter_copy starts here: */
        pub pos: Bpos,

        #[c_bitfield]
        #>[bitfield(u16, ty = u32)]
        pub bits: pub struct btree_path_bits {
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
        },
        pub nodes_locked: u8,

        pub l: [cs::btree_path_level; cs::BTREE_MAX_DEPTH as usize],
        /* C's TRACK_PATH_ALLOCATED, which btree/types_inline.h defines from
         * these: rustc sees only the configuration */
        #[cfg(any(CONFIG_BCACHEFS_LOCK_TIME_STATS, CONFIG_BCACHEFS_DEBUG))]
        pub ip_allocated: core::ffi::c_ulong,
    }
}
c_default!(BtreePath);

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

/// What every level of a path table the transaction has reallocated away from
/// holds, in debug builds: for a path pointer kept across the realloc.
pub(crate) fn btree_path_poison() -> *mut BtreeNode {
    err_ptr(BchError::from(bch_errcode::BCH_ERR_no_btree_node_stale_paths))
}

impl BtreePath {
    /// NULL above the root, or an error pointer: why there's no node.
    pub(crate) fn node(&self, level: usize) -> *mut BtreeNode {
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
            crate::warn_once!(b == btree_path_poison(), no_fs,
                              "using a btree path that outlived trans->paths");
        }
        b
    }

    pub(crate) fn lock_seq_matches(&self, b: &BtreeNode, level: usize) -> bool {
        self.l[level].lock_seq == b.c.lock.six().seq.get()
    }

    pub(crate) fn has_node(&self, b: &BtreeNode) -> bool {
        let level = b.c.level as usize;

        core::ptr::eq(self.l[level].b, b) && self.lock_seq_matches(b, level)
    }
}
