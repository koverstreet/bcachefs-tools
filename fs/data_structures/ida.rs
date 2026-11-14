// SPDX-License-Identifier: GPL-2.0

//! ID allocator.
//!
//! Implementation: a d-ary bitmap tree in a flat array, d == BITS_PER_LONG,
//! eytzinger layout. Each node is a machine word; a set bit means the
//! corresponding child subtree has at least one free id. Leaves represent
//! ids directly (set bit == id free).
//!
//! Growth: when the root has no free bits and max would admit a larger tree,
//! we realloc to a tree one level deeper. The old tree becomes the leftmost
//! subtree of the new root; the rest of the new tree is initialized to all
//! bits set (all ids free).
//!
//! Replaces the xarray-backed kernel ida, in both builds: we don't need id ->
//! pointer translation.
//!
//! The lock is a spinlock, irqsave: ids are freed from bio completions. So a
//! deeper tree is allocated outside it, and the tree it replaces freed outside
//! it - kvmalloc() and kvfree() may sleep.

use crate::c;
use crate::locking::Locked;
use crate::util::alloc::{AllocError, KVVec};
use pin_init::Zeroable;

const B: usize = usize::BITS as usize;
const B_BITS: u32 = usize::BITS.ilog2();

/// IDA_DEBUG: verify the whole tree after every change.
const DEBUG: bool = false;

/* Total nodes at levels 0..L-1 == (B^L - 1) / (B - 1). */
fn nodes_up_to_level(l: u32) -> usize {
    (0..l).map(|i| 1 << (i * B_BITS)).sum()
}

fn capacity(depth: u32) -> usize {
    if depth == 0 {
        return 0;
    }
    1usize.checked_shl(depth * B_BITS).unwrap_or(usize::MAX)
}

fn first_leaf(depth: u32) -> usize {
    if depth != 0 { nodes_up_to_level(depth - 1) } else { 0 }
}

/*
 * Compute the bit range [bit_min, bit_max] within a node covering
 * base..base+(B << span_shift)-1 that overlaps the id range [min, max].
 * None if no bit is in-range.
 */
fn bit_range(base: usize, span_shift: u32, min: u32, max: u32) -> Option<(u32, u32)> {
    let (min, max) = (min as usize, max as usize);

    if max < base || min >= base + (B << span_shift) {
        return None;
    }

    let bit_min = if min > base { ((min - base) >> span_shift) as u32 } else { 0 };
    let bit_max = ((max - base) >> span_shift).min(B - 1) as u32;

    (bit_min <= bit_max).then_some((bit_min, bit_max))
}

/// The tree, and its depth: 0 = uninitialized tree.
struct Tree {
    depth: u32,
    /// (B^depth - 1) / (B - 1) words
    nodes: KVVec<usize>,
}

// depth 0, no nodes
unsafe impl Zeroable for Tree {}

impl Tree {
    fn alloc_descend(&mut self, i: usize, l: u32, base: usize, min: u32, max: u32) -> Option<u32> {
        let d = self.depth;
        /* span covered by a single bit at level L */
        let span_shift = (d - l - 1) * B_BITS;
        let (bit_min, bit_max) = bit_range(base, span_shift, min, max)?;

        let word = self.nodes[i];
        let mut mask = !0usize << bit_min;
        if (bit_max as usize) < B - 1 {
            mask &= (1 << (bit_max + 1)) - 1;
        }

        let mut usable = word & mask;

        while usable != 0 {
            let bit = usable.trailing_zeros();
            let new_base = base + ((bit as usize) << span_shift);

            if l == d - 1 {
                /* Leaf: bit maps to id. */
                assert!(new_base <= i32::MAX as usize, "ida: id {new_base} past INT_MAX");
                self.nodes[i] &= !(1 << bit);

                /* Propagate "full" upward. */
                let mut j = i;
                while j > 0 && self.nodes[j] == 0 {
                    let parent = (j - 1) >> B_BITS;
                    let bit_in_parent = (j - 1) & (B - 1);
                    assert!(self.nodes[parent] & (1 << bit_in_parent) != 0,
                            "ida: node {parent} bit {bit_in_parent} says node {j} is full already");
                    self.nodes[parent] &= !(1 << bit_in_parent);
                    j = parent;
                }
                return Some(new_base as u32);
            }

            let child = (i << B_BITS) + 1 + bit as usize;
            if let Some(id) = self.alloc_descend(child, l + 1, new_base, min, max) {
                return Some(id);
            }

            /* Child's subtree has free slots but none in [min, max]. Skip. */
            usable &= usable - 1;
        }

        None
    }

    fn alloc(&mut self, min: u32, max: u32) -> Option<u32> {
        if self.depth == 0 {
            return None;
        }
        let id = self.alloc_descend(0, 0, 0, min, max)?;
        assert!(id >= min && id <= max, "ida: id {id} outside [{min}, {max}]");
        self.verify();
        Some(id)
    }

    /*
     * No free id in [min, max]: either current tree is full, or all its free
     * bits are outside [min, max]. Grow if there's room for ids above current
     * capacity (and max allows) - and keep (depth * B_BITS) < BITS_PER_LONG,
     * for the shift math of nodes_up_to_level() and capacity().
     */
    fn can_grow(&self, max: u32) -> bool {
        let cap = capacity(self.depth);
        cap <= max as usize &&
            cap < i32::MAX as usize + 1 &&
            (self.depth + 1) * B_BITS < usize::BITS
    }

    /// One level deeper, into @new_nodes - every id free, a tree of depth + 1:
    /// the old nodes, to free outside the lock.
    fn grow(&mut self, mut new_nodes: KVVec<usize>) -> KVVec<usize> {
        /*
         * Old tree becomes the leftmost subtree of new tree. For each level L
         * in the old tree (0..depth-1), its B^L nodes move to new level L+1,
         * which starts at nodes_up_to_level(L+1). The old level L starts at
         * nodes_up_to_level(L).
         */
        for l in 0..self.depth {
            let count = 1 << (l * B_BITS);
            let (to, from) = (nodes_up_to_level(l + 1), nodes_up_to_level(l));
            new_nodes[to..to + count].copy_from_slice(&self.nodes[from..from + count]);
        }

        /*
         * New root bit 0 reflects the old root: if old root had no free bits,
         * new root's bit 0 is clear. Bits 1..B-1 correspond to fresh subtrees
         * that are all free, so they stay set.
         */
        new_nodes[0] = if self.depth != 0 && self.nodes[0] == 0 { !1 } else { !0 };

        self.depth += 1;
        let old = core::mem::replace(&mut self.nodes, new_nodes);
        self.verify();
        old
    }

    fn free(&mut self, id: u32) {
        assert!(self.depth != 0, "ida: id {id} freed, none allocated");
        assert!((id as usize) < capacity(self.depth), "ida: id {id} freed, past the tree");

        let leaf_idx = first_leaf(self.depth) + (id as usize >> B_BITS);
        let bit = id as usize & (B - 1);

        /* Double-free: the bit must currently be CLEAR (id allocated). */
        assert!(self.nodes[leaf_idx] & (1 << bit) == 0, "ida: id {id} freed twice");

        let mut was_zero = self.nodes[leaf_idx] == 0;
        self.nodes[leaf_idx] |= 1 << bit;

        /* Propagate "has free" upward for nodes that were previously fully allocated. */
        let mut j = leaf_idx;
        while was_zero && j > 0 {
            let parent = (j - 1) >> B_BITS;
            let bit_in_parent = (j - 1) & (B - 1);
            was_zero = self.nodes[parent] == 0;
            self.nodes[parent] |= 1 << bit_in_parent;
            j = parent;
        }

        self.verify();
    }

    fn find_first(&self) -> Option<u32> {
        let first = first_leaf(self.depth);
        let n_leaves = capacity(self.depth) / B;

        (0..n_leaves).find_map(|k| {
            let allocated = !self.nodes[first + k];
            (allocated != 0).then(|| (k * B) as u32 + allocated.trailing_zeros())
        })
    }

    /*
     * Walk the tree verifying: at every interior node, bit k is set iff the
     * k-th child subtree contains at least one free slot. At leaves, bits are
     * the ground truth (each bit == id free). Expensive; debug builds only.
     */
    fn verify_subtree(&self, i: usize, l: u32) {
        if l == self.depth - 1 {
            return; /* leaf: self-consistent by definition */
        }

        let word = self.nodes[i];
        for bit in 0..B {
            let child = (i << B_BITS) + 1 + bit;
            let child_has_free = self.nodes[child] != 0;
            let parent_says_free = (word >> bit) & 1 != 0;

            assert_eq!(child_has_free, parent_says_free,
                       "ida: invariant violated at node {i} bit {bit}: child[{child}] word={:#x}",
                       self.nodes[child]);
            self.verify_subtree(child, l + 1);
        }
    }

    fn verify(&self) {
        if !DEBUG {
            return;
        }
        if self.depth == 0 {
            assert!(self.nodes.is_empty());
            return;
        }
        assert!(!self.nodes.is_empty());
        self.verify_subtree(0, 0);
    }
}

pub struct Ida(Locked<c::spinlock_t, Tree>);

// its lock, zeroed, isn't initialized: init_zeroed()
unsafe impl Zeroable for Ida {}

impl Ida {
    /// For an Ida in zeroed memory, or destroy()ed.
    ///
    /// # Safety
    /// Nothing is using it, or will until it returns.
    pub unsafe fn init_zeroed(&self) {
        let Ok(()) = unsafe { self.0.init_zeroed(c"&ida->lock", crate::lock_class_key!()) };
    }

    /// The lowest free id in [@min, @max] - @max no more than INT_MAX, as the
    /// kernel's ida has it.
    pub fn alloc_range(&self, min: u32, max: u32) -> Result<u32, AllocError> {
        let max = max.min(i32::MAX as u32);
        if min > max {
            return Err(AllocError);
        }

        loop {
            let depth = {
                let mut t = self.0.lock();
                if let Some(id) = t.alloc(min, max) {
                    return Ok(id);
                }
                if !t.can_grow(max) {
                    return Err(AllocError);
                }
                t.depth
            };

            /* Initialize everything to "all free". */
            let new_nodes = KVVec::from_elem(!0usize, nodes_up_to_level(depth + 1))?;

            let unused = {
                let mut t = self.0.lock();
                // grown meanwhile: ours goes
                if t.depth == depth { t.grow(new_nodes) } else { new_nodes }
            };
            drop(unused);
        }
    }

    pub fn free(&self, id: u32) {
        self.0.lock().free(id)
    }

    /// The lowest id allocated.
    pub fn find_first(&self) -> Option<u32> {
        self.0.lock().find_first()
    }

    /// Every id freed, and the tree with them.
    pub fn destroy(&self) {
        let nodes = {
            let mut t = self.0.lock();
            t.depth = 0;
            core::mem::take(&mut t.nodes)
        };
        drop(nodes);
    }
}
