// SPDX-License-Identifier: GPL-2.0

//! Generic radix trees/sparse arrays: GenRadix<T>, a sparse array of T,
//! allocated a node at a time and zeroed, whose entries never move once they
//! exist.
//!
//! The radix tree allocates a node of entries at a time, so entries may exist
//! that were never explicitly allocated - they will be initialized to all
//! zeroes. So an entry type is Zeroable, and says what its leaf is - a node's
//! worth of entries - with genradix_entry!().
//!
//! There's a tree per depth: tree d holds the leaves numbered below ARY^d
//! that tree d - 1 doesn't, under d levels of interior nodes. So an index is
//! as few steps away as its size needs, and no tree grows by moving its root
//! under a new one. A link is a OnceEither, to an interior node or a leaf:
//! set once, from any thread, then never moved.
//!
//! Allocating takes &mut - two callers allocating one index would otherwise
//! each hold a &mut T. SharedGenRadix, for one allocated from several threads
//! at once, has its entries shared.

use crate::data_structures::array_vec::ArrayVec;
use crate::util::alloc::{AllocError, AllocGfp, Box, Flags, Kmalloc};
use crate::util::once_either::{Either, OnceEither};
use core::iter::Enumerate;
use core::marker::PhantomData;
use core::mem::size_of;
use core::slice;
use pin_init::{init_zeroed, Zeroable};

pub const NODE_SIZE: usize = 512;

const ARY: usize = NODE_SIZE / size_of::<usize>();
const ARY_SHIFT: usize = ARY.ilog2() as usize;

/* trees 0 to the depth a usize leaf number needs */
const ROOTS: usize = (usize::BITS as usize).div_ceil(ARY_SHIFT) + 1;

/// A GenRadix's entry type: Zeroable, as a node's entries are before anything
/// writes them, and its Leaf, genradix_entry!()'s - a node's worth.
pub trait Entry: Zeroable + Sized {
    type Leaf: AsRef<[Self]> + AsMut<[Self]> + Zeroable;
}

/// Each type, an Entry of a GenRadix: its leaf, as many as fit in a node.
#[macro_export]
macro_rules! genradix_entry {
    ($($t:ty),+ $(,)?) => {$(
        impl $crate::data_structures::genradix::Entry for $t {
            type Leaf = [$t; $crate::data_structures::genradix::NODE_SIZE / ::core::mem::size_of::<$t>()];
        }
    )+};
}

const fn per_leaf<T: Entry>() -> usize {
    let n = size_of::<T::Leaf>() / size_of::<T>();
    assert!(n > 0 && size_of::<T::Leaf>() == n * size_of::<T>(),
            "GenRadix: a leaf that isn't an array of entries, or an entry bigger than a node");
    n
}

/// A leaf: its entries, aligned so a link can say it's one.
#[repr(C, align(2))]
#[derive(Zeroable)]
struct Leaf<T: Entry>(T::Leaf);

#[repr(C)]
#[derive(Zeroable)]
struct Interior<T: Entry> {
    children: [Link<T>; ARY],
}

type Link<T> = OnceEither<Interior<T>, Leaf<T>>;

fn interior<I, L>(e: Either<I, L>) -> I {
    match e {
        Either::Left(i)  => i,
        Either::Right(_) => panic!("genradix: a leaf where an interior node goes"),
    }
}

fn leaf<I, L>(e: Either<I, L>) -> L {
    match e {
        Either::Right(l) => l,
        Either::Left(_)  => panic!("genradix: an interior node where a leaf goes"),
    }
}

/// Zeroed.
fn alloc_node<N: Zeroable>(gfp: Flags) -> Result<Box<N, Kmalloc>, AllocError> {
    Box::init_in(init_zeroed(), Kmalloc.with_flags(gfp)).map(Box::into_unflagged)
}

/// @idx's leaf number, its place in the leaf, and its tree: the depth its
/// leaf number needs.
fn split<T: Entry>(idx: usize) -> (usize, usize, usize) {
    let q = idx / per_leaf::<T>();
    (q, idx % per_leaf::<T>(), ((usize::BITS - q.leading_zeros()) as usize).div_ceil(ARY_SHIFT))
}

/// The child of a node at @height that leaf @q is under.
fn child(q: usize, height: usize) -> usize {
    (q >> (ARY_SHIFT * (height - 1))) & (ARY - 1)
}

/// Nodes for get_or_alloc_preallocated(), allocated ahead of it - zeroed, as
/// genradix_alloc_node() - and freed if they're never taken: a leaf and an
/// interior node. An allocation that needs more takes what's there, and
/// fails for the rest only if it can't allocate them.
pub struct NewNode<T: Entry> {
    interior: Option<Box<Interior<T>, Kmalloc>>,
    leaf:     Option<Box<Leaf<T>, Kmalloc>>,
}

impl<T: Entry> NewNode<T> {
    pub const fn none() -> Self {
        NewNode { interior: None, leaf: None }
    }

    /// What's been taken, allocated again.
    pub fn alloc(&mut self) -> Result<(), AllocError> {
        if self.interior.is_none() {
            self.interior = Some(alloc_node(Flags::empty())?);
        }
        if self.leaf.is_none() {
            self.leaf = Some(alloc_node(Flags::empty())?);
        }
        Ok(())
    }

    fn interior(&mut self, gfp: Flags) -> Result<Box<Interior<T>, Kmalloc>, AllocError> {
        self.interior.take().map_or_else(|| alloc_node(gfp), Ok)
    }

    fn leaf(&mut self, gfp: Flags) -> Result<Box<Leaf<T>, Kmalloc>, AllocError> {
        self.leaf.take().map_or_else(|| alloc_node(gfp), Ok)
    }
}

/// The trees: GenRadix's and SharedGenRadix's, which differ in how they
/// allocate.
#[repr(transparent)]
#[derive(Zeroable)]
struct Trees<T: Entry> {
    roots: [Link<T>; ROOTS],
}

impl<T: Entry> Trees<T> {
    fn get(&self, idx: usize) -> Option<&T> {
        let (q, r, d) = split::<T>(idx);
        let mut link = self.roots.get(d)?;
        for h in (1..=d).rev() {
            link = &interior(link.get()?).children[child(q, h)];
        }
        Some(&leaf(link.get()?).0.as_ref()[r])
    }

    fn get_mut(&mut self, idx: usize) -> Option<&mut T> {
        let (q, r, d) = split::<T>(idx);
        let mut link = self.roots.get_mut(d)?;
        for h in (1..=d).rev() {
            link = &mut interior(link.get_mut()?).children[child(q, h)];
        }
        Some(&mut leaf(link.get_mut()?).0.as_mut()[r])
    }

    /// @idx, allocating its nodes, zeroed, if they haven't been - from @spare
    /// first, then with @gfp.
    fn get_or_alloc(&mut self, idx: usize, spare: &mut NewNode<T>, gfp: Flags) -> Result<&mut T, AllocError> {
        let (q, r, d) = split::<T>(idx);
        let mut link = self.roots.get_mut(d).ok_or(AllocError)?;
        for h in (1..=d).rev() {
            link = &mut interior(link.get_mut_or_set_left(|| spare.interior(gfp))?).children[child(q, h)];
        }
        Ok(&mut leaf(link.get_mut_or_set_right(|| spare.leaf(gfp))?).0.as_mut()[r])
    }

    /// get_or_alloc(), by &self: what another thread allocated first is
    /// taken, and what lost to it freed.
    fn alloc_shared(&self, idx: usize, gfp: Flags) -> Result<(), AllocError> {
        let (q, _, d) = split::<T>(idx);
        let mut link = self.roots.get(d).ok_or(AllocError)?;
        for h in (1..=d).rev() {
            let e = match link.get() {
                Some(e) => e,
                None    => link.get_or_set_left(alloc_node(gfp)?).0,
            };
            link = &interior(e).children[child(q, h)];
        }
        match link.get() {
            Some(e) => leaf(e),
            None    => leaf(link.get_or_set_right(alloc_node(gfp)?).0),
        };
        Ok(())
    }

    fn iter_from(&self, idx: usize) -> Walk<'_, T, &Link<T>, false> {
        Walk::new(self.roots.iter(), idx)
    }

    fn iter_rev(&self) -> Walk<'_, T, &Link<T>, true> {
        Walk::new(self.roots.iter(), 0)
    }

    fn iter_mut_from(&mut self, idx: usize) -> Walk<'_, T, &mut Link<T>, false> {
        Walk::new(self.roots.iter_mut(), idx)
    }

    fn iter_rev_mut(&mut self) -> Walk<'_, T, &mut Link<T>, true> {
        Walk::new(self.roots.iter_mut(), 0)
    }
}

/// A link, shared or not: what a walk opens it to.
trait Access<'a>: Sized {
    type Entry;
    type Links: DoubleEndedIterator<Item = Self> + ExactSizeIterator;
    type Entries: DoubleEndedIterator<Item = Self::Entry> + ExactSizeIterator;

    fn open(self) -> Option<Either<Self::Links, Self::Entries>>;
}

impl<'a, T: Entry> Access<'a> for &'a Link<T> {
    type Entry = &'a T;
    type Links = slice::Iter<'a, Link<T>>;
    type Entries = slice::Iter<'a, T>;

    fn open(self) -> Option<Either<Self::Links, Self::Entries>> {
        Some(match self.get()? {
            Either::Left(i)  => Either::Left(i.children.iter()),
            Either::Right(l) => Either::Right(l.0.as_ref().iter()),
        })
    }
}

impl<'a, T: Entry> Access<'a> for &'a mut Link<T> {
    type Entry = &'a mut T;
    type Links = slice::IterMut<'a, Link<T>>;
    type Entries = slice::IterMut<'a, T>;

    fn open(self) -> Option<Either<Self::Links, Self::Entries>> {
        Some(match self.get_mut()? {
            Either::Left(i)  => Either::Left(i.children.iter_mut()),
            Either::Right(l) => Either::Right(l.0.as_mut().iter_mut()),
        })
    }
}

fn step<I: DoubleEndedIterator, const REV: bool>(it: &mut I) -> Option<I::Item> {
    if REV { it.next_back() } else { it.next() }
}

/// A node being walked: the first leaf number under it, its height, and its
/// children.
struct Frame<I> {
    q:      usize,
    height: usize,
    it:     Enumerate<I>,
}

/// Every entry of each allocated leaf, written or not, in index order - or
/// reversed, from the last - and forward, from @start.
struct Walk<'a, T: Entry, A: Access<'a>, const REV: bool> {
    start:  usize,
    roots:  Enumerate<A::Links>,
    frames: ArrayVec<Frame<A::Links>, ROOTS>,
    leaf:   Option<(usize, Enumerate<A::Entries>)>,
    _t:     PhantomData<&'a T>,
}

impl<'a, T: Entry, A: Access<'a>, const REV: bool> Walk<'a, T, A, REV> {
    fn new(roots: A::Links, start: usize) -> Self {
        Walk { start, roots: roots.enumerate(), frames: ArrayVec::new(), leaf: None, _t: PhantomData }
    }
}

impl<'a, T: Entry, A: Access<'a>, const REV: bool> Iterator for Walk<'a, T, A, REV> {
    type Item = (usize, A::Entry);

    fn next(&mut self) -> Option<Self::Item> {
        let start_q = self.start / per_leaf::<T>();

        loop {
            if let Some((base, entries)) = &mut self.leaf {
                match step::<_, REV>(entries) {
                    Some((r, e)) if REV || *base + r >= self.start => return Some((*base + r, e)),
                    Some(_) => continue,
                    None    => self.leaf = None,
                }
            }

            let (q, height, link) = match self.frames.last_mut() {
                Some(f) => match step::<_, REV>(&mut f.it) {
                    Some((c, link)) => (f.q + (c << (ARY_SHIFT * (f.height - 1))), f.height - 1, link),
                    None => {
                        self.frames.pop();
                        continue;
                    }
                },
                None => {
                    let (d, link) = step::<_, REV>(&mut self.roots)?;
                    (0, d, link)
                }
            };

            /* forward, past what's all before @start */
            let prefix = |q: usize| q.checked_shr((ARY_SHIFT * height) as u32).unwrap_or(0);
            if !REV && prefix(q) < prefix(start_q) {
                continue;
            }

            match link.open() {
                None => {}
                Some(Either::Left(children)) => {
                    assert!(height > 0, "genradix: an interior node where a leaf goes");
                    self.frames.push(Frame { q, height, it: children.enumerate() });
                }
                Some(Either::Right(entries)) => {
                    assert!(height == 0, "genradix: a leaf where an interior node goes");
                    self.leaf = Some((q * per_leaf::<T>(), entries.enumerate()));
                }
            }
        }
    }
}

/// A sparse array of T, allocated by &mut.
#[repr(transparent)]
#[derive(Zeroable)]
pub struct GenRadix<T: Entry>(Trees<T>);

impl<T: Entry> GenRadix<T> {
    /// The entry at @idx - None if its node was never allocated: as
    /// genradix_ptr().
    #[inline]
    pub fn get(&self, idx: usize) -> Option<&T> {
        self.0.get(idx)
    }

    #[inline]
    pub fn get_mut(&mut self, idx: usize) -> Option<&mut T> {
        self.0.get_mut(idx)
    }

    /// The entry at @idx, allocating its node - zeroed - if it hasn't been:
    /// as genradix_ptr_alloc().
    #[inline]
    pub fn get_or_alloc(&mut self, idx: usize) -> Result<&mut T, AllocError> {
        self.0.get_or_alloc(idx, &mut NewNode::none(), Flags::empty())
    }

    /// get_or_alloc(), from @node first, then with @gfp: as
    /// genradix_ptr_alloc_preallocated(), for a caller that allocates as it
    /// needs to outside a lock, and retries under it.
    #[inline]
    pub fn get_or_alloc_preallocated(&mut self, idx: usize, node: &mut NewNode<T>, gfp: Flags)
        -> Result<&mut T, AllocError>
    {
        self.0.get_or_alloc(idx, node, gfp)
    }

    /// The entries from @idx on, with their indices - every entry of each
    /// allocated node, written or not: as genradix_for_each_from().
    pub fn iter_from(&self, idx: usize) -> impl Iterator<Item = (usize, &T)> {
        self.0.iter_from(idx)
    }

    /// iter_from(0)'s entries, last first: as genradix_for_each_reverse().
    pub fn iter_rev(&self) -> impl Iterator<Item = (usize, &T)> {
        self.0.iter_rev()
    }

    pub fn iter_mut_from(&mut self, idx: usize) -> impl Iterator<Item = (usize, &mut T)> {
        self.0.iter_mut_from(idx)
    }

    pub fn iter_rev_mut(&mut self) -> impl Iterator<Item = (usize, &mut T)> {
        self.0.iter_rev_mut()
    }

    /// Free every node: as genradix_free(). Empty after, as a new one is.
    pub fn free(&mut self) {
        self.0 = Zeroable::zeroed();
    }
}

/// A GenRadix allocated through shared references, from several threads at
/// once: a link is set once, by whichever thread gets there first, and a node
/// never moves. Its entries are shared, then: what changes one is its own - a
/// lock in it, atomics, a LockedBy.
#[repr(transparent)]
#[derive(Zeroable)]
pub struct SharedGenRadix<T: Entry>(Trees<T>);

impl<T: Entry> SharedGenRadix<T> {
    /// Empty: as genradix_init().
    pub fn new() -> Self {
        Zeroable::zeroed()
    }

    /// Free every node, before drop: empty after, as new. As genradix_free().
    ///
    /// # Safety
    /// Nothing is using it, nor any entry it handed out.
    pub unsafe fn free(&self) {
        for r in &self.0.roots {
            drop(unsafe { r.take_shared() });
        }
    }

    /// Allocate @idx's node, zeroed, if it hasn't been - as
    /// genradix_ptr_alloc(), without the entry.
    #[inline]
    pub fn prealloc(&self, idx: usize) -> Result<(), AllocError> {
        self.0.alloc_shared(idx, Flags::empty())
    }

    /// Every entry below @nr allocated, zeroed, if it hasn't been - as
    /// genradix_prealloc(): prealloc() of one entry in each node.
    pub fn prealloc_n(&self, nr: usize) -> Result<(), AllocError> {
        (0..nr).step_by(per_leaf::<T>()).try_for_each(|idx| self.prealloc(idx))
    }

    /// The entry at @idx - None if its node was never allocated: as
    /// genradix_ptr().
    #[inline]
    pub fn get(&self, idx: usize) -> Option<&T> {
        self.0.get(idx)
    }

    /// GenRadix::iter_from()'s.
    pub fn iter_from(&self, idx: usize) -> impl Iterator<Item = (usize, &T)> {
        self.0.iter_from(idx)
    }
}

#[cfg(test)]
mod tests {
    use super::{per_leaf, GenRadix, SharedGenRadix, ARY};

    crate::genradix_entry!(u64);

    /// Indices either side of each tree's start, and some way into it.
    fn indices() -> impl Iterator<Item = usize> {
        let per = per_leaf::<u64>();
        (0..5u32).flat_map(move |d| {
            let start = per * ARY.pow(d);
            [start - 1, start, start + 1, start * 3 + 7]
        })
    }

    #[test]
    fn get_and_walk() {
        let mut g: GenRadix<u64> = pin_init::Zeroable::zeroed();
        let mut want: Vec<usize> = indices().collect();
        want.sort_unstable();
        want.dedup();

        for &i in &want {
            *g.get_or_alloc(i).unwrap() = i as u64 + 1;
        }
        for &i in &want {
            assert_eq!(g.get(i).copied(), Some(i as u64 + 1), "get({i})");
        }

        // a walk sees every entry of each allocated leaf: the written ones
        // are ours, in order
        let fwd: Vec<usize> = g.iter_from(0).filter(|(_, v)| **v != 0).map(|(i, v)| {
            assert_eq!(*v, i as u64 + 1);
            i
        }).collect();
        assert_eq!(fwd, want);

        let mut rev: Vec<usize> = g.iter_rev().filter(|(_, v)| **v != 0).map(|(i, _)| i).collect();
        rev.reverse();
        assert_eq!(rev, want);

        for &start in &want {
            let from: Vec<usize> = g.iter_from(start).filter(|(_, v)| **v != 0).map(|(i, _)| i).collect();
            let expect: Vec<usize> = want.iter().copied().filter(|&i| i >= start).collect();
            assert_eq!(from, expect, "iter_from({start})");
        }

        for (i, v) in g.iter_mut_from(0) {
            if *v != 0 {
                *v = i as u64 * 2;
            }
        }
        assert!(g.iter_rev_mut().all(|(i, v)| *v == 0 || *v == i as u64 * 2));

        g.free();
        assert!(g.get(want[0]).is_none() && g.iter_from(0).next().is_none());
    }

    #[test]
    fn shared() {
        let g: SharedGenRadix<u64> = SharedGenRadix::new();
        g.prealloc_n(per_leaf::<u64>() * ARY + 1).unwrap();
        assert!(g.get(per_leaf::<u64>() * ARY).is_some());
        assert!(g.get(per_leaf::<u64>() * ARY * 2).is_none());
        assert_eq!(g.iter_from(0).count(), per_leaf::<u64>() * (ARY + 1));
    }
}
