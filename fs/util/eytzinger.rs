// SPDX-License-Identifier: GPL-2.0

//! Eytzinger layout: a complete binary tree in an array, root first, then
//! each level in turn - node i's children at 2i and 2i + 1. The C
//! (util/eytzinger.[ch]) stays until its C users are gone.
//!
//! In place of binary search over a sorted array: binary search is a worst
//! case for branch prediction and prefetching, while here a node's children
//! are adjacent in memory, so they can be fetched before the comparison
//! resolves - several levels' worth, when several nodes fit on a cacheline.
//!
//! A tree's index math is a method of its shape: Eytzinger1<T>, of a size,
//! for every unsigned T it's instantiated for - and its `extra`, which the
//! inorder conversions take, computed once at new(), where C's callers cache
//! it by hand for the __eytzinger1_*() variants.
//!
//! Two variants, by index base:
//! - Eytzinger1: nodes 1..=size, slot 0 of an array unused. Each level
//!   starts at a power of two, so if the array is cacheline aligned, every
//!   level is: the faster one. 0 is "no node", as in C.
//! - Eytzinger0: nodes 0..size, every slot a node: the more convenient one.
//!   The same tree, every index one less - each method Eytzinger1's on
//!   i + 1, and None where C says -1.
//!
//! Searching and sorting are over slices - Eytzinger1::find_by(), sort_by(),
//! Eytzinger0::find_le_by() and the rest - with the slice methods'
//! comparisons: a find's closure takes an element and says how it orders
//! against what's searched for, as binary_search_by()'s; a sort's compares
//! two elements, as sort_by()'s. The C's cmp/swap function pointers, priv
//! wrappers and alignment-picked word swaps are what closures and
//! slice::swap() are.

use core::cmp::Ordering;

/// A one-based eytzinger tree of `size` nodes.
#[derive(Clone, Copy, Debug)]
pub struct Eytzinger1<T> {
    size:  T,
    /// Twice the nodes on the bottom level: past it, inorder positions skip
    /// the missing leaves of an incomplete tree.
    extra: T,
}

/// A zero-based eytzinger tree of `size` nodes.
#[derive(Clone, Copy, Debug)]
pub struct Eytzinger0<T>(Eytzinger1<T>);

macro_rules! eytzinger_impl {
    ($t:ty) => {
        impl Eytzinger1<$t> {
            /// The index of @x's highest set bit; @x nonzero. C's __fls().
            const fn fls(x: $t) -> u32 {
                <$t>::BITS - 1 - x.leading_zeros()
            }

            /// A tree of @size nodes: at most half the type's range, as the
            /// inorder conversions reach 2 * size.
            pub const fn new(size: $t) -> Self {
                debug_assert!(size <= <$t>::MAX / 2);
                let extra = if size != 0 { (size + 1 - (1 << Self::fls(size))) << 1 } else { 0 };
                Self { size, extra }
            }

            pub const fn size(&self) -> $t {
                self.size
            }

            /// Node @i's left (@dir 0) or right (@dir 1) child.
            pub fn child(i: $t, dir: $t) -> $t {
                debug_assert!(dir <= 1);
                (i << 1) + dir
            }

            pub fn left_child(i: $t) -> $t {
                Self::child(i, 0)
            }

            pub fn right_child(i: $t) -> $t {
                Self::child(i, 1)
            }

            /// The first node in order, the leftmost; 0 for an empty tree.
            pub fn first(&self) -> $t {
                if self.size != 0 { 1 << Self::fls(self.size) } else { 0 }
            }

            /// The last node in order, the rightmost; 0 for an empty tree.
            pub fn last(&self) -> $t {
                (1 << Self::fls(self.size + 1)) - 1
            }

            /// The node after @i in order; 0 after the last.
            pub fn next(&self, mut i: $t) -> $t {
                debug_assert!(i != 0 && i <= self.size);

                if Self::right_child(i) <= self.size {
                    i = Self::right_child(i);

                    i <<= Self::fls(self.size) - Self::fls(i);
                    i >>= (i > self.size) as u32;
                } else {
                    i >>= i.trailing_ones() + 1;
                }
                i
            }

            /// The node before @i in order; 0 before the first.
            pub fn prev(&self, mut i: $t) -> $t {
                debug_assert!(i != 0 && i <= self.size);

                if Self::left_child(i) <= self.size {
                    i = Self::left_child(i) + 1;

                    i <<= Self::fls(self.size) - Self::fls(i);
                    i -= 1;
                    i >>= (i > self.size) as u32;
                } else {
                    i >>= i.trailing_zeros() + 1;
                }
                i
            }

            /// Node @i's inorder (sorted) position, 1-based.
            pub fn to_inorder(&self, mut i: $t) -> $t {
                debug_assert!(i != 0 && i <= self.size);

                let b = Self::fls(i);
                let shift = Self::fls(self.size) - b;

                i ^= 1 << b;
                i <<= 1;
                i |= 1;
                i <<= shift;

                // Past extra, only the levels above the bottom: shifted, so
                // even, as extra is - the halving is exact.
                if i > self.extra {
                    i -= (i - self.extra) >> 1;
                }
                i
            }

            /// The node at inorder position @i, 1-based.
            pub fn from_inorder(&self, mut i: $t) -> $t {
                debug_assert!(i != 0 && i <= self.size);

                if i > self.extra {
                    i += i - self.extra;
                }

                let shift = i.trailing_zeros();

                i >>= shift + 1;
                i |= 1 << (Self::fls(self.size) - shift);
                i
            }

            /// The nodes, in order.
            pub fn iter(&self) -> impl Iterator<Item = $t> {
                let e = *self;
                let node = |i: $t| (i != 0).then_some(i);
                core::iter::successors(node(e.first()), move |&i| node(e.next(i)))
            }
        }

        impl Eytzinger0<$t> {
            pub const fn new(size: $t) -> Self {
                Self(Eytzinger1::<$t>::new(size))
            }

            pub const fn size(&self) -> $t {
                self.0.size
            }

            /// Node @i's left (@dir 0) or right (@dir 1) child.
            pub fn child(i: $t, dir: $t) -> $t {
                debug_assert!(dir <= 1);
                (i << 1) + 1 + dir
            }

            pub fn left_child(i: $t) -> $t {
                Self::child(i, 0)
            }

            pub fn right_child(i: $t) -> $t {
                Self::child(i, 1)
            }

            pub fn first(&self) -> Option<$t> {
                self.0.first().checked_sub(1)
            }

            pub fn last(&self) -> Option<$t> {
                self.0.last().checked_sub(1)
            }

            pub fn next(&self, i: $t) -> Option<$t> {
                self.0.next(i + 1).checked_sub(1)
            }

            pub fn prev(&self, i: $t) -> Option<$t> {
                self.0.prev(i + 1).checked_sub(1)
            }

            /// Node @i's inorder (sorted) position, 0-based.
            pub fn to_inorder(&self, i: $t) -> $t {
                self.0.to_inorder(i + 1) - 1
            }

            /// The node at inorder position @i, 0-based.
            pub fn from_inorder(&self, i: $t) -> $t {
                self.0.from_inorder(i + 1) - 1
            }

            /// The nodes, in order.
            pub fn iter(&self) -> impl Iterator<Item = $t> {
                let e = *self;
                core::iter::successors(e.first(), move |&i| e.next(i))
            }

            /// The nodes, in reverse order.
            pub fn iter_rev(&self) -> impl Iterator<Item = $t> {
                let e = *self;
                core::iter::successors(e.last(), move |&i| e.prev(i))
            }
        }
    };
}

eytzinger_impl!(u16);
eytzinger_impl!(u32);
eytzinger_impl!(usize);

/// The @n nodes stored in @v from @base - index pos - base is Eytzinger1 node
/// pos - sorted into eytzinger order, as eytzinger1_sort_r(): compared and
/// swapped by index, @v whatever holds them.
fn sort_impl<S: ?Sized>(
    v:        &mut S,
    n:        usize,
    base:     usize,
    mut cmp:  impl FnMut(&S, usize, usize) -> Ordering,
    mut swap: impl FnMut(&mut S, usize, usize),
) {
    let e = Eytzinger1::<usize>::new(n);
    let pos = |inorder: usize| e.from_inorder(inorder) - base;

    // Heapsort over inorder positions 1..=n, each stored at its eytzinger
    // position - so when the inorder sequence is sorted, the tree is.
    let mut gt = |v: &S, l: usize, r: usize| cmp(v, pos(l), pos(r));
    let mut swap = |v: &mut S, l: usize, r: usize| swap(v, pos(l), pos(r));

    // heapify
    for i in (1..=n / 2).rev() {
        // Find the sift-down path all the way to the leaves.
        let mut j = i;
        while j * 2 < n {
            let k = j * 2;
            j = if gt(v, k, k + 1) == Ordering::Greater { k } else { k + 1 };
        }

        // Special case for the last leaf with no sibling.
        if j * 2 == n {
            j *= 2;
        }

        // Backtrack to the correct location.
        while j != i && gt(v, i, j) != Ordering::Less {
            j /= 2;
        }

        // Shift the element into its correct place.
        let k = j;
        while j != i {
            j /= 2;
            swap(v, j, k);
        }

        crate::util::kernel::cond_resched();
    }

    // sort
    for i in (2..=n).rev() {
        swap(v, 1, i);

        // Find the sift-down path all the way to the leaves.
        let mut j = 1;
        while j * 2 + 1 < i {
            let k = j * 2;
            j = if gt(v, k, k + 1) == Ordering::Greater { k } else { k + 1 };
        }

        // Special case for the last leaf with no sibling.
        if j * 2 + 1 == i {
            j *= 2;
        }

        // Backtrack to the correct location.
        while j >= 1 && gt(v, 1, j) != Ordering::Less {
            j /= 2;
        }

        // Shift the element into its correct place.
        let k = j;
        while j > 1 {
            j /= 2;
            swap(v, j, k);
        }

        crate::util::kernel::cond_resched();
    }
}

fn sort_by_impl<T>(v: &mut [T], n: usize, base: usize, mut cmp: impl FnMut(&T, &T) -> Ordering) {
    sort_impl(v, n, base, |v, l, r| cmp(&v[l], &v[r]), |v, l, r| v.swap(l, r))
}

/// The node of @n stored from @base that @f - given its index, pos - base -
/// says is the one searched for (Equal), by Eytzinger1 position.
fn find_impl(n: usize, base: usize, mut f: impl FnMut(usize) -> Ordering) -> Option<usize> {
    let mut i = 1;
    while i <= n {
        match f(i - base) {
            Ordering::Equal => return Some(i),
            o => i = Eytzinger1::<usize>::child(i, (o == Ordering::Less) as usize),
        }
    }
    None
}

impl Eytzinger1<usize> {
    /// The tree in @v: one node less than its slots, for the unused slot 0.
    pub fn of<T>(v: &[T]) -> Self {
        Self::new(v.len().saturating_sub(1))
    }

    /// The node of @v that @f says is the one searched for (Equal), if any.
    pub fn find_by<T>(v: &[T], mut f: impl FnMut(&T) -> Ordering) -> Option<usize> {
        find_impl(Self::of(v).size, 0, |i| f(&v[i]))
    }

    /// @v's nodes, slot 0 aside, sorted into eytzinger order.
    pub fn sort_by<T>(v: &mut [T], cmp: impl FnMut(&T, &T) -> Ordering) {
        sort_by_impl(v, Self::of(v).size, 0, cmp)
    }
}

impl Eytzinger0<usize> {
    /// The tree in @v: a node per slot.
    pub fn of<T>(v: &[T]) -> Self {
        Self::new(v.len())
    }

    /// The node of @v that @f says is the one searched for (Equal), if any.
    pub fn find_by<T>(v: &[T], mut f: impl FnMut(&T) -> Ordering) -> Option<usize> {
        Self::of(v).find(|i| f(&v[i]))
    }

    /// The greatest node of @v <= what @f compares against, if any.
    pub fn find_le_by<T>(v: &[T], mut f: impl FnMut(&T) -> Ordering) -> Option<usize> {
        let mut n = 1;
        while n <= v.len() {
            n = Eytzinger1::<usize>::child(n, (f(&v[n - 1]) != Ordering::Greater) as usize);
        }
        n >>= n.trailing_zeros() + 1;
        n.checked_sub(1)
    }

    /// The smallest node of @v > what @f compares against, if any.
    pub fn find_gt_by<T>(v: &[T], mut f: impl FnMut(&T) -> Ordering) -> Option<usize> {
        let mut n = 1;
        while n <= v.len() {
            n = Eytzinger1::<usize>::child(n, (f(&v[n - 1]) != Ordering::Greater) as usize);
        }
        n >>= (n + 1).trailing_zeros() + 1;
        n.checked_sub(1)
    }

    /// The smallest node of @v >= what @f compares against, if any.
    pub fn find_ge_by<T>(v: &[T], mut f: impl FnMut(&T) -> Ordering) -> Option<usize> {
        let mut n = 1;
        while n <= v.len() {
            n = Eytzinger1::<usize>::child(n, (f(&v[n - 1]) == Ordering::Less) as usize);
        }
        n >>= (n + 1).trailing_zeros() + 1;
        n.checked_sub(1)
    }

    /// @v sorted into eytzinger order.
    pub fn sort_by<T>(v: &mut [T], cmp: impl FnMut(&T, &T) -> Ordering) {
        sort_by_impl(v, v.len(), 1, cmp)
    }

    // By index, for nodes that aren't a slice's - a table of runtime-sized
    // entries:

    /// The node @f says is the one searched for (Equal), if any.
    pub fn find(&self, f: impl FnMut(usize) -> Ordering) -> Option<usize> {
        find_impl(self.0.size, 1, f).map(|i| i - 1)
    }

    /// @v's nodes sorted into eytzinger order: compared, and swapped, in @v
    /// by index.
    pub fn sort<S: ?Sized>(&self, v: &mut S, cmp: impl FnMut(&S, usize, usize) -> Ordering,
                           swap: impl FnMut(&mut S, usize, usize)) {
        sort_impl(v, self.0.size, 1, cmp, swap)
    }
}

#[cfg(test)]
mod tests {
    use super::{Eytzinger0, Eytzinger1};

    /// A permutation of 0, 2, 4.. (2n - 2), from an LCG: deterministic, no
    /// dependencies.
    fn shuffled(n: usize, seed: u64) -> Vec<u32> {
        let mut v: Vec<u32> = (0..n as u32).map(|x| x * 2).collect();
        let mut s = seed;
        for i in (1..n).rev() {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            v.swap(i, (s >> 33) as usize % (i + 1));
        }
        v
    }

    // The index math and the sort are only checked against each other and a
    // linear scan: a wrong answer here is a lookup that silently misses.
    #[test]
    fn eytzinger1_sort_find() {
        for n in 0..300 {
            let e = Eytzinger1::<usize>::new(n);
            let e32 = Eytzinger1::<u32>::new(n as u32);
            let e16 = Eytzinger1::<u16>::new(n as u16);
            for i in 1..=n {
                assert_eq!(e.from_inorder(e.to_inorder(i)), i);
                assert_eq!(e32.to_inorder(i as u32) as usize, e.to_inorder(i));
                assert_eq!(e16.from_inorder(i as u16) as usize, e.from_inorder(i));
            }
            let order: Vec<usize> = e.iter().collect();
            assert_eq!(order, (1..=n).map(|i| e.from_inorder(i)).collect::<Vec<_>>());
            assert_eq!(order.last().copied().unwrap_or(0), e.last());
            assert!(e32.iter().map(|i| i as usize).eq(order.iter().copied()));
            assert!(e16.iter().map(|i| i as usize).eq(order.iter().copied()));

            let mut v = vec![u32::MAX];
            v.extend(shuffled(n, n as u64));
            Eytzinger1::sort_by(&mut v, |a, b| a.cmp(b));
            assert_eq!(v[0], u32::MAX);
            assert!(order.windows(2).all(|w| v[w[0]] < v[w[1]]), "n {n}: not sorted");

            for x in 0..2 * n as u32 + 1 {
                let found = Eytzinger1::find_by(&v, |e| e.cmp(&x));
                assert_eq!(found.map(|i| v[i]), (x % 2 == 0 && x < 2 * n as u32).then_some(x));
            }
        }
    }

    #[test]
    fn eytzinger0_sort_find() {
        for n in 0..300 {
            let e = Eytzinger0::<usize>::new(n);
            let order: Vec<usize> = e.iter().collect();
            assert_eq!(order, (0..n).map(|i| e.from_inorder(i)).collect::<Vec<_>>());
            assert!(order.iter().enumerate().all(|(pos, &i)| e.to_inorder(i) == pos));
            let mut rev: Vec<usize> = e.iter_rev().collect();
            rev.reverse();
            assert_eq!(rev, order);

            let mut v = shuffled(n, n as u64 + 1000);
            Eytzinger0::sort_by(&mut v, |a, b| a.cmp(b));
            assert!(order.windows(2).all(|w| v[w[0]] < v[w[1]]), "n {n}: not sorted");

            let sorted: Vec<u32> = order.iter().map(|&i| v[i]).collect();
            for x in 0..2 * n as u32 + 1 {
                let val = |i: Option<usize>| i.map(|i| v[i]);
                assert_eq!(val(Eytzinger0::find_by(&v, |e| e.cmp(&x))),
                           sorted.iter().copied().find(|&e| e == x));
                assert_eq!(val(Eytzinger0::find_le_by(&v, |e| e.cmp(&x))),
                           sorted.iter().copied().rev().find(|&e| e <= x));
                assert_eq!(val(Eytzinger0::find_gt_by(&v, |e| e.cmp(&x))),
                           sorted.iter().copied().find(|&e| e > x));
                assert_eq!(val(Eytzinger0::find_ge_by(&v, |e| e.cmp(&x))),
                           sorted.iter().copied().find(|&e| e >= x));
            }
        }
    }
}
