// SPDX-License-Identifier: GPL-2.0

//! A fifo of absolute indices: front and back count up forever, wrapping at
//! the index type's width, and the entry at index i lives at data[i & mask].
//! So an index names its entry for as long as it's in the fifo, whatever's
//! pushed and popped around it - the journal's are its sequence numbers.
//!
//! The struct is util/fifo.h's, Fifo<T, I> in types/lib.rs: C's macros and
//! these methods work on the same one, until the journal's C is gone and
//! fifo.h with it. size is C's limit on how many entries it holds; here it's
//! always the whole buffer, mask + 1 - as C's journal fifos have it too - and
//! goes with fifo.h.
//!
//! Growing keeps every index where it is: the buffer's a power of two, and
//! the old one is copied into each old-sized stretch of the new one, so i &
//! mask lands on i's entry under the new mask too, whatever bits it gained.
//!
//! An empty Fifo has no buffer, as C's before init_fifo(); one C has freed
//! has none again (free_fifo() NULLs it), so dropping it frees nothing.

use crate::types::Fifo;
use crate::util::alloc::{kvfree_sized, kvrealloc, AllocError, Flags};
use core::mem::{needs_drop, size_of, MaybeUninit};
use core::ptr::NonNull;
use num_traits::{AsPrimitive, ConstZero, PrimInt, Unsigned, WrappingAdd, WrappingSub};

pub trait FifoIdx: PrimInt + Unsigned + ConstZero + WrappingAdd + WrappingSub + AsPrimitive<usize> {
    /// Truncated to the type's width, as C's index arithmetic is.
    fn from_usize(n: usize) -> Self;
}

impl<I> FifoIdx for I
where
    I: PrimInt + Unsigned + ConstZero + WrappingAdd + WrappingSub + AsPrimitive<usize>,
    usize: AsPrimitive<I>,
{
    fn from_usize(n: usize) -> I {
        n.as_()
    }
}

// SAFETY: it owns its entries, as a Vec does
unsafe impl<T: Send, I: FifoIdx + Send> Send for Fifo<T, I> {}
unsafe impl<T: Sync, I: FifoIdx + Sync> Sync for Fifo<T, I> {}

impl<T, I: FifoIdx> Default for Fifo<T, I> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, I: FifoIdx> Fifo<T, I> {
    pub const fn new() -> Self {
        Fifo { front: I::ZERO, back: I::ZERO, size: I::ZERO, mask: I::ZERO,
               data: core::ptr::null_mut() }
    }

    fn add(i: I, n: usize) -> I {
        i.wrapping_add(&I::from_usize(n))
    }

    fn prev(i: I) -> I {
        i.wrapping_sub(&I::one())
    }

    /// How far @i is past front, in the index type's width.
    fn offset(&self, i: I) -> usize {
        i.wrapping_sub(&self.front).as_()
    }

    fn slot(&self, i: I) -> *mut T {
        self.data.wrapping_add((i & self.mask).as_())
    }

    /// The buffer's entries, mask + 1, or none.
    fn buf_len(&self) -> usize {
        if self.data.is_null() { 0 } else { self.mask.as_() + 1 }
    }

    pub fn len(&self) -> usize {
        self.offset(self.back)
    }

    pub fn is_empty(&self) -> bool {
        self.front == self.back
    }

    pub fn capacity(&self) -> usize {
        self.size.as_()
    }

    pub fn free(&self) -> usize {
        self.capacity() - self.len()
    }

    pub fn is_full(&self) -> bool {
        self.len() == self.capacity()
    }

    pub fn front_idx(&self) -> I {
        self.front
    }

    /// One past the last entry's index: the next push_back()'s.
    pub fn back_idx(&self) -> I {
        self.back
    }

    pub fn get(&self, i: I) -> Option<&T> {
        // SAFETY: in [front, back): an entry
        (self.offset(i) < self.len()).then(|| unsafe { &*self.slot(i) })
    }

    pub fn get_mut(&mut self, i: I) -> Option<&mut T> {
        // SAFETY: as get()'s
        (self.offset(i) < self.len()).then(|| unsafe { &mut *self.slot(i) })
    }

    pub fn peek_front(&self) -> Option<&T> {
        self.get(self.front)
    }

    pub fn peek_back(&self) -> Option<&T> {
        self.get(Self::prev(self.back))
    }

    pub fn push_back(&mut self, v: T) -> Result<(), T> {
        if self.is_full() {
            return Err(v);
        }
        // SAFETY: a free slot
        unsafe { self.slot(self.back).write(v) };
        self.back = Self::add(self.back, 1);
        Ok(())
    }

    pub fn push_front(&mut self, v: T) -> Result<(), T> {
        if self.is_full() {
            return Err(v);
        }
        self.front = Self::prev(self.front);
        // SAFETY: a free slot
        unsafe { self.slot(self.front).write(v) };
        Ok(())
    }

    pub fn pop_front(&mut self) -> Option<T> {
        if self.is_empty() {
            return None;
        }
        // SAFETY: an entry, no longer one once front's past it
        let v = unsafe { self.slot(self.front).read() };
        self.front = Self::add(self.front, 1);
        Some(v)
    }

    pub fn pop_back(&mut self) -> Option<T> {
        if self.is_empty() {
            return None;
        }
        self.back = Self::prev(self.back);
        // SAFETY: an entry, no longer one now back's at it
        Some(unsafe { self.slot(self.back).read() })
    }

    /// Drop the first @n entries.
    pub fn advance_front(&mut self, n: usize) {
        assert!(n <= self.len(), "advancing a fifo's front past its back");
        if needs_drop::<T>() {
            for _ in 0..n {
                drop(self.pop_front());
            }
        } else {
            self.front = Self::add(self.front, n);
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> + '_ {
        let (a, b) = self.as_slices();
        a.iter().chain(b)
    }

    /// The entries, front to back, as the buffer has them: two stretches,
    /// the second empty unless they wrap.
    pub fn as_slices(&self) -> (&[T], &[T]) {
        if self.data.is_null() {
            return (&[], &[]);
        }
        let (start, first, rest) = self.stretches(self.front, self.len());
        // SAFETY: entries, front to back
        unsafe {
            (core::slice::from_raw_parts(self.data.wrapping_add(start), first),
             core::slice::from_raw_parts(self.data, rest))
        }
    }

    /// The free slots after back, as as_slices() has the entries.
    fn spare_mut(&mut self) -> (&mut [MaybeUninit<T>], &mut [MaybeUninit<T>]) {
        if self.data.is_null() {
            return (&mut [], &mut []);
        }
        let (start, first, rest) = self.stretches(self.back, self.free());
        let data = self.data.cast::<MaybeUninit<T>>();
        // SAFETY: free slots, disjoint, and ours while we're borrowed
        unsafe {
            (core::slice::from_raw_parts_mut(data.wrapping_add(start), first),
             core::slice::from_raw_parts_mut(data, rest))
        }
    }

    /// @n slots from index @i: where the first stretch starts, its length,
    /// and the length of the second, from the buffer's start.
    fn stretches(&self, i: I, n: usize) -> (usize, usize, usize) {
        let start = (i & self.mask).as_();
        let first = n.min(self.buf_len() - start);
        (start, first, n - first)
    }

    /// Room for @more entries past those there, the buffer grown if it must
    /// be - to a power of two, every index kept. @gfp as kvrealloc()'s.
    pub fn reserve(&mut self, more: usize, gfp: Flags) -> Result<(), AllocError> {
        const { assert!(size_of::<T>() != 0, "a fifo of zero sized entries") };

        let want = self.len().checked_add(more).ok_or(AllocError)?;
        if want <= self.capacity() {
            return Ok(());
        }

        let old = self.buf_len();
        let new = want.checked_next_power_of_two().ok_or(AllocError)?;
        // size, the largest index field, holds the whole buffer
        let size: I = num_traits::cast(new).ok_or(AllocError)?;

        let p = kvrealloc(self.data.cast(), old * size_of::<T>(), new * size_of::<T>(), gfp)?
            .cast::<T>().as_ptr();
        if old != 0 {
            for k in 1..new / old {
                // SAFETY: within the new buffer; the first stretch is the old
                unsafe { p.copy_to_nonoverlapping(p.add(k * old), old) };
            }
        }

        self.data = p;
        self.mask = size - I::one();
        self.size = size;
        Ok(())
    }
}

impl<T: Copy, I: FifoIdx> Fifo<T, I> {
    /// Up to @n entries off the front, a stretch at a time: @f takes each
    /// stretch it's given, or refuses it and the rest. How many it took.
    pub fn drain_with(&mut self, n: usize, mut f: impl FnMut(&[T]) -> bool) -> usize {
        let (a, b) = self.as_slices();
        let mut done = 0;
        for s in [a, b] {
            let k = s.len().min(n - done);
            if k == 0 || !f(&s[..k]) {
                break;
            }
            done += k;
        }
        self.advance_front(done);
        done
    }

    /// Up to @n entries onto the back, as many as fit, a stretch of free
    /// slots at a time: @f fills each stretch it's given, or refuses it and
    /// the rest. How many it filled.
    ///
    /// # Safety
    /// @f returns true only having written every slot of its stretch.
    pub unsafe fn fill_with(&mut self, n: usize, mut f: impl FnMut(&mut [MaybeUninit<T>]) -> bool)
        -> usize
    {
        let n = n.min(self.free());
        let (a, b) = self.spare_mut();
        let mut done = 0;
        for s in [a, b] {
            let k = s.len().min(n - done);
            if k == 0 || !f(&mut s[..k]) {
                break;
            }
            done += k;
        }
        self.back = Self::add(self.back, done);
        done
    }

    /// @s at the back, all of it - Err if it doesn't fit.
    pub fn extend_from_slice(&mut self, s: &[T]) -> Result<(), ()> {
        if s.len() > self.free() {
            return Err(());
        }
        let mut src = s;
        // SAFETY: each stretch written in full
        unsafe {
            self.fill_with(s.len(), |d| {
                let (h, t) = src.split_at(d.len());
                d.as_mut_ptr().cast::<T>().copy_from_nonoverlapping(h.as_ptr(), h.len());
                src = t;
                true
            })
        };
        Ok(())
    }
}

impl<T, I: FifoIdx> Drop for Fifo<T, I> {
    fn drop(&mut self) {
        let Some(data) = NonNull::new(self.data) else { return };
        self.advance_front(self.len());
        // SAFETY: reserve()'s - or init_fifo()'s, kvmalloc() too - and no one's
        unsafe { kvfree_sized(data.cast(), self.buf_len() * size_of::<T>()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::alloc::flags::GFP_KERNEL;

    #[test]
    fn indices_survive_wrap_and_grow() {
        let mut f = Fifo::<u32, u16>::new();
        f.reserve(4, GFP_KERNEL).unwrap();

        // walk the indices past the u16 wrap, the buffer wrapping too
        for i in 0..70000u32 {
            f.push_back(i).unwrap();
            assert_eq!(f.pop_front(), Some(i));
        }
        for i in 0..3 {
            f.push_back(i).unwrap();
        }
        let front = f.front_idx();

        f.reserve(13, GFP_KERNEL).unwrap();
        assert_eq!(f.capacity(), 16);
        for i in 0..3u32 {
            assert_eq!(f.get(Fifo::<u32, u16>::add(front, i as usize)), Some(&i));
        }
        assert_eq!(f.iter().copied().collect::<Vec<_>>(), [0, 1, 2]);
    }

    #[test]
    fn slices_wrap() {
        let mut f = Fifo::<u8>::new();
        f.reserve(8, GFP_KERNEL).unwrap();
        f.extend_from_slice(b"abcdef").unwrap();
        assert_eq!(f.drain_with(5, |s| s == b"abcde"), 5);
        f.extend_from_slice(b"ghijk").unwrap();
        assert!(f.extend_from_slice(b"lmn").is_err());

        let (a, b) = f.as_slices();
        assert_eq!((a, b), (&b"fgh"[..], &b"ijk"[..]));
    }
}
