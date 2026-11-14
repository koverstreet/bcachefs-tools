// SPDX-License-Identifier: GPL-2.0

//! Generic radix trees/sparse arrays: GenRadix<T>, a sparse array of T,
//! allocated a node at a time and zeroed, whose entries never move once they
//! exist.
//!
//! Very simple and minimalistic, supporting arbitrary size entries up to
//! NODE_SIZE.
//!
//! The radix tree allocates one page of entries at a time, so entries may
//! exist that were never explicitly allocated - they will be initialized to
//! all zeroes.
//!
//! Internally, a genradix is just a radix tree of pages, and indexing works in
//! terms of byte offsets. The split is C's: GenRadix is generic and #[inline],
//! and turns an index into a byte offset with size_of::<T>() a constant - so a
//! size that's a power of two is a shift, and one that isn't a multiply and
//! shift for the divide and the modulo by objects per node; and under it, the
//! core - Tree - works in byte offsets and isn't generic: one copy, not one
//! per T.
//!
//! An entry in an allocated node is zeroed memory until something writes it,
//! and so is a T only if zeroed is one: T: Zeroable. Allocating takes &mut -
//! two callers allocating one index would otherwise each hold a &mut T.
//!
//! No Drop: GenRadix's holders free it, with free(), as C's did.
//! SharedGenRadix, for one allocated from several threads at once, frees on
//! drop.

use crate::c;
use crate::types::GenRadix;
use crate::util::alloc::{current_gfp_context_raw, kfree_unsized, kzalloc_gfp, kzalloc_nofail, AllocError};
use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::mem::size_of;
use core::ptr::{self, NonNull};
use crate::util::rcu::rcu_dereference;
use core::sync::atomic::{AtomicPtr, Ordering::{Acquire, Relaxed, Release}};
use pin_init::Zeroable;

const NODE_SHIFT: u32 = 9;
pub const NODE_SIZE: usize = 1 << NODE_SHIFT;

const ARY: usize = NODE_SIZE / size_of::<*mut Node>();
const ARY_SHIFT: u32 = ARY.ilog2();

/* depth that's needed for a genradix that can address up to ULONG_MAX: */
const MAX_DEPTH: u32 = (usize::BITS - NODE_SHIFT).div_ceil(ARY_SHIFT);

const DEPTH_MASK: usize = (MAX_DEPTH as usize + 1).next_power_of_two() - 1;

const fn depth_shift(depth: u32) -> u32 {
    NODE_SHIFT + ARY_SHIFT * depth
}

/*
 * Returns size (of data, in bytes) that a tree of a given depth holds:
 */
const fn depth_size(depth: u32) -> usize {
    1 << depth_shift(depth)
}

/// ilog2(@offset) < depth_shift(@depth): a tree of @depth holds @offset.
fn holds(depth: u32, offset: usize) -> bool {
    offset.checked_shr(depth_shift(depth)).unwrap_or(0) == 0
}

fn root_to_depth(r: *mut Node) -> u32 {
    (r.addr() & DEPTH_MASK) as u32
}

fn root_to_node(r: *mut Node) -> *mut Node {
    r.map_addr(|a| a & !DEPTH_MASK)
}

/// A node: NODE_SIZE bytes, zeroed when allocated - an interior node's ARY
/// child pointers, or a leaf's data. kmalloc() of a power of two, so aligned
/// to its size, in either build.
struct Node {
    _bytes: [u8; NODE_SIZE],
}

impl Node {
    /// # Safety
    /// @n is an interior node, and @i < ARY.
    unsafe fn child<'a>(n: *mut Node, i: usize) -> &'a AtomicPtr<Node> {
        unsafe { AtomicPtr::from_ptr(n.cast::<*mut Node>().add(i)) }
    }

    fn alloc(gfp: c::gfp_t) -> *mut Node {
        kzalloc_gfp(NODE_SIZE, gfp).map_or(ptr::null_mut(), |p| p.as_ptr().cast())
    }

    /// # Safety
    /// @n is alloc()'s, or null, and nothing uses it after.
    unsafe fn free(n: *mut Node) {
        unsafe { kfree_unsized(n.cast()) }
    }
}

/// The tree, in byte offsets: its root node, with the tree's depth in the
/// pointer's low bits.
#[repr(transparent)]
pub struct Tree {
    root: AtomicPtr<Node>,
}

// null: empty
unsafe impl Zeroable for Tree {}

/// Where an iteration is: the byte offset, and the index it's of.
struct IterPos {
    offset: usize,
    pos:    usize,
}

impl Tree {
    /*
     * Returns pointer to the specified byte @offset within @radix, or NULL if not
     * allocated
     */
    fn ptr(&self, mut offset: usize) -> *mut u8 {
        let r = rcu_dereference(&self.root);
        let mut n = root_to_node(r);
        let level = root_to_depth(r);
        let mut shift = depth_shift(level);

        if !holds(level, offset) {
            return ptr::null_mut();
        }

        while !n.is_null() && shift > NODE_SHIFT {
            shift -= ARY_SHIFT;
            n = rcu_dereference(unsafe { Node::child(n, offset >> shift) });
            offset &= (1 << shift) - 1;
        }

        if n.is_null() { ptr::null_mut() } else { unsafe { n.cast::<u8>().add(offset) } }
    }

    /*
     * Returns pointer to the specified byte @offset within @radix, allocating it if
     * necessary - newly allocated slots are always zeroed out:
     */
    fn ptr_alloc(&self, mut offset: usize, preallocated: Option<&mut NewNode>, gfp: c::gfp_t) -> *mut u8 {
        let mut v = rcu_dereference(&self.root);
        let mut new_node = preallocated.map_or(ptr::null_mut(), |p| core::mem::replace(&mut p.0, ptr::null_mut()));
        let mut n;
        let mut level;

        /* Increase tree depth if necessary: */
        loop {
            let r = v;

            n     = root_to_node(r);
            level = root_to_depth(r);

            if !n.is_null() && holds(level, offset) {
                break;
            }

            if new_node.is_null() {
                new_node = Node::alloc(gfp);
                if new_node.is_null() {
                    return ptr::null_mut();
                }
            }

            unsafe { Node::child(new_node, 0) }.store(n, Relaxed);
            let new_root = new_node.map_addr(|a| a | if n.is_null() { 0 } else { level as usize + 1 });

            match self.root.compare_exchange(r, new_root, Release, Relaxed) {
                Ok(_) => {
                    v = new_root;
                    new_node = ptr::null_mut();
                }
                Err(cur) => {
                    v = cur;
                    unsafe { Node::child(new_node, 0) }.store(ptr::null_mut(), Relaxed);
                }
            }
        }

        while level > 0 {
            level -= 1;
            let p = unsafe { Node::child(n, offset >> depth_shift(level)) };
            offset &= depth_size(level) - 1;

            n = rcu_dereference(p);
            if n.is_null() {
                if new_node.is_null() {
                    new_node = Node::alloc(gfp);
                    if new_node.is_null() {
                        return ptr::null_mut();
                    }
                }

                match p.compare_exchange(ptr::null_mut(), new_node, Release, Relaxed) {
                    Ok(_) => {
                        n = new_node;
                        new_node = ptr::null_mut();
                    }
                    Err(cur) => n = cur,
                }
            }
        }

        unsafe { Node::free(new_node) };

        unsafe { n.cast::<u8>().add(offset) }
    }

    fn iter_peek(&self, iter: &mut IterPos, objs_per_page: usize) -> *mut u8 {
        if iter.offset == usize::MAX {
            return ptr::null_mut();
        }

        'restart: loop {
            let r = rcu_dereference(&self.root);
            if r.is_null() {
                return ptr::null_mut();
            }

            let mut n     = root_to_node(r);
            let mut level = root_to_depth(r);

            if !holds(level, iter.offset) {
                return ptr::null_mut();
            }

            while level > 0 {
                level -= 1;

                let mut i = (iter.offset >> depth_shift(level)) & (ARY - 1);

                loop {
                    let child = rcu_dereference(unsafe { Node::child(n, i) });
                    if !child.is_null() {
                        n = child;
                        break;
                    }

                    let objs_per_ptr = depth_size(level);

                    let Some(next) = iter.offset.checked_add(objs_per_ptr) else {
                        iter.offset = usize::MAX;
                        iter.pos    = usize::MAX;
                        return ptr::null_mut();
                    };

                    i += 1;
                    iter.offset = next & !(objs_per_ptr - 1);
                    iter.pos    = (iter.offset >> NODE_SHIFT) * objs_per_page;
                    if i == ARY {
                        continue 'restart;
                    }
                }
            }

            return unsafe { n.cast::<u8>().add(iter.offset & (NODE_SIZE - 1)) };
        }
    }

    fn iter_peek_prev(&self, iter: &mut IterPos, objs_per_page: usize,
                      obj_size_plus_page_remainder: usize) -> *mut u8 {
        if iter.offset == usize::MAX {
            return ptr::null_mut();
        }

        'restart: loop {
            let r = rcu_dereference(&self.root);
            if r.is_null() {
                return ptr::null_mut();
            }

            let mut n     = root_to_node(r);
            let mut level = root_to_depth(r);

            if !holds(level, iter.offset) {
                iter.offset = depth_size(level);
                iter.pos    = (iter.offset >> NODE_SHIFT) * objs_per_page;

                iter.offset -= obj_size_plus_page_remainder;
                iter.pos -= 1;
            }

            while level > 0 {
                level -= 1;

                let mut i = (iter.offset >> depth_shift(level)) & (ARY - 1);

                loop {
                    let child = rcu_dereference(unsafe { Node::child(n, i) });
                    if !child.is_null() {
                        n = child;
                        break;
                    }

                    let objs_per_ptr = depth_size(level);

                    iter.offset &= !(objs_per_ptr - 1);
                    iter.pos    = (iter.offset >> NODE_SHIFT) * objs_per_page;

                    if iter.offset == 0 {
                        return ptr::null_mut();
                    }

                    iter.offset -= obj_size_plus_page_remainder;
                    iter.pos -= 1;

                    if i == 0 {
                        continue 'restart;
                    }
                    i -= 1;
                }
            }

            return unsafe { n.cast::<u8>().add(iter.offset & (NODE_SIZE - 1)) };
        }
    }

    /// Free every node: empty after.
    ///
    /// # Safety
    /// Nothing is using it, nor any entry it handed out.
    unsafe fn free(&self) {
        fn free_recurse(n: *mut Node, level: u32) {
            if level > 0 {
                for i in 0..ARY {
                    let child = unsafe { Node::child(n, i) }.load(Relaxed);
                    if !child.is_null() {
                        free_recurse(child, level - 1);
                    }
                }
            }

            unsafe { Node::free(n) }
        }

        let r = self.root.swap(ptr::null_mut(), Acquire);
        if !r.is_null() {
            free_recurse(root_to_node(r), root_to_depth(r));
        }
    }
}

impl<T: Zeroable> GenRadix<T> {
    /// What C's BUILD_BUG_ON() in __idx_to_offset() says. Nodes are
    /// NODE_SIZE-aligned - kmalloc() of a power of two, in either build - and
    /// entries at multiples of T's size from a node's start, so every entry is
    /// aligned too.
    const FITS: () = assert!(size_of::<T>() != 0 && size_of::<T>() <= NODE_SIZE,
                             "GenRadix: an entry bigger than a node, or zero-sized");

    /// @idx's byte offset - as __idx_to_offset(), None past usize. An entry
    /// doesn't straddle nodes: for a size that doesn't divide NODE_SIZE, a
    /// node's tail is unused.
    #[inline]
    fn offset(idx: usize) -> Option<usize> {
        let () = Self::FITS;
        let size = size_of::<T>();

        if size.is_power_of_two() {
            idx.checked_mul(size)
        } else {
            let per_node = NODE_SIZE / size;
            (idx / per_node).checked_mul(NODE_SIZE)?.checked_add((idx % per_node) * size)
        }
    }

    /// The entry at @idx - None if its node was never allocated: as
    /// genradix_ptr().
    #[inline]
    pub fn get(&self, idx: usize) -> Option<&T> {
        let p = self.tree.ptr(Self::offset(idx)?);
        // SAFETY: an entry of an allocated node - a T, by Zeroable, if
        // nothing has written it
        NonNull::new(p.cast::<T>()).map(|p| unsafe { p.as_ref() })
    }

    /// get(), to edit.
    #[inline]
    pub fn get_mut(&mut self, idx: usize) -> Option<&mut T> {
        let p = self.tree.ptr(Self::offset(idx)?);
        // SAFETY: as get()'s, and &mut self
        NonNull::new(p.cast::<T>()).map(|mut p| unsafe { p.as_mut() })
    }

    /// The entry at @idx, allocating its node - zeroed - if it hasn't been:
    /// as genradix_ptr_alloc().
    #[inline]
    pub fn get_or_alloc(&mut self, idx: usize) -> Result<&mut T, AllocError> {
        let offset = Self::offset(idx).ok_or(AllocError)?;
        let gfp = current_gfp_context_raw(c::GFP_KERNEL);
        let p = self.tree.ptr_alloc(offset, None, gfp);
        // SAFETY: as get_mut()'s
        NonNull::new(p.cast::<T>()).map(|mut p| unsafe { p.as_mut() }).ok_or(AllocError)
    }

    /// get_or_alloc(), with @node for it to use, if it has one - taken if it
    /// does: as genradix_ptr_alloc_preallocated(), for a caller that
    /// allocates as it needs to outside a lock, and retries under it.
    #[inline]
    pub fn get_or_alloc_preallocated(&mut self, idx: usize, node: &mut NewNode, gfp: c::gfp_t)
        -> Result<&mut T, AllocError>
    {
        let offset = Self::offset(idx).ok_or(AllocError)?;
        let gfp = current_gfp_context_raw(gfp);
        let p = self.tree.ptr_alloc(offset, Some(node), gfp);
        // SAFETY: as get_mut()'s
        NonNull::new(p.cast::<T>()).map(|mut p| unsafe { p.as_mut() }).ok_or(AllocError)
    }

    /// The entries from @idx on, with their indices - every entry of each
    /// allocated node, written or not: as genradix_for_each_from().
    #[inline]
    pub fn iter_from(&self, idx: usize) -> Iter<'_, T> {
        Iter(RawIter::new(&self.tree, idx))
    }

    /// iter_from(), the entries as pointers: for one C writes as it's read.
    #[inline]
    pub fn iter_from_raw(&self, idx: usize) -> RawIter<'_, T> {
        RawIter::new(&self.tree, idx)
    }

    /// iter_from(0)'s entries, last first: as genradix_for_each_reverse().
    #[inline]
    pub fn iter_rev(&self) -> RevIter<'_, T> {
        RevIter(RawRevIter::new(&self.tree))
    }

    #[inline]
    pub fn iter_mut_from(&mut self, idx: usize) -> impl Iterator<Item = (usize, &mut T)> + '_ {
        RawIter::<T>::new(&self.tree, idx).map(|(pos, mut p)| (pos, unsafe { p.as_mut() }))
    }

    #[inline]
    pub fn iter_rev_mut(&mut self) -> impl Iterator<Item = (usize, &mut T)> + '_ {
        RawRevIter::<T>::new(&self.tree).map(|(pos, mut p)| (pos, unsafe { p.as_mut() }))
    }

    /// Free every node: as genradix_free(). Empty after, as a new one is.
    pub fn free(&mut self) {
        unsafe { self.tree.free() }
    }
}

/// A node for get_or_alloc_preallocated(), allocated ahead of it - zeroed,
/// as genradix_alloc_node() - and freed if it's never taken.
pub struct NewNode(*mut Node);

impl NewNode {
    pub const fn none() -> Self {
        NewNode(ptr::null_mut())
    }

    /// GFP_KERNEL - and with @nofail, __GFP_NOFAIL.
    pub fn alloc(&mut self, nofail: bool) -> Result<(), AllocError> {
        let n = if nofail {
            kzalloc_nofail(NODE_SIZE).cast()
        } else {
            Node::alloc(current_gfp_context_raw(c::GFP_KERNEL))
        };
        if n.is_null() {
            return Err(AllocError);
        }
        *self = NewNode(n);
        Ok(())
    }
}

impl Drop for NewNode {
    fn drop(&mut self) {
        unsafe { Node::free(self.0) }
    }
}

/// A GenRadix allocated through shared references, from several threads at
/// once: Tree::ptr_alloc() is safe against concurrent allocation and lookup,
/// and a node never moves. Its entries are shared, then: a T is Sync, and what
/// changes it is its own - a lock in it, atomics, a LockedBy. Freed on drop.
#[repr(transparent)]
pub struct SharedGenRadix<T>(UnsafeCell<GenRadix<T>>);

// SAFETY: allocation and lookup are safe from any thread; the entries are had
// shared, so T: Sync
unsafe impl<T: Send> Send for SharedGenRadix<T> {}
unsafe impl<T: Send + Sync> Sync for SharedGenRadix<T> {}

// SAFETY: zeroed is empty, as GenRadix's is
unsafe impl<T> Zeroable for SharedGenRadix<T> {}

impl<T> SharedGenRadix<T> {
    /// Empty: as genradix_init().
    pub fn new() -> Self {
        Zeroable::zeroed()
    }

    /// Free every node, before drop: empty after, as new. As genradix_free().
    ///
    /// # Safety
    /// Nothing is using it, nor any entry it handed out.
    pub unsafe fn free(&self) {
        unsafe { (*self.0.get()).tree.free() }
    }
}

impl<T: Zeroable> SharedGenRadix<T> {
    fn tree(&self) -> &Tree {
        unsafe { &(*self.0.get()).tree }
    }

    /// Allocate @idx's node, zeroed, if it hasn't been - as
    /// genradix_ptr_alloc(), without the entry.
    #[inline]
    pub fn prealloc(&self, idx: usize) -> Result<(), AllocError> {
        let offset = GenRadix::<T>::offset(idx).ok_or(AllocError)?;
        let gfp = current_gfp_context_raw(c::GFP_KERNEL);
        let p = self.tree().ptr_alloc(offset, None, gfp);
        if p.is_null() { Err(AllocError) } else { Ok(()) }
    }

    /// Every entry below @nr allocated, zeroed, if it hasn't been - as
    /// genradix_prealloc(): prealloc() of one entry in each node.
    pub fn prealloc_n(&self, nr: usize) -> Result<(), AllocError> {
        (0..nr).step_by(NODE_SIZE / size_of::<T>()).try_for_each(|idx| self.prealloc(idx))
    }

    /// The entry at @idx - None if its node was never allocated: as
    /// genradix_ptr(). A T, by Zeroable, if nothing has written it.
    #[inline]
    pub fn get(&self, idx: usize) -> Option<&T> {
        let p = self.tree().ptr(GenRadix::<T>::offset(idx)?);
        NonNull::new(p.cast::<T>()).map(|p| unsafe { p.as_ref() })
    }

    /// GenRadix::iter_from()'s.
    #[inline]
    pub fn iter_from(&self, idx: usize) -> Iter<'_, T> {
        Iter(RawIter::new(self.tree(), idx))
    }
}

impl<T> Drop for SharedGenRadix<T> {
    fn drop(&mut self) {
        unsafe { self.0.get_mut().tree.free() }
    }
}

/// GenRadix::iter_from()'s walk, the entries as pointers.
pub struct RawIter<'a, T> {
    tree: &'a Tree,
    iter: IterPos,
    _t:   PhantomData<&'a T>,
}

impl<'a, T: Zeroable> RawIter<'a, T> {
    #[inline]
    fn new(tree: &'a Tree, idx: usize) -> Self {
        // genradix_iter_init(): an offset past usize is the end
        let offset = GenRadix::<T>::offset(idx).unwrap_or(usize::MAX);
        RawIter { tree, iter: IterPos { offset, pos: idx }, _t: PhantomData }
    }
}

pub struct Iter<'a, T>(RawIter<'a, T>);

impl<'a, T: Zeroable> Iterator for Iter<'a, T> {
    type Item = (usize, &'a T);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        // SAFETY: as GenRadix::get()'s
        self.0.next().map(|(pos, p)| (pos, unsafe { p.as_ref() }))
    }
}

impl<T: Zeroable> Iterator for RawIter<'_, T> {
    type Item = (usize, NonNull<T>);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        let size = size_of::<T>();

        // genradix_iter_peek(): which skips unallocated nodes, moving the
        // iterator to the next allocated one
        let p = self.tree.iter_peek(&mut self.iter, NODE_SIZE / size);
        let item = (self.iter.pos, NonNull::new(p.cast::<T>())?);

        // genradix_iter_advance(): past the node's last whole entry, to the
        // next node
        let next = self.iter.offset.checked_add(size).and_then(|offset| {
            if !size.is_power_of_two() && (offset & (NODE_SIZE - 1)) + size > NODE_SIZE {
                offset.checked_next_multiple_of(NODE_SIZE)
            } else {
                Some(offset)
            }
        });
        match next {
            Some(offset) => {
                self.iter.offset = offset;
                self.iter.pos += 1;
            }
            None => {
                self.iter.offset = usize::MAX;
                self.iter.pos    = usize::MAX;
            }
        }

        Some(item)
    }
}

/// GenRadix::iter_rev()'s walk.
pub struct RawRevIter<'a, T> {
    tree: &'a Tree,
    iter: IterPos,
    _t:   PhantomData<&'a T>,
}

impl<'a, T: Zeroable> RawRevIter<'a, T> {
    #[inline]
    fn new(tree: &'a Tree) -> Self {
        // genradix_last_pos(): the last index an offset can hold
        let pos = usize::MAX / NODE_SIZE * (NODE_SIZE / size_of::<T>()) - 1;
        let offset = GenRadix::<T>::offset(pos).expect("the last position's offset fits");
        RawRevIter { tree, iter: IterPos { offset, pos }, _t: PhantomData }
    }
}

pub struct RevIter<'a, T>(RawRevIter<'a, T>);

impl<'a, T: Zeroable> Iterator for RevIter<'a, T> {
    type Item = (usize, &'a T);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        // SAFETY: as GenRadix::get()'s
        self.0.next().map(|(pos, p)| (pos, unsafe { p.as_ref() }))
    }
}

impl<T: Zeroable> Iterator for RawRevIter<'_, T> {
    type Item = (usize, NonNull<T>);

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        let size = size_of::<T>();

        // genradix_iter_peek_prev(): which skips unallocated nodes, moving the
        // iterator to the last entry of the previous allocated one
        let p = self.tree.iter_peek_prev(&mut self.iter, NODE_SIZE / size, size + NODE_SIZE % size);
        let item = (self.iter.pos, NonNull::new(p.cast::<T>())?);

        // genradix_iter_rewind(): back over a node's unused tail, to the
        // previous node's last whole entry
        if self.iter.offset == 0 || self.iter.offset == usize::MAX {
            self.iter.offset = usize::MAX;
        } else {
            if self.iter.offset & (NODE_SIZE - 1) == 0 {
                self.iter.offset -= NODE_SIZE % size;
            }
            self.iter.offset -= size;
            self.iter.pos -= 1;
        }

        Some(item)
    }
}
