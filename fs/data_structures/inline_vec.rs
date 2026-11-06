// SPDX-License-Identifier: GPL-2.0

//! A vector with room for @N elements in itself - on the stack, or in what
//! holds it - that moves them to the heap when it needs more: C's
//! DARRAY_PREALLOCATED(), for what's usually small and occasionally isn't.
//!
//! A union of the two, told apart by the low bit of the first word: on the
//! heap it's a DstVec, a pointer, its low bit clear; inline, that word's low
//! bytes are an @L holding the length shifted up past a set low bit, and the
//! elements follow. So an inline length is at most @L's half, and N may be no
//! more. Spilling is a DstVec's allocation, and fallible - an AllocError,
//! where darray_make_room() is -ENOMEM. Once on the heap it stays there:
//! shrinking back would move the elements again for nothing.
//!
//! Its elements are a slice, Deref'd: the slice's methods - iteration,
//! indexing, sort_unstable - are its. Growing it is reserve(), push() and
//! insert(), each fallible as the allocation under it is.
//!
//! RCU readers may read it - read_rcu() - while its writer, its lock's holder,
//! changes it. A spill or a grow writes only the first word, with a release,
//! and never the inline elements after it, which nothing writes again once
//! they're spilled; what's grown out of is freed as its elements say, a grace
//! period on if they aren't FreeAnytime.

use crate::data_structures::array_vec::LenUint;
use crate::data_structures::dst_array::assert_rcu_freed;
use crate::data_structures::dst_vec::{DstVec, DstVecHead};
use crate::util::alloc::AllocError;
use crate::util::rcu;
use core::mem::{offset_of, ManuallyDrop, MaybeUninit};
use core::ops::{Deref, DerefMut};
use core::ptr;
use core::slice;
use core::sync::atomic::{AtomicPtr, Ordering::{Acquire, Release}};

#[repr(transparent)]
pub struct InlineVec<T, const N: usize, L: LenUint = u32> {
    repr: Repr<T, N, L>,
}

#[repr(C)]
union Repr<T, const N: usize, L: LenUint> {
    heap: ManuallyDrop<DstVec<T, L>>,
    inline: ManuallyDrop<Inline<T, N, L>>,
}

#[repr(C)]
struct Inline<T, const N: usize, L: LenUint> {
    pad: L::WordPad,
    /* the length << 1 | 1 */
    tag: L,
    elems: [MaybeUninit<T>; N],
}

unsafe impl<T: Send, const N: usize, L: LenUint> Send for InlineVec<T, N, L> {}
unsafe impl<T: Sync, const N: usize, L: LenUint> Sync for InlineVec<T, N, L> {}

impl<T, const N: usize, L: LenUint> InlineVec<T, N, L> {
    pub const fn new() -> Self {
        const { assert!(N <= L::MAX >> 1, "InlineVec: its inline capacity doesn't fit its length type") };

        let inline = Inline { pad: L::WORD_PAD, tag: L::ONE, elems: [const { MaybeUninit::uninit() }; N] };
        Self { repr: Repr { inline: ManuallyDrop::new(inline) } }
    }

    /// new(), its inline room zeroes: so what an RCU reader may see before
    /// it's pushed - a length is published before the element it counts - is
    /// an element.
    pub const fn new_zeroed() -> Self
    where
        T: ::pin_init::Zeroable,
    {
        const { assert!(N <= L::MAX >> 1, "InlineVec: its inline capacity doesn't fit its length type") };

        let inline = Inline { pad: L::WORD_PAD, tag: L::ONE, elems: [const { MaybeUninit::zeroed() }; N] };
        Self { repr: Repr { inline: ManuallyDrop::new(inline) } }
    }

    /// Whether its elements are on the heap.
    pub fn spilled(&self) -> bool {
        // spilled, these are the pointer's low bytes
        unsafe { self.repr.inline.tag.to_usize() & 1 == 0 }
    }

    pub fn len(&self) -> usize {
        if self.spilled() {
            unsafe { self.repr.heap.len() }
        } else {
            unsafe { self.repr.inline.tag.to_usize() >> 1 }
        }
    }

    pub fn capacity(&self) -> usize {
        if self.spilled() { unsafe { self.repr.heap.capacity() } } else { N }
    }

    /// # Safety
    /// It isn't spilled; the first @len are initialized, and @len is at most N.
    unsafe fn set_inline_len(&mut self, len: usize) {
        unsafe { (*self.repr.inline).tag = L::from_usize(len << 1 | 1) };
    }

    /// Its elements moved to a DstVec with room for @cap.
    fn spill(&mut self, cap: usize) -> Result<(), AllocError> {
        if cap > L::MAX {
            return Err(AllocError);
        }
        let len = self.len();
        let mut heap = DstVec::with_capacity(cap.max(N.saturating_mul(2)).min(L::MAX))?;
        unsafe {
            ptr::copy_nonoverlapping(self.repr.inline.elems.as_ptr().cast(), heap.as_mut_ptr(), len);
            heap.set_len(len);
        }
        self.set_heap(heap);
        Ok(())
    }

    /// @heap where its elements are now: the first word alone written, by a
    /// release - an RCU reader may be in the elements it had, and has to see
    /// what was copied into @heap once it sees @heap.
    fn set_heap(&mut self, heap: DstVec<T, L>) {
        let word = unsafe { AtomicPtr::from_ptr((&raw mut self.repr.heap).cast::<*mut DstVecHead<L>>()) };
        word.store(heap.into_raw().as_ptr(), Release);
    }

    /// Its elements moved into @into, if it has more room - allocated before,
    /// as under a spinlock it must be - and what they were in freed, as they
    /// say; else @into back.
    pub fn grow_into(&mut self, mut into: DstVec<T, L>) -> Result<(), DstVec<T, L>> {
        let len = self.len();
        if into.capacity() <= self.capacity() || !into.is_empty() {
            return Err(into);
        }

        unsafe {
            ptr::copy_nonoverlapping(self.as_ptr(), into.as_mut_ptr(), len);
            into.set_len(len);
        }
        if self.spilled() {
            let old = unsafe { ManuallyDrop::take(&mut self.repr.heap) };
            self.set_heap(into);
            unsafe { old.free_moved() };
        } else {
            self.set_heap(into);
        }
        Ok(())
    }

    /// Its elements, for an RCU reader while its writer - its lock's holder -
    /// changes it: what it's grown out of is freed a grace period on, its
    /// elements not FreeAnytime, and what it's grown into published after
    /// they're copied there. Its length isn't: a reader may see one before
    /// the element it counts, which publishes itself, an atomic.
    ///
    /// # Safety
    /// @v lives for 'a, its writer's stores to its elements are atomic, and
    /// it's had only through &mut by its writer and read_rcu() by readers.
    pub unsafe fn read_rcu<'a>(v: *const Self, _rcu: &'a rcu::ReadGuard) -> &'a [T] {
        assert_rcu_freed::<MaybeUninit<T>>();
        const { assert!(offset_of!(Inline<T, N, L>, elems) >= size_of::<usize>(),
                        "InlineVec: RCU readers need its elements past the first word, which a spill rewrites") };

        unsafe {
            let inline = (&raw const (*v).repr.inline).cast::<Inline<T, N, L>>();
            let tag = ptr::read_volatile(&raw const (*inline).tag).to_usize();
            if tag & 1 != 0 {
                slice::from_raw_parts((&raw const (*inline).elems).cast::<T>(), tag >> 1)
            } else {
                let word = AtomicPtr::from_ptr((&raw const (*v).repr.heap).cast::<*mut DstVecHead<L>>().cast_mut());
                DstVec::<T, L>::read_rcu(word.load(Acquire))
            }
        }
    }

    /// Room for @additional more, moving to the heap if they don't fit
    /// inline: darray_make_room().
    pub fn reserve(&mut self, additional: usize) -> Result<(), AllocError> {
        if self.spilled() {
            return unsafe { (*self.repr.heap).reserve(additional) };
        }

        let want = self.len().checked_add(additional).ok_or(AllocError)?;
        if want > N { self.spill(want) } else { Ok(()) }
    }

    /// @x, added last.
    pub fn push(&mut self, x: T) -> Result<(), AllocError> {
        let len = self.len();
        if !self.spilled() && len < N {
            unsafe {
                (*self.repr.inline).elems[len].write(x);
                self.set_inline_len(len + 1);
            }
            return Ok(());
        }

        self.reserve(1)?;
        unsafe { (*self.repr.heap).push(x) }
    }

    /// Empty, keeping what it's allocated: a darray's nr = 0.
    pub fn clear(&mut self) {
        if self.spilled() {
            unsafe { (*self.repr.heap).clear() };
            return;
        }

        let elems: *mut [T] = &mut **self;
        unsafe {
            self.set_inline_len(0);
            ptr::drop_in_place(elems);
        }
    }

    /// The first @len kept, the rest dropped; nothing, if it's no longer.
    pub fn truncate(&mut self, len: usize) {
        if len >= self.len() {
            return;
        }

        let tail: *mut [T] = &mut self[len..];
        unsafe {
            self.set_len(len);
            ptr::drop_in_place(tail);
        }
    }

    /// # Safety
    /// @len is at most its capacity, and the first @len are initialized: as
    /// Vec::set_len().
    unsafe fn set_len(&mut self, len: usize) {
        if self.spilled() {
            unsafe { (*self.repr.heap).set_len(len) };
        } else {
            unsafe { self.set_inline_len(len) };
        }
    }

    /// @x, inserted at @i, the elements from @i on moving up. Panics past the
    /// end, as slices do.
    pub fn insert(&mut self, i: usize, x: T) -> Result<(), AllocError> {
        assert!(i <= self.len(), "inserting at {i} of {}", self.len());
        self.push(x)?;
        self[i..].rotate_right(1);
        Ok(())
    }
}

impl<T, const N: usize, L: LenUint> Drop for InlineVec<T, N, L> {
    fn drop(&mut self) {
        if self.spilled() {
            unsafe { ManuallyDrop::drop(&mut self.repr.heap) };
        } else {
            self.clear();
        }
    }
}

impl<T, const N: usize, L: LenUint> Default for InlineVec<T, N, L> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const N: usize, L: LenUint> Deref for InlineVec<T, N, L> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        if self.spilled() {
            unsafe { &self.repr.heap }
        } else {
            unsafe { slice::from_raw_parts(self.repr.inline.elems.as_ptr().cast(), self.len()) }
        }
    }
}

impl<T, const N: usize, L: LenUint> DerefMut for InlineVec<T, N, L> {
    fn deref_mut(&mut self) -> &mut [T] {
        let len = self.len();
        if self.spilled() {
            unsafe { &mut *self.repr.heap }
        } else {
            unsafe { slice::from_raw_parts_mut((*self.repr.inline).elems.as_mut_ptr().cast(), len) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spills_past_n_keeping_order() {
        let mut v = InlineVec::<u32, 2>::new();
        v.push(1).unwrap();
        v.insert(0, 0).unwrap();
        assert!(!v.spilled());

        // a third doesn't fit inline: the first two move with it
        v.insert(1, 5).unwrap();
        assert!(v.spilled());
        v.push(9).unwrap();
        assert_eq!(*v, [0, 5, 1, 9]);
    }

    #[test]
    fn reserve_spills_ahead() {
        let mut v = InlineVec::<u32, 4>::new();
        v.reserve(4).unwrap();
        assert!(!v.spilled(), "four fit");
        v.reserve(5).unwrap();
        assert!(v.spilled(), "five don't");
        assert!(v.is_empty());
    }

    #[test]
    fn a_u8_length() {
        let mut v = InlineVec::<u16, 3, u8>::new();
        for i in 0..200 {
            v.push(i).unwrap();
        }
        assert_eq!(v.len(), 200);
        assert_eq!(v[199], 199);
        assert!(v.reserve(100).is_err(), "past u8::MAX");
    }

    #[test]
    fn small_is_small() {
        assert_eq!(size_of::<InlineVec<u8, 7, u8>>(), size_of::<usize>());
    }

    #[test]
    fn grows_into_what_it_is_given() {
        let mut v = InlineVec::<u32, 2>::new_zeroed();
        v.push(1).unwrap();
        v.push(2).unwrap();

        assert!(v.grow_into(DstVec::with_capacity(2).unwrap()).is_err(), "no more room: handed back");
        assert!(v.grow_into(DstVec::with_capacity(4).unwrap()).is_ok());
        assert!(v.spilled());
        v.push(3).unwrap();
        assert!(v.grow_into(DstVec::with_capacity(8).unwrap()).is_ok());
        assert_eq!((&*v, v.capacity()), (&[1, 2, 3][..], 8));
    }
}
