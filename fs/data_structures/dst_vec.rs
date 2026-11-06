// SPDX-License-Identifier: GPL-2.0

//! DstVec<T, L>: a vector on a DstArray, its length and capacity the header,
//! so it's a single pointer, its low bit clear: InlineVec tags it.

use crate::data_structures::array_vec::LenUint;
use crate::data_structures::dst_array::{DstArray, DstHead};
use crate::util::alloc::AllocError;
use core::marker::PhantomData;
use core::mem::{ManuallyDrop, MaybeUninit};
use core::ops::{Deref, DerefMut};
use core::ptr::{self, NonNull};
use core::slice;
use core::sync::atomic::{AtomicPtr, Ordering::Release};

#[repr(C)]
pub struct DstVecHead<L> {
    len: L,
    cap: L,
}

unsafe impl<L: LenUint> DstHead for DstVecHead<L> {
    fn nr(&self) -> usize {
        self.cap.to_usize()
    }
}

type Buf<T, L> = DstArray<DstVecHead<L>, MaybeUninit<T>>;

#[repr(transparent)]
pub struct DstVec<T, L: LenUint = u32> {
    head: NonNull<DstVecHead<L>>,
    _t: PhantomData<T>,
}

unsafe impl<T: Send, L: LenUint> Send for DstVec<T, L> {}
unsafe impl<T: Sync, L: LenUint> Sync for DstVec<T, L> {}

impl<T, L: LenUint> DstVec<T, L> {
    pub fn with_capacity(cap: usize) -> Result<Self, AllocError> {
        if cap > L::MAX {
            return Err(AllocError);
        }
        let head = Buf::<T, L>::alloc(DstVecHead { len: L::ZERO, cap: L::from_usize(cap) })?;
        Ok(Self { head, _t: PhantomData })
    }

    fn buf(&self) -> &Buf<T, L> {
        unsafe { &*Buf::<T, L>::from_thin(self.head.as_ptr()) }
    }

    fn buf_mut(&mut self) -> &mut Buf<T, L> {
        unsafe { &mut *Buf::<T, L>::from_thin(self.head.as_ptr()) }
    }

    pub fn len(&self) -> usize {
        self.buf().head.len.to_usize()
    }

    pub fn capacity(&self) -> usize {
        self.buf().head.cap.to_usize()
    }

    pub fn as_mut_ptr(&mut self) -> *mut T {
        self.buf_mut().elems.as_mut_ptr().cast()
    }

    /// # Safety
    /// The first @len are initialized, and @len is within its capacity.
    pub unsafe fn set_len(&mut self, len: usize) {
        self.buf_mut().head.len = L::from_usize(len);
    }

    /// Room for @additional more: if there isn't, a new allocation at least
    /// twice the size.
    pub fn reserve(&mut self, additional: usize) -> Result<(), AllocError> {
        let len = self.len();
        let want = len.checked_add(additional).ok_or(AllocError)?;
        if want <= self.capacity() {
            return Ok(());
        }
        if want > L::MAX {
            return Err(AllocError);
        }

        let cap = want.max(self.capacity().saturating_mul(2)).min(L::MAX);
        let head = DstVecHead { len: L::from_usize(len), cap: L::from_usize(cap) };
        let new = unsafe { Buf::<T, L>::realloc(self.head, len, head)? };
        let old = self.head;
        // by a release: an RCU reader that sees @new sees what was copied
        unsafe { AtomicPtr::from_ptr((&raw mut self.head).cast::<*mut DstVecHead<L>>()) }.store(new.as_ptr(), Release);
        unsafe { Buf::<T, L>::free(old) };
        Ok(())
    }

    /// Its buffer, owned by the caller.
    pub(crate) fn into_raw(self) -> NonNull<DstVecHead<L>> {
        ManuallyDrop::new(self).head
    }

    /// Its buffer freed, not its elements: they've been moved out.
    ///
    /// # Safety
    /// Its first len() have been moved out, and nothing but RCU readers uses
    /// it after.
    pub(crate) unsafe fn free_moved(self) {
        unsafe { Buf::<T, L>::free(self.into_raw()) };
    }

    /// The elements of the buffer at @head, as an RCU reader sees them,
    /// their length read whole: as InlineVec::read_rcu().
    ///
    /// # Safety
    /// As InlineVec::read_rcu()'s.
    pub(crate) unsafe fn read_rcu<'a>(head: *mut DstVecHead<L>) -> &'a [T] {
        unsafe {
            let buf = Buf::<T, L>::from_thin(head);
            let len = ptr::read_volatile(&raw const (*buf).head.len).to_usize();
            slice::from_raw_parts((&raw const (*buf).elems).cast::<T>(), len)
        }
    }

    pub fn push(&mut self, x: T) -> Result<(), AllocError> {
        self.reserve(1)?;
        let len = self.len();
        unsafe {
            self.as_mut_ptr().add(len).write(x);
            self.set_len(len + 1);
        }
        Ok(())
    }

    pub fn clear(&mut self) {
        let elems: *mut [T] = &mut **self;
        unsafe {
            self.set_len(0);
            ptr::drop_in_place(elems);
        }
    }
}

impl<T, L: LenUint> Drop for DstVec<T, L> {
    fn drop(&mut self) {
        self.clear();
        unsafe { Buf::<T, L>::free(self.head) };
    }
}

impl<T, L: LenUint> Deref for DstVec<T, L> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        let buf = self.buf();
        unsafe { slice::from_raw_parts(buf.elems.as_ptr().cast(), buf.head.len.to_usize()) }
    }
}

impl<T, L: LenUint> DerefMut for DstVec<T, L> {
    fn deref_mut(&mut self) -> &mut [T] {
        let len = self.len();
        unsafe { slice::from_raw_parts_mut(self.as_mut_ptr(), len) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grows_keeping_order() {
        let mut v = DstVec::<u64, u8>::with_capacity(1).unwrap();
        for i in 0..100 {
            v.push(i).unwrap();
        }
        assert_eq!(v.len(), 100);
        assert!(v.iter().copied().eq(0..100));
        assert!(v.reserve(200).is_err(), "past u8::MAX");
    }

    #[test]
    fn is_a_pointer() {
        assert_eq!(size_of::<DstVec<u64, u8>>(), size_of::<usize>());
    }
}
