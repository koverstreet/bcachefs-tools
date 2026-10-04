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

pub mod flags {
    use super::Flags;

    pub const GFP_KERNEL: Flags = Flags(0);
    pub const GFP_NOWAIT: Flags = Flags(1);
}

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

    pub fn clear(&mut self) {
        self.0.clear()
    }

    pub fn truncate(&mut self, len: usize) {
        self.0.truncate(len)
    }

    pub fn capacity(&self) -> usize {
        self.0.capacity()
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
