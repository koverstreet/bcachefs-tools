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
use core::mem::MaybeUninit;
use core::ops::Range;
use core::ptr::{self, NonNull};

/// # Invariants
/// @p is @len bytes, initialized and valid for 'a, which only C and these
/// copies touch.
#[derive(Clone, Copy)]
pub struct VolatileSlice<'a> {
    p:   NonNull<u8>,
    len: usize,
    _a:  PhantomData<&'a mut [u8]>,
}

impl<'a> VolatileSlice<'a> {
    /// # Safety
    /// @p is @len bytes, initialized and valid for 'a, and no Rust reference
    /// to it is used meanwhile.
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

    /// Panics if the lengths differ, as a slice's does.
    pub fn copy_to_slice(&self, dst: &mut [u8]) {
        assert_eq!(self.len, dst.len(), "copying a volatile slice into a slice of another length");
        // SAFETY: the invariant: no reference is to it, so @dst isn't
        unsafe { ptr::copy_nonoverlapping(self.as_ptr(), dst.as_mut_ptr(), self.len) }
    }

    /// Whether it's @b's bytes, as they are as it's read.
    pub fn eq_slice(&self, b: &[u8]) -> bool {
        const CHUNK: usize = 256;
        let mut buf = [0u8; CHUNK];
        self.len == b.len() && b.chunks(CHUNK).enumerate().all(|(i, c)| {
            let buf = &mut buf[..c.len()];
            self.sub(i * CHUNK..i * CHUNK + c.len()).copy_to_slice(buf);
            buf == c
        })
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

/// Memory of our own, read and written only through it while it's borrowed.
impl<'a> From<&'a mut [u8]> for VolatileSlice<'a> {
    fn from(b: &'a mut [u8]) -> Self {
        // SAFETY: borrowed for 'a, so no other reference is used meanwhile
        unsafe { Self::from_raw(NonNull::from(&mut *b).cast(), b.len()) }
    }
}

/// Where C writes - a codec, its output: a bio's memory, or memory of ours not
/// yet written. Written, never read: what's written to it is had back as a
/// VolatileSlice - from C, by assume_init(), saying how much.
#[derive(Clone, Copy)]
pub struct Out<'a> {
    p:   NonNull<u8>,
    len: usize,
    _a:  PhantomData<&'a mut [u8]>,
}

impl<'a> Out<'a> {
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

    pub fn overlaps(&self, other: &VolatileSlice<'_>) -> bool {
        let (a, b) = (self.p.as_ptr().addr(), other.as_ptr().addr());
        a < b + other.len() && b < a + self.len
    }

    /// Panics past its end.
    pub fn sub(self, r: Range<usize>) -> Self {
        assert!(r.start <= r.end && r.end <= self.len, "{r:?} of an output buffer of {}", self.len);
        // SAFETY: within it
        Out { p: unsafe { self.p.add(r.start) }, len: r.end - r.start, _a: PhantomData }
    }

    /// It, written.
    pub fn fill(self, b: u8) -> VolatileSlice<'a> {
        // SAFETY: its bytes, written only by C and these; then every one has
        // been
        unsafe {
            ptr::write_bytes(self.as_ptr(), b, self.len);
            self.assume_init()
        }
    }

    /// It, written. Panics if the lengths differ, as a slice's does.
    pub fn copy_from_slice(self, src: &[u8]) -> VolatileSlice<'a> {
        assert_eq!(self.len, src.len(), "copying a slice into an output buffer of another length");
        // SAFETY: as fill(): and no reference is to it, so @src isn't
        unsafe {
            ptr::copy_nonoverlapping(src.as_ptr(), self.as_ptr(), self.len);
            self.assume_init()
        }
    }

    /// @a and @b, written within it, the one right after the other, as one.
    pub fn joined(self, a: VolatileSlice<'a>, b: VolatileSlice<'a>) -> VolatileSlice<'a> {
        let (base, start, mid) = (self.p.as_ptr().addr(), a.as_ptr().addr(), b.as_ptr().addr());
        assert!(base <= start && start + a.len() == mid && mid + b.len() <= base + self.len,
                "joining what isn't written one after the other within an output buffer");
        // SAFETY: both written, so all of it between
        unsafe { self.sub(start - base..mid + b.len() - base).assume_init() }
    }

    /// Its bytes, written.
    ///
    /// # Safety
    /// Every one has been.
    pub unsafe fn assume_init(self) -> VolatileSlice<'a> {
        // SAFETY: the caller's, and its own
        unsafe { VolatileSlice::from_raw(self.p, self.len) }
    }
}

impl<'a> From<VolatileSlice<'a>> for Out<'a> {
    fn from(v: VolatileSlice<'a>) -> Self {
        Out { p: v.p, len: v.len, _a: PhantomData }
    }
}

impl<'a> From<&'a mut [MaybeUninit<u8>]> for Out<'a> {
    fn from(b: &'a mut [MaybeUninit<u8>]) -> Self {
        Out { p: NonNull::from(&mut *b).cast(), len: b.len(), _a: PhantomData }
    }
}
