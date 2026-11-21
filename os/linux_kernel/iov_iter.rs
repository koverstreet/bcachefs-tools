// SPDX-License-Identifier: GPL-2.0

//! The VFS's iov_iter: a read's or a write's buffer, walked as it's used.

use crate::c;
#[cfg(kernel)]
use crate::data_structures::inline_vec::InlineVec;
use crate::errcode::{ret_to_result_void, BchError};
#[cfg(kernel)]
use crate::util::alloc::AllocError;
use core::ffi::{c_int, c_void};
use core::mem::MaybeUninit;
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

    /// Our own, in @slot, over @len bytes of user memory at @addr, to read
    /// into: import_ubuf(), which checks the range is the user's.
    pub fn user_dest(slot: &'a mut MaybeUninit<c::iov_iter>, addr: u64, len: usize)
        -> Result<Self, BchError>
    {
        ret_to_result_void(unsafe {
            c::import_ubuf(c::ITER_DEST as c_int, addr as usize as *mut c_void, len, slot.as_mut_ptr())
        })?;
        Ok(Self(unsafe { slot.assume_init_mut() }))
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

    /// iov_iter_revert(): back over the last @bytes taken.
    #[cfg(kernel)]
    pub fn revert(&mut self, bytes: usize) {
        unsafe { c::iov_iter_revert(self.0, bytes) }
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

/// An iov_iter held for an IO that outlives the call it was passed to: a copy
/// of the VFS's, its iovecs the caller's till keep_iovecs() copies them in.
#[cfg(kernel)]
pub struct HeldIovIter {
    iter: c::iov_iter,
    iov:  InlineVec<kernel::bindings::iovec, 2>,
}

// its buffers are user addresses, and its iovecs its own, or the caller's
// while it waits
#[cfg(kernel)]
unsafe impl Send for HeldIovIter {}

#[cfg(kernel)]
impl HeldIovIter {
    /// # Safety
    /// @i's iovecs outlive every use of it before keep_iovecs().
    pub unsafe fn new(i: &mut IovIter<'_>) -> Self {
        Self { iter: unsafe { core::ptr::read(&*i.0) }, iov: InlineVec::new() }
    }

    /// iter_is_ubuf(): one buffer, held in the iter itself.
    pub fn is_ubuf(&self) -> bool {
        self.iter.iter_type == c::iter_type_ITER_UBUF as u8
    }

    /// iter_is_iovec()
    pub fn is_iovec(&self) -> bool {
        self.iter.iter_type == c::iter_type_ITER_IOVEC as u8
    }

    fn nr_segs(&self) -> usize {
        unsafe { self.iter.__bindgen_anon_2.nr_segs as usize }
    }

    fn iov_ptr(&mut self) -> &mut *const kernel::bindings::iovec {
        unsafe { &mut self.iter.__bindgen_anon_1.__bindgen_anon_1.as_mut().__bindgen_anon_1.__iov }
    }

    /// The iter, its position in its own iovecs - wherever they've moved to
    /// with it - if they're its own.
    pub fn get(&mut self) -> IovIter<'_> {
        if !self.iov.is_empty() {
            // an iovec iter's position is its segments left, ending the array
            let at = self.iov.len() - self.nr_segs();
            *self.iov_ptr() = self.iov[at..].as_ptr();
        }
        IovIter(&mut self.iter)
    }

    /// An iovec iter's iovecs left copied in, so it no longer needs the
    /// caller's.
    pub fn keep_iovecs(&mut self) -> Result<(), AllocError> {
        assert!(self.is_iovec() && self.iov.is_empty(), "keeping the iovecs of an iter of type {}, kept already: {}",
                self.iter.iter_type, !self.iov.is_empty());

        let src = unsafe { core::slice::from_raw_parts(*self.iov_ptr(), self.nr_segs()) };
        self.iov.reserve(src.len())?;
        for v in src {
            // the kernel's iovec isn't Copy
            self.iov.push(kernel::bindings::iovec { iov_base: v.iov_base, iov_len: v.iov_len })?;
        }
        Ok(())
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
