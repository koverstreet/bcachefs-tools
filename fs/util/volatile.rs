// SPDX-License-Identifier: GPL-2.0

//! VolatileSlice: memory that may be written while we read it - a bio's
//! pages: a user's buffer under direct IO, a page cache page dirtied under
//! writeback. Never a slice: it's read by copying out, written by copying in,
//! or handed to C as a pointer. One is a slice only when it's ours, or
//! known stable - assume_stable(), which says why.
//!
//! What's persisted has to be computed from a slice: compressing, or
//! checksumming for a write, memory that changed underneath would write the
//! corruption out. Decompressing into it is fine - a user scribbling on their
//! own read buffer gets a checksum error, and the retry bounces.

use core::marker::PhantomData;
use core::ops::Range;
use core::ptr::{self, NonNull};

/// # Invariants
/// @p is @len bytes, valid for 'a, which only C and these copies touch.
#[derive(Clone, Copy)]
pub struct VolatileSlice<'a> {
    p:   NonNull<u8>,
    len: usize,
    _a:  PhantomData<&'a mut [u8]>,
}

impl<'a> VolatileSlice<'a> {
    /// # Safety
    /// @p is @len bytes, valid for 'a, and no Rust reference to it is used
    /// meanwhile.
    pub unsafe fn from_raw(p: NonNull<u8>, len: usize) -> Self {
        VolatileSlice { p, len, _a: PhantomData }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// For C.
    pub fn as_ptr(&self) -> *mut u8 {
        self.p.as_ptr()
    }

    /// Panics past its end.
    pub fn sub(self, r: Range<usize>) -> Self {
        assert!(r.start <= r.end && r.end <= self.len, "{r:?} of a volatile slice of {}", self.len);
        // SAFETY: within it
        unsafe { Self::from_raw(self.p.add(r.start), r.end - r.start) }
    }

    pub fn overlaps(&self, other: &VolatileSlice<'_>) -> bool {
        let (a, b) = (self.p.as_ptr().addr(), other.p.as_ptr().addr());
        a < b + other.len && b < a + self.len
    }

    pub fn fill(&self, b: u8) {
        // SAFETY: the invariant
        unsafe { ptr::write_bytes(self.as_ptr(), b, self.len) }
    }

    /// As a slice.
    ///
    /// # Safety
    /// Nothing writes it for 'a, and it's initialized: ours and written, or
    /// pages known stable.
    pub unsafe fn assume_stable(self) -> &'a [u8] {
        // SAFETY: the caller's
        unsafe { core::slice::from_raw_parts(self.as_ptr(), self.len) }
    }
}
