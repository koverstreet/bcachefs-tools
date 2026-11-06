// SPDX-License-Identifier: GPL-2.0

//! Vec<T, A, L>: a growable array allocated by A, its length and capacity
//! L, laid out as C's darray - nr, size, data - with data NULL until it
//! allocates. KVec and KVVec are it: the same in both builds.

use crate::data_structures::array_vec::LenUint;
use crate::util::alloc::{flags, Alloc, AllocError, Flags};
use core::alloc::Layout;
use core::marker::PhantomData;
use core::mem::{size_of, ManuallyDrop, MaybeUninit};
use core::ops::{Deref, DerefMut};
use core::ptr::{self, NonNull};
use core::slice;

#[repr(C)]
pub struct Vec<T, A: Alloc, L: LenUint = usize> {
    nr:   L,
    size: L,
    data: Option<NonNull<T>>,
    _p:   PhantomData<(T, A)>,
}

unsafe impl<T: Send, A: Alloc, L: LenUint> Send for Vec<T, A, L> {}
unsafe impl<T: Sync, A: Alloc, L: LenUint> Sync for Vec<T, A, L> {}

// Its elements are on the heap, and don't move with it: as a Box's
impl<T, A: Alloc, L: LenUint> Unpin for Vec<T, A, L> {}

// zeroed is empty: nr and size 0, data NULL - as a kzalloc()'d darray
unsafe impl<T, A: Alloc, L: LenUint> ::pin_init::Zeroable for Vec<T, A, L> {}

/// push_within_capacity()'s, with what wasn't pushed.
pub struct PushError<T>(pub T);

impl<T, A: Alloc, L: LenUint> Vec<T, A, L> {
    const ZST: bool = size_of::<T>() == 0;

    pub const fn new() -> Self {
        Vec { nr: L::ZERO, size: L::ZERO, data: None, _p: PhantomData }
    }

    pub fn with_capacity(capacity: usize) -> Result<Self, AllocError> {
        let mut v = Self::new();
        v.reserve(capacity)?;
        Ok(v)
    }

    pub fn len(&self) -> usize {
        self.nr.to_usize()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn capacity(&self) -> usize {
        if Self::ZST { L::MAX } else { self.size.to_usize() }
    }

    pub fn as_ptr(&self) -> *const T {
        self.data.unwrap_or(NonNull::dangling()).as_ptr()
    }

    pub fn as_mut_ptr(&mut self) -> *mut T {
        self.data.unwrap_or(NonNull::dangling()).as_ptr()
    }

    pub fn spare_capacity_mut(&mut self) -> &mut [MaybeUninit<T>] {
        let (len, cap) = (self.len(), self.capacity());
        unsafe { slice::from_raw_parts_mut(self.as_mut_ptr().add(len).cast(), cap - len) }
    }

    fn layout(cap: usize) -> Result<Layout, AllocError> {
        Layout::array::<T>(cap).map_err(|_| AllocError)
    }

    /// Room for @additional more: if there isn't, reallocated at least twice
    /// the size.
    pub fn reserve(&mut self, additional: usize) -> Result<(), AllocError> {
        self.reserve_gfp(additional, flags::GFP_KERNEL)
    }

    fn reserve_gfp(&mut self, additional: usize, gfp: Flags) -> Result<(), AllocError> {
        let (len, cap) = (self.len(), self.capacity());
        let want = len.checked_add(additional).ok_or(AllocError)?;
        if want <= cap {
            return Ok(());
        }
        if want > L::MAX {
            return Err(AllocError);
        }

        let new_cap = want.max(cap.saturating_mul(2)).min(L::MAX);
        let p = unsafe { A::realloc(self.data.map(NonNull::cast), Self::layout(new_cap)?, Self::layout(cap)?, gfp)? };
        self.data = Some(p.cast());
        self.size = L::from_usize(new_cap);
        Ok(())
    }

    /// # Safety
    /// There's room.
    unsafe fn push_unchecked(&mut self, v: T) {
        let len = self.len();
        unsafe {
            self.as_mut_ptr().add(len).write(v);
            self.inc_len(1);
        }
    }

    pub fn push(&mut self, v: T) -> Result<(), AllocError> {
        self.push_gfp(v, flags::GFP_KERNEL)
    }

    /// push(), with flags of its own: what may run in interrupt context.
    pub fn push_gfp(&mut self, v: T, gfp: Flags) -> Result<(), AllocError> {
        self.reserve_gfp(1, gfp)?;
        unsafe { self.push_unchecked(v) };
        Ok(())
    }

    pub fn push_within_capacity(&mut self, v: T) -> Result<(), PushError<T>> {
        if self.len() == self.capacity() {
            return Err(PushError(v));
        }
        unsafe { self.push_unchecked(v) };
        Ok(())
    }

    /// @n more, zeroed, if there's room for them: None, never a reallocation,
    /// if there isn't.
    pub fn extend_zeroed_within_capacity(&mut self, n: usize) -> Option<&mut [T]>
    where
        T: ::pin_init::Zeroable,
    {
        let len = self.len();
        if n > self.capacity() - len {
            return None;
        }
        unsafe {
            let p = self.as_mut_ptr().add(len);
            p.write_bytes(0, n);
            self.inc_len(n);
            Some(slice::from_raw_parts_mut(p, n))
        }
    }

    pub fn pop(&mut self) -> Option<T> {
        let len = self.len().checked_sub(1)?;
        self.nr = L::from_usize(len);
        Some(unsafe { self.as_ptr().add(len).read() })
    }

    pub fn extend_from_slice(&mut self, other: &[T]) -> Result<(), AllocError>
    where
        T: Clone,
    {
        self.reserve(other.len())?;
        for v in other {
            unsafe { self.push_unchecked(v.clone()) };
        }
        Ok(())
    }

    pub fn from_elem(value: T, n: usize) -> Result<Self, AllocError>
    where
        T: Clone,
    {
        let mut v = Self::with_capacity(n)?;
        if n != 0 {
            for _ in 1..n {
                unsafe { v.push_unchecked(value.clone()) };
            }
            unsafe { v.push_unchecked(value) };
        }
        Ok(v)
    }

    /// Its first @len kept, the rest dropped.
    pub fn truncate(&mut self, len: usize) {
        let old = self.len();
        if len >= old {
            return;
        }
        self.nr = L::from_usize(len);
        unsafe { ptr::drop_in_place(ptr::slice_from_raw_parts_mut(self.as_mut_ptr().add(len), old - len)) };
    }

    pub fn clear(&mut self) {
        self.truncate(0)
    }

    /// Only what @f keeps, in order.
    pub fn retain(&mut self, mut f: impl FnMut(&mut T) -> bool) {
        let len = self.len();
        let p = self.as_mut_ptr();
        /* if @f panics, what's past what it's seen leaks, rather than drops twice */
        self.nr = L::ZERO;

        let mut kept = 0;
        for i in 0..len {
            unsafe {
                let e = p.add(i);
                if f(&mut *e) {
                    if kept != i {
                        ptr::copy_nonoverlapping(e, p.add(kept), 1);
                    }
                    kept += 1;
                    self.nr = L::from_usize(kept);
                } else {
                    ptr::drop_in_place(e);
                }
            }
        }
    }

    /// Every element, by value, the vector left empty - its allocation kept.
    pub fn drain_all(&mut self) -> DrainAll<'_, T> {
        let len = self.len();
        self.nr = L::ZERO;
        DrainAll { elems: unsafe { slice::from_raw_parts_mut(self.as_mut_ptr(), len) }.iter_mut() }
    }

    /// # Safety
    /// The @additional past its length are initialized, and within its
    /// capacity.
    pub unsafe fn inc_len(&mut self, additional: usize) {
        debug_assert!(self.len() + additional <= self.capacity());
        self.nr = L::from_usize(self.len() + additional);
    }

    /// # Safety
    /// @ptr is A's, of @capacity Ts - dangling, or NULL, for none - its first
    /// @length initialized.
    pub unsafe fn from_raw_parts(ptr: *mut T, length: usize, capacity: usize) -> Self {
        let data = if capacity != 0 && !Self::ZST { NonNull::new(ptr) } else { None };
        Vec { nr: L::from_usize(length), size: L::from_usize(capacity), data, _p: PhantomData }
    }

    /// Its allocation - dangling for none - length and capacity, the caller's.
    pub fn into_raw_parts(self) -> (*mut T, usize, usize) {
        let mut v = ManuallyDrop::new(self);
        (v.as_mut_ptr(), v.len(), v.capacity())
    }
}

impl<T, A: Alloc, L: LenUint> Drop for Vec<T, A, L> {
    fn drop(&mut self) {
        self.clear();
        if let Some(p) = self.data {
            let layout = Self::layout(self.capacity()).expect("its own layout");
            unsafe { A::free(p.cast(), layout) };
        }
    }
}

impl<T, A: Alloc, L: LenUint> Default for Vec<T, A, L> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, A: Alloc, L: LenUint> Deref for Vec<T, A, L> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        unsafe { slice::from_raw_parts(self.as_ptr(), self.len()) }
    }
}

impl<T, A: Alloc, L: LenUint> DerefMut for Vec<T, A, L> {
    fn deref_mut(&mut self) -> &mut [T] {
        unsafe { slice::from_raw_parts_mut(self.as_mut_ptr(), self.len()) }
    }
}

/// Vec::drain_all()'s: what isn't taken is dropped with it.
pub struct DrainAll<'v, T> {
    elems: slice::IterMut<'v, T>,
}

impl<T> Iterator for DrainAll<'_, T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        self.elems.next().map(|e| unsafe { ptr::read(e) })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.elems.size_hint()
    }
}

impl<T> Drop for DrainAll<'_, T> {
    fn drop(&mut self) {
        for e in &mut self.elems {
            unsafe { ptr::drop_in_place(e) };
        }
    }
}

/// A Vec by value: its elements, then its allocation freed.
pub struct IntoIter<T, A: Alloc, L: LenUint = usize> {
    v:     ManuallyDrop<Vec<T, A, L>>,
    start: usize,
    end:   usize,
}

impl<T, A: Alloc, L: LenUint> Iterator for IntoIter<T, A, L> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        if self.start == self.end {
            return None;
        }
        self.start += 1;
        Some(unsafe { self.v.as_ptr().add(self.start - 1).read() })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.end - self.start, Some(self.end - self.start))
    }
}

impl<T, A: Alloc, L: LenUint> DoubleEndedIterator for IntoIter<T, A, L> {
    fn next_back(&mut self) -> Option<T> {
        if self.start == self.end {
            return None;
        }
        self.end -= 1;
        Some(unsafe { self.v.as_ptr().add(self.end).read() })
    }
}

impl<T, A: Alloc, L: LenUint> ExactSizeIterator for IntoIter<T, A, L> {}

impl<T, A: Alloc, L: LenUint> Drop for IntoIter<T, A, L> {
    fn drop(&mut self) {
        unsafe {
            let rest = self.v.as_mut_ptr().add(self.start);
            ptr::drop_in_place(ptr::slice_from_raw_parts_mut(rest, self.end - self.start));
            /* the elements are gone: the vector's drop frees the allocation */
            self.v.nr = L::ZERO;
            ManuallyDrop::drop(&mut self.v);
        }
    }
}

impl<T, A: Alloc, L: LenUint> IntoIterator for Vec<T, A, L> {
    type Item = T;
    type IntoIter = IntoIter<T, A, L>;

    fn into_iter(self) -> IntoIter<T, A, L> {
        let end = self.len();
        IntoIter { v: ManuallyDrop::new(self), start: 0, end }
    }
}
