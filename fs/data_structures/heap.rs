// SPDX-License-Identifier: GPL-2.0

//! A min-heap over a KVVec, ordered by a comparator, as the kernel's min_heap.

use core::cmp::Ordering;
use core::ops::{Deref, DerefMut};

use crate::util::alloc::KVVec;

/// @v[node] moved down until no child orders after it: a max-heap's sift, by
/// @cmp.
pub(crate) fn sift_down<T>(v: &mut [T], mut node: usize, cmp: &mut impl FnMut(&T, &T) -> Ordering) {
    loop {
        let mut child = 2 * node + 1;
        if child >= v.len() {
            break;
        }
        if child + 1 < v.len() && cmp(&v[child], &v[child + 1]) == Ordering::Less {
            child += 1;
        }
        if cmp(&v[node], &v[child]) != Ordering::Less {
            break;
        }
        v.swap(node, child);
        node = child;
    }
}

pub struct MinHeap<T, F: FnMut(&T, &T) -> Ordering> {
    v:   KVVec<T>,
    cmp: F,
}

impl<T, F: FnMut(&T, &T) -> Ordering> MinHeap<T, F> {
    /// @v as a heap, least first by @cmp: min_heapify_all().
    pub fn new(v: KVVec<T>, cmp: F) -> Self {
        let mut heap = MinHeap { v, cmp };
        for i in (0..heap.v.len() / 2).rev() {
            heap.sift_down(i);
        }
        heap
    }

    pub fn len(&self) -> usize {
        self.v.len()
    }

    pub fn is_empty(&self) -> bool {
        self.v.is_empty()
    }

    pub fn peek(&self) -> Option<&T> {
        self.v.first()
    }

    /// The least, to change in place: it's sifted back down when the PeekMut
    /// drops.
    pub fn peek_mut(&mut self) -> Option<PeekMut<'_, T, F>> {
        (!self.v.is_empty()).then(|| PeekMut { heap: Some(self) })
    }

    pub fn pop(&mut self) -> Option<T> {
        let last = self.v.len().checked_sub(1)?;
        self.v.swap(0, last);
        let min = self.v.pop();
        self.sift_down(0);
        min
    }

    fn sift_down(&mut self, node: usize) {
        let MinHeap { v, cmp } = self;
        sift_down(v, node, &mut |l, r| cmp(r, l));
    }
}

pub struct PeekMut<'a, T, F: FnMut(&T, &T) -> Ordering> {
    /* None once popped: nothing left to sift */
    heap: Option<&'a mut MinHeap<T, F>>,
}

impl<T, F: FnMut(&T, &T) -> Ordering> PeekMut<'_, T, F> {
    pub fn pop(mut this: Self) -> T {
        let heap = this.heap.take().expect("a PeekMut's heap");
        heap.pop().expect("a PeekMut's heap is not empty")
    }

    fn heap(&self) -> &MinHeap<T, F> {
        self.heap.as_deref().expect("a PeekMut's heap")
    }
}

impl<T, F: FnMut(&T, &T) -> Ordering> Deref for PeekMut<'_, T, F> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.heap().v[0]
    }
}

impl<T, F: FnMut(&T, &T) -> Ordering> DerefMut for PeekMut<'_, T, F> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.heap.as_deref_mut().expect("a PeekMut's heap").v[0]
    }
}

impl<T, F: FnMut(&T, &T) -> Ordering> Drop for PeekMut<'_, T, F> {
    fn drop(&mut self) {
        if let Some(heap) = self.heap.take() {
            heap.sift_down(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MinHeap, PeekMut};
    use crate::util::alloc::KVVec;

    // A wrong heap is silent: node scan would resolve overlapping nodes out
    // of order and keep the wrong one.
    #[test]
    fn min_heap_pops_in_order() {
        let mut s: u64 = 1;
        for n in 0..200 {
            let mut v = KVVec::new();
            let mut want = Vec::new();
            for _ in 0..n {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let x = (s >> 40) as u32 % 50;
                v.push(x).unwrap();
                want.push(x);
            }
            let mut heap = MinHeap::new(v, |l: &u32, r: &u32| l.cmp(r));

            /* raising the least re-sifts it, as node scan trims a node's start */
            if let Some(mut min) = heap.peek_mut() {
                let i = want.iter().position(|&x| x == *min).unwrap();
                *min += 25;
                want[i] = *min;
            }
            want.sort_unstable();

            let mut got = Vec::new();
            while let Some(min) = heap.peek_mut() {
                got.push(PeekMut::pop(min));
            }
            assert_eq!(got, want, "n = {n}");
        }
    }
}
