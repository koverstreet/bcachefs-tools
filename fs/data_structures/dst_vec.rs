// SPDX-License-Identifier: GPL-2.0

//! DstVec<T, L, A>: a vector on a DstArray, its length, capacity and
//! allocator the header, so it's a single pointer, its low bit clear:
//! InlineVec tags it. Its allocator is in its allocation - where it frees to
//! travels with what it frees, as a bio's bi_pool does.

use crate::data_structures::array_vec::LenUint;
use crate::data_structures::dst_array::{DstArray, DstHead};
use crate::util::alloc::{Alloc, AllocError, KVmalloc};
use crate::util::vstructs::SelfSized;
use core::marker::PhantomData;
use core::mem::{ManuallyDrop, MaybeUninit};
use core::ops::{Deref, DerefMut};
use core::ptr::{self, NonNull};
use core::slice;
use core::sync::atomic::{AtomicPtr, Ordering::Release};

#[repr(C)]
pub struct DstVecHead<L, A> {
    len:   L,
    cap:   L,
    alloc: A,
}

// SAFETY: cap, a plain field, written only as it's made
unsafe impl<L: LenUint, A> DstHead for DstVecHead<L, A> {
    fn nr(&self) -> usize {
        self.cap.to_usize()
    }
}

type Buf<T, L, A> = DstArray<DstVecHead<L, A>, MaybeUninit<T>>;

#[repr(transparent)]
pub struct DstVec<T, L: LenUint = u32, A: Alloc + Copy = KVmalloc> {
    head: NonNull<DstVecHead<L, A>>,
    _t: PhantomData<T>,
}

unsafe impl<T: Send, L: LenUint, A: Alloc + Copy + Send> Send for DstVec<T, L, A> {}
unsafe impl<T: Sync, L: LenUint, A: Alloc + Copy + Sync> Sync for DstVec<T, L, A> {}

impl<T, L: LenUint> DstVec<T, L> {
    pub fn with_capacity(cap: usize) -> Result<Self, AllocError> {
        Self::with_capacity_in(cap, KVmalloc)
    }

    /// with_capacity(), for what mustn't fail to allocate: past its length
    /// type is a bug.
    pub fn with_capacity_nofail(cap: usize) -> Self {
        assert!(cap <= L::MAX, "a DstVec of {cap}, past its length type");
        let head = Buf::<T, L, KVmalloc>::alloc_nofail(DstVecHead { len: L::ZERO, cap: L::from_usize(cap), alloc: KVmalloc });
        Self { head, _t: PhantomData }
    }
}

impl<T, L: LenUint, A: Alloc + Copy> DstVec<T, L, A> {
    /// with_capacity(), from @alloc: its room what @alloc gave, which it's
    /// freed by.
    pub fn with_capacity_in(cap: usize, alloc: A) -> Result<Self, AllocError> {
        if cap > L::MAX {
            return Err(AllocError);
        }
        let head = |room: usize| DstVecHead { len: L::ZERO, cap: L::from_usize(room.min(L::MAX)), alloc };
        Ok(Self { head: Buf::<T, L, A>::alloc_in(cap, head, &alloc)?, _t: PhantomData })
    }

    /// What one with room for @cap is allocated as: a mempool's element, for
    /// a pool of them.
    pub fn alloc_size(cap: usize) -> usize {
        Buf::<T, L, A>::layout(cap).expect("a DstVec's layout").size()
    }

    fn buf(&self) -> &Buf<T, L, A> {
        unsafe { &*Buf::<T, L, A>::from_thin(self.head.as_ptr().cast()) }
    }

    fn buf_mut(&mut self) -> &mut Buf<T, L, A> {
        unsafe { &mut *Buf::<T, L, A>::from_thin(self.head.as_ptr().cast()).cast_mut() }
    }

    pub fn len(&self) -> usize {
        self.buf().head().len.to_usize()
    }

    pub fn capacity(&self) -> usize {
        self.buf().head().cap.to_usize()
    }

    pub fn as_mut_ptr(&mut self) -> *mut T {
        self.buf_mut().elems.as_mut_ptr().cast()
    }

    /// # Safety
    /// The first @len are initialized, and @len is within its capacity.
    pub unsafe fn set_len(&mut self, len: usize) {
        // through the buffer's own pointer to its header, not the buffer: it
        // has its header only by &
        unsafe { (*self.head.as_ptr()).len = L::from_usize(len) };
    }

    /// Room for @additional more: if there isn't, a new allocation at least
    /// twice the size, from its allocator.
    pub fn reserve(&mut self, additional: usize) -> Result<(), AllocError> {
        let len = self.len();
        let want = len.checked_add(additional).ok_or(AllocError)?;
        if want <= self.capacity() {
            return Ok(());
        }
        if want > L::MAX {
            return Err(AllocError);
        }

        let alloc = self.buf().head().alloc;
        let cap = want.max(self.capacity().saturating_mul(2)).min(L::MAX);
        let head = |room: usize| DstVecHead { len: L::from_usize(len), cap: L::from_usize(room.min(L::MAX)), alloc };
        let new = unsafe { Buf::<T, L, A>::realloc_in(self.head, len, cap, head, &alloc)? };
        let old = self.head;
        // by a release: an RCU reader that sees @new sees what was copied
        unsafe { AtomicPtr::from_ptr((&raw mut self.head).cast::<*mut DstVecHead<L, A>>()) }.store(new.as_ptr(), Release);
        unsafe { Buf::<T, L, A>::free_in(old, &alloc) };
        Ok(())
    }

    /// Its buffer, owned by the caller.
    pub(crate) fn into_raw(self) -> NonNull<DstVecHead<L, A>> {
        ManuallyDrop::new(self).head
    }

    /// Its buffer freed, not its elements: they've been moved out.
    ///
    /// # Safety
    /// Its first len() have been moved out, and nothing but RCU readers uses
    /// it after.
    pub(crate) unsafe fn free_moved(self) {
        let alloc = self.buf().head().alloc;
        unsafe { Buf::<T, L, A>::free_in(self.into_raw(), &alloc) };
    }

    /// The elements of the buffer at @head, as an RCU reader sees them,
    /// their length read whole: as InlineVec::read_rcu().
    ///
    /// # Safety
    /// As InlineVec::read_rcu()'s.
    pub(crate) unsafe fn read_rcu<'a>(head: *mut DstVecHead<L, A>) -> &'a [T] {
        unsafe {
            let buf = Buf::<T, L, A>::from_thin(head.cast());
            let len = ptr::read_volatile(&raw const (*head).len).to_usize();
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

impl<T, L: LenUint, A: Alloc + Copy> Drop for DstVec<T, L, A> {
    fn drop(&mut self) {
        self.clear();
        let alloc = self.buf().head().alloc;
        unsafe { Buf::<T, L, A>::free_in(self.head, &alloc) };
    }
}

impl<T, L: LenUint, A: Alloc + Copy> Deref for DstVec<T, L, A> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        let buf = self.buf();
        unsafe { slice::from_raw_parts(buf.elems.as_ptr().cast(), buf.head().len.to_usize()) }
    }
}

impl<T, L: LenUint, A: Alloc + Copy> DerefMut for DstVec<T, L, A> {
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

    /// Gives 256 bytes for anything up to that, as a mempool gives an element,
    /// and keeps what it's handed back.
    #[derive(Clone, Copy)]
    struct Rounding<'a>(&'a core::cell::Cell<Option<usize>>);

    unsafe impl core::alloc::Allocator for Rounding<'_> {
        fn allocate(&self, layout: core::alloc::Layout) -> Result<NonNull<[u8]>, core::alloc::AllocError> {
            let given = core::alloc::Layout::from_size_align(layout.size().max(256), layout.align()).unwrap();
            std::alloc::Global.allocate(given)
        }

        unsafe fn deallocate(&self, p: NonNull<u8>, layout: core::alloc::Layout) {
            self.0.set(Some(layout.size()));
            let given = core::alloc::Layout::from_size_align(layout.size().max(256), layout.align()).unwrap();
            unsafe { std::alloc::Global.deallocate(p, given) }
        }
    }

    unsafe impl Alloc for Rounding<'_> {}

    #[test]
    fn freed_by_what_it_was_given() {
        let freed = core::cell::Cell::new(None);
        let v = DstVec::<u64, u16, _>::with_capacity_in(2, Rounding(&freed)).unwrap();
        assert_eq!(v.capacity(), (256 - Buf::<u64, u16, Rounding<'_>>::TAIL) / 8, "its room is what the allocator gave");
        drop(v);
        assert_eq!(freed.get(), Some(256), "freed as the size it was given, not asked for");
    }
}
