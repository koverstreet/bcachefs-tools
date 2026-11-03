// SPDX-License-Identifier: GPL-2.0

//! A vector with room for @N elements in itself - on the stack, or in what
//! holds it - that moves them to the heap when it needs more: C's
//! DARRAY_PREALLOCATED(), for what's usually small and occasionally isn't.
//!
//! The inline half is arrayvec's; the heap half a KVVec, so spilling is the
//! kernel's allocation, with its flags, and fallible - an AllocError, where
//! darray_make_room() is -ENOMEM - never the global allocator's, which the
//! kernel build doesn't have. Once on the heap it stays there: shrinking back
//! would move the elements again for nothing.
//!
//! Its elements are a slice, Deref'd: the slice's methods - iteration,
//! indexing, sort_unstable - are its. Growing it is reserve(), push() and
//! insert(), each fallible as the allocation under it is.

use crate::util::alloc::{AllocError, Flags, KVVec};
use arrayvec::ArrayVec;
use core::ops::{Deref, DerefMut};

pub enum InlineVec<T, const N: usize> {
    Inline(ArrayVec<T, N>),
    Heap(KVVec<T>),
}

impl<T, const N: usize> InlineVec<T, N> {
    pub const fn new() -> Self {
        Self::Inline(ArrayVec::new_const())
    }

    /// Room for @additional more, moving to the heap if they don't fit
    /// inline: darray_make_room().
    pub fn reserve(&mut self, additional: usize, flags: Flags) -> Result<(), AllocError> {
        match self {
            Self::Inline(v) if v.len() + additional <= N => Ok(()),
            Self::Inline(v) => {
                let mut heap = KVVec::with_capacity(v.len() + additional, flags)?;
                for x in v.drain(..) {
                    heap.push(x, flags).expect("pushing within reserved capacity doesn't allocate");
                }
                *self = Self::Heap(heap);
                Ok(())
            }
            Self::Heap(v) => v.reserve(additional, flags),
        }
    }

    /// @x, added last.
    pub fn push(&mut self, x: T, flags: Flags) -> Result<(), AllocError> {
        self.reserve(1, flags)?;
        match self {
            Self::Inline(v) => v.push(x),
            Self::Heap(v) => v.push(x, flags).expect("pushing within reserved capacity doesn't allocate"),
        }
        Ok(())
    }

    /// @x, inserted at @i, the elements from @i on moving up. Panics past the
    /// end, as slices do.
    pub fn insert(&mut self, i: usize, x: T, flags: Flags) -> Result<(), AllocError> {
        assert!(i <= self.len(), "inserting at {i} of {}", self.len());
        self.push(x, flags)?;
        self[i..].rotate_right(1);
        Ok(())
    }
}

impl<T, const N: usize> Default for InlineVec<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const N: usize> Deref for InlineVec<T, N> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        match self {
            Self::Inline(v) => v,
            Self::Heap(v) => v,
        }
    }
}

impl<T, const N: usize> DerefMut for InlineVec<T, N> {
    fn deref_mut(&mut self) -> &mut [T] {
        match self {
            Self::Inline(v) => v,
            Self::Heap(v) => v,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::alloc::flags::GFP_KERNEL;

    #[test]
    fn spills_past_n_keeping_order() {
        let mut v = InlineVec::<u32, 2>::new();
        v.push(1, GFP_KERNEL).unwrap();
        v.insert(0, 0, GFP_KERNEL).unwrap();
        assert!(matches!(v, InlineVec::Inline(_)));

        // a third doesn't fit inline: the first two move with it
        v.insert(1, 5, GFP_KERNEL).unwrap();
        assert!(matches!(v, InlineVec::Heap(_)));
        v.push(9, GFP_KERNEL).unwrap();
        assert_eq!(*v, [0, 5, 1, 9]);
    }

    #[test]
    fn reserve_spills_ahead() {
        let mut v = InlineVec::<u32, 4>::new();
        v.reserve(4, GFP_KERNEL).unwrap();
        assert!(matches!(v, InlineVec::Inline(_)), "four fit");
        v.reserve(5, GFP_KERNEL).unwrap();
        assert!(matches!(v, InlineVec::Heap(_)), "five don't");
        assert!(v.is_empty());
    }
}
