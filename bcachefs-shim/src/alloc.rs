// SPDX-License-Identifier: GPL-2.0
//! Userspace stand-in for the subset of `kernel::alloc` that fs/ uses, with
//! the same names and signatures, so fs/ code is written once against the
//! kernel's API.
//!
//! Allocation is fallible, as in-kernel: push() reports a failed allocation
//! as AllocError instead of aborting, via try_reserve(). Flags are accepted
//! and ignored - userspace has one way to allocate.

use std::ops::{Deref, DerefMut};

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct AllocError;

#[derive(Copy, Clone, PartialEq, Eq)]
pub struct Flags(u32);

impl core::ops::BitOr for Flags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Flags(self.0 | rhs.0)
    }
}

pub mod flags {
    use super::Flags;

    pub const GFP_KERNEL: Flags = Flags(0);
    pub const GFP_NOWAIT: Flags = Flags(1);
    pub const __GFP_NOWARN: Flags = Flags(2);
}

extern "C" {
    fn realloc(p: *mut core::ffi::c_void, size: usize) -> *mut core::ffi::c_void;
}

/// krealloc(), as the shim's slab.h has it: realloc(), its memory what C's
/// kfree() - free() - takes back. Null if it can't.
///
/// # Safety
/// @p is null, or malloc()'s and not used after unless this fails.
pub unsafe fn krealloc(p: *mut u8, size: usize) -> *mut u8 {
    // SAFETY: the caller's
    unsafe { realloc(p.cast(), size).cast() }
}

/// What push_within_capacity() gives back: there was no room for it.
pub struct PushError<T>(pub T);

/// As the kernel's KVVec: a vector whose allocations can fail.
pub struct KVVec<T>(Vec<T>);

impl<T> KVVec<T> {
    pub const fn new() -> Self {
        KVVec(Vec::new())
    }

    pub fn with_capacity(capacity: usize, _flags: Flags) -> Result<Self, AllocError> {
        let mut v = Vec::new();
        v.try_reserve(capacity).map_err(|_| AllocError)?;
        Ok(KVVec(v))
    }

    /// Make room for @additional more: grows to max(capacity * 2, len +
    /// additional), as the kernel's does.
    pub fn reserve(&mut self, additional: usize, _flags: Flags) -> Result<(), AllocError> {
        self.0.try_reserve(additional).map_err(|_| AllocError)
    }

    pub fn push(&mut self, v: T, _flags: Flags) -> Result<(), AllocError> {
        self.0.try_reserve(1).map_err(|_| AllocError)?;
        self.0.push(v);
        Ok(())
    }

    /// @v pushed if there's room, else given back.
    pub fn push_within_capacity(&mut self, v: T) -> Result<(), PushError<T>> {
        if self.0.len() < self.0.capacity() {
            self.0.push(v);
            Ok(())
        } else {
            Err(PushError(v))
        }
    }

    /// Every element, by value; the buffer's kept.
    pub fn drain_all(&mut self) -> std::vec::Drain<'_, T> {
        self.0.drain(..)
    }

    pub fn extend_from_slice(&mut self, other: &[T], _flags: Flags) -> Result<(), AllocError>
    where
        T: Clone,
    {
        self.0.try_reserve(other.len()).map_err(|_| AllocError)?;
        self.0.extend_from_slice(other);
        Ok(())
    }

    /// @n clones of @value.
    pub fn from_elem(value: T, n: usize, _flags: Flags) -> Result<Self, AllocError>
    where
        T: Clone,
    {
        let mut v = Vec::new();
        v.try_reserve(n).map_err(|_| AllocError)?;
        v.resize(n, value);
        Ok(KVVec(v))
    }

    pub fn clear(&mut self) {
        self.0.clear()
    }

    pub fn truncate(&mut self, len: usize) {
        self.0.truncate(len)
    }

    pub fn pop(&mut self) -> Option<T> {
        self.0.pop()
    }

    /// Keep what @f says to, in order; drop the rest.
    pub fn retain(&mut self, f: impl FnMut(&mut T) -> bool) {
        self.0.retain_mut(f)
    }

    pub fn capacity(&self) -> usize {
        self.0.capacity()
    }

    /// # Safety
    /// As Vec::from_raw_parts(): @ptr from this allocator - malloc()'s, which
    /// a kvmalloc()'d buffer in userspace is - @capacity long, @length of it
    /// initialized; or dangling, for none.
    pub unsafe fn from_raw_parts(ptr: *mut T, length: usize, capacity: usize) -> Self {
        // SAFETY: the caller's
        KVVec(unsafe { Vec::from_raw_parts(ptr, length, capacity) })
    }

    /// Its buffer, length and capacity, for C to own: the buffer is
    /// malloc()'d, which C's kvfree() frees.
    pub fn into_raw_parts(self) -> (*mut T, usize, usize) {
        let mut v = core::mem::ManuallyDrop::new(self.0);
        (v.as_mut_ptr(), v.len(), v.capacity())
    }
}

impl<T> Default for KVVec<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Deref for KVVec<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        &self.0
    }
}

impl<T> DerefMut for KVVec<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        &mut self.0
    }
}

/// As the kernel's Vec: its elements, by value.
impl<T> IntoIterator for KVVec<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}
