// SPDX-License-Identifier: GPL-2.0

//! DstVec<T, L, A>: a vector on a DstArray, its length the header and its
//! capacity the count, owned by a ThinBox - so it's a single pointer, its low
//! bit clear: InlineVec tags it. Its allocator is kept before its buffer, so
//! one with state - a bio's BvecAlloc - is what it grows from and frees to.

use crate::data_structures::array_vec::LenUint;
use crate::data_structures::dst_array::DstArray;
use crate::util::alloc::{Alloc, AllocError, Global};
use crate::util::thin::ThinBox;
use crate::util::vstructs::{FlexArray, SelfSized};
use core::marker::PhantomData;
use core::mem::{ManuallyDrop, MaybeUninit};
use core::ops::{Deref, DerefMut};
use core::ptr::{self, NonNull};
use core::slice;
use core::sync::atomic::{AtomicPtr, Ordering::Release};

#[repr(C)]
pub struct DstVecHead<L> {
    len: L,
}

type Buf<T, L> = DstArray<DstVecHead<L>, MaybeUninit<T>, L>;

#[repr(transparent)]
pub struct DstVec<T, L: LenUint = u32, A: Alloc + Copy = crate::os::alloc::Global> {
    buf: ThinBox<Buf<T, L>, A>,
    _t:  PhantomData<T>,
}

impl<T, L: LenUint> DstVec<T, L> {
    pub fn with_capacity(cap: usize) -> Result<Self, AllocError> {
        Self::with_capacity_in(cap, Global)
    }

    /// with_capacity(), for what mustn't fail to allocate: past its length
    /// type is a bug.
    pub fn with_capacity_nofail(cap: usize) -> Self {
        assert!(cap <= L::MAX, "a DstVec of {cap}, past its length type");
        Self { buf: Buf::<T, L>::alloc_nofail(cap, DstVecHead { len: L::ZERO }), _t: PhantomData }
    }
}

impl<T, L: LenUint, A: Alloc + Copy> DstVec<T, L, A> {
    /// with_capacity(), from @alloc: its room what @alloc gave, which it's
    /// freed by.
    pub fn with_capacity_in(cap: usize, alloc: A) -> Result<Self, AllocError> {
        if cap > L::MAX {
            return Err(AllocError);
        }
        Ok(Self { buf: Buf::<T, L>::alloc_in(cap, DstVecHead { len: L::ZERO }, &alloc)?, _t: PhantomData })
    }

    /// What one with room for @cap is allocated as, its allocator before it:
    /// a mempool's element, for a pool of them.
    pub fn alloc_size(cap: usize) -> usize {
        Buf::<T, L>::layout::<A>(cap).expect("a DstVec's layout").0.size()
    }

    pub fn len(&self) -> usize {
        self.buf.head().len.to_usize()
    }

    pub fn capacity(&self) -> usize {
        self.buf.nr()
    }

    pub fn as_mut_ptr(&mut self) -> *mut T {
        self.buf.elems.as_mut_ptr().cast()
    }

    /// # Safety
    /// The first @len are initialized, and @len is within its capacity.
    pub unsafe fn set_len(&mut self, len: usize) {
        self.buf.head_mut().len = L::from_usize(len);
    }

    /// Room for @additional more: if there isn't, a new allocation at least
    /// twice the size, from its allocator; what it's grown out of freed as its
    /// elements say.
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
        let a = *ThinBox::allocator(&self.buf);
        let new = unsafe { Buf::<T, L>::realloc_in(&self.buf, len, cap, DstVecHead { len: L::from_usize(len) }, &a)? };
        // by a release: an RCU reader that sees @new sees what was copied
        let word = unsafe { AtomicPtr::from_ptr((&raw mut self.buf).cast::<*mut ()>()) };
        let old = word.swap(ThinBox::into_raw(new).as_ptr(), Release);
        drop(unsafe { ThinBox::<Buf<T, L>, A>::from_raw(NonNull::new_unchecked(old)) });
        Ok(())
    }

    /// Its buffer, owned by the caller, as a word: from_raw()'s to take back.
    pub(crate) fn into_raw(self) -> NonNull<()> {
        ThinBox::as_ptr(&ManuallyDrop::new(self).buf)
    }

    /// The elements of the buffer at @p, as an RCU reader sees them, their
    /// length read whole: as InlineVec::read_rcu().
    ///
    /// # Safety
    /// As InlineVec::read_rcu()'s, @p a DstVec's into_raw().
    pub(crate) unsafe fn read_rcu<'a>(p: *mut ()) -> &'a [T] {
        unsafe {
            let buf = Buf::<T, L>::from_thin(p);
            let len = ptr::read_volatile(&raw const (*p.cast::<DstVecHead<L>>()).len).to_usize();
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
    }
}

impl<T, L: LenUint, A: Alloc + Copy> Deref for DstVec<T, L, A> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        unsafe { slice::from_raw_parts(self.buf.elems.as_ptr().cast(), self.len()) }
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
        assert_eq!(v.capacity(), (256 - size_of::<Rounding>() - Buf::<u64, u16>::TAIL) / 8,
                   "its room is what the allocator gave, past the allocator kept before it");
        drop(v);
        assert_eq!(freed.get(), Some(256), "freed as the size it was given, not asked for");
    }
}
