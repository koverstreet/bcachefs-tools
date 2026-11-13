// SPDX-License-Identifier: GPL-2.0

//! The VFS's iov_iter: a read's or a write's buffer, walked as it's used.

use crate::c;
use core::ops::{Deref, DerefMut};

/// An iov_iter, borrowed from the VFS for a call.
pub struct IovIter<'a>(&'a mut c::iov_iter);

impl<'a> IovIter<'a> {
    /// # Safety
    /// @i is valid, and used by nothing else, for 'a: the VFS's, for the call
    /// it's passed to.
    pub unsafe fn from_raw(i: *mut c::iov_iter) -> Self {
        Self(unsafe { &mut *i })
    }

    pub fn as_ptr(&mut self) -> *mut c::iov_iter {
        self.0
    }

    /// iov_iter_count()
    pub fn count(&self) -> usize {
        unsafe { self.0.__bindgen_anon_1.__bindgen_anon_1.as_ref().count }
    }

    fn set_count(&mut self, count: usize) {
        unsafe { self.0.__bindgen_anon_1.__bindgen_anon_1.as_mut().count = count }
    }

    /// bio_iov_vecs_to_alloc(): the bvecs a bio of its next pages wants, up to
    /// BIO_MAX_VECS.
    pub fn bio_vecs(&mut self) -> u16 {
        unsafe { c::bio_iov_vecs_to_alloc(self.0, c::BIO_MAX_VECS as core::ffi::c_int) as u16 }
    }

    /// user_backed_iter(): its pages are the user's.
    pub fn user_backed(&self) -> bool {
        let ty = self.0.iter_type;
        ty == c::iter_type_ITER_UBUF as u8 || ty == c::iter_type_ITER_IOVEC as u8
    }

    /// Its next @bytes faulted in, to copy from atomically: how many couldn't
    /// be - fault_in_iov_iter_readable().
    #[cfg(kernel)]
    pub fn fault_in_readable(&mut self, bytes: usize) -> usize {
        unsafe { c::fault_in_iov_iter_readable(self.0, bytes) }
    }

    /// What's left of its current segment: iov_iter_single_seg_count().
    #[cfg(kernel)]
    pub fn single_seg_count(&self) -> usize {
        unsafe { c::iov_iter_single_seg_count(&*self.0) }
    }

    #[cfg(kernel)]
    pub fn revert(&mut self, bytes: usize) {
        IovIterFields::revert(self.0, bytes)
    }

    /// Its last @n bytes held back till the guard's dropped: iov_iter_truncate(),
    /// then iov_iter_reexpand() by what was taken.
    pub fn hold_back(&mut self, n: usize) -> HeldBack<'_, 'a> {
        let count = self.count();
        assert!(n <= count, "holding back {n} bytes of an iov_iter of {count}");
        self.set_count(count - n);
        HeldBack { iter: self, n }
    }
}

/// An iov_iter's fields, as a DIO write reads its copy of the VFS's, held for
/// an IO that outlives the call.
#[cfg(kernel)]
pub trait IovIterFields {
    fn count(&self) -> usize;

    /// iter_is_ubuf(): one buffer, held in the iter itself.
    fn is_ubuf(&self) -> bool;

    /// iter_is_iovec()
    fn is_iovec(&self) -> bool;

    fn nr_segs(&self) -> usize;

    /// An iovec iter's iovecs.
    fn iov(&self) -> *const kernel::bindings::iovec;

    /// # Safety
    /// An iovec iter's, @iov a copy of its iovecs, which lives as long as it's
    /// used.
    unsafe fn set_iov(&mut self, iov: *const kernel::bindings::iovec);

    /// iov_iter_revert(): back over the last @bytes taken.
    fn revert(&mut self, bytes: usize);
}

#[cfg(kernel)]
impl IovIterFields for c::iov_iter {
    fn count(&self) -> usize {
        unsafe { self.__bindgen_anon_1.__bindgen_anon_1.as_ref().count }
    }

    fn is_ubuf(&self) -> bool {
        self.iter_type == c::iter_type_ITER_UBUF as u8
    }

    fn is_iovec(&self) -> bool {
        self.iter_type == c::iter_type_ITER_IOVEC as u8
    }

    fn nr_segs(&self) -> usize {
        unsafe { self.__bindgen_anon_2.nr_segs as usize }
    }

    fn iov(&self) -> *const kernel::bindings::iovec {
        unsafe { self.__bindgen_anon_1.__bindgen_anon_1.as_ref().__bindgen_anon_1.__iov }
    }

    unsafe fn set_iov(&mut self, iov: *const kernel::bindings::iovec) {
        unsafe { self.__bindgen_anon_1.__bindgen_anon_1.as_mut().__bindgen_anon_1.__iov = iov }
    }

    fn revert(&mut self, bytes: usize) {
        unsafe { c::iov_iter_revert(self, bytes) }
    }
}

pub struct HeldBack<'b, 'a> {
    iter: &'b mut IovIter<'a>,
    n:    usize,
}

impl<'a> Deref for HeldBack<'_, 'a> {
    type Target = IovIter<'a>;

    fn deref(&self) -> &IovIter<'a> {
        self.iter
    }
}

impl<'a> DerefMut for HeldBack<'_, 'a> {
    fn deref_mut(&mut self) -> &mut IovIter<'a> {
        self.iter
    }
}

impl Drop for HeldBack<'_, '_> {
    fn drop(&mut self) {
        let count = self.iter.count();
        self.iter.set_count(count + self.n);
    }
}
