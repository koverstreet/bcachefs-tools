// SPDX-License-Identifier: GPL-2.0

//! A type parameter for a structure reached from several threads in one
//! place and owned by one in another: Shared, its fields are what that
//! takes; Unshared, they're plain, had through &mut.

use core::cell::UnsafeCell;

pub trait Sharing {
    /// What a lock protects.
    type Cell<T>;

    fn cell<T>(v: T) -> Self::Cell<T>;
    fn cell_get_mut<T>(c: &mut Self::Cell<T>) -> &mut T;
}

pub enum Shared {}

pub enum Unshared {}

impl Sharing for Shared {
    type Cell<T> = LockCell<T>;

    fn cell<T>(v: T) -> LockCell<T> {
        LockCell(UnsafeCell::new(v))
    }

    fn cell_get_mut<T>(c: &mut LockCell<T>) -> &mut T {
        c.0.get_mut()
    }
}

impl Sharing for Unshared {
    type Cell<T> = T;

    fn cell<T>(v: T) -> T {
        v
    }

    fn cell_get_mut<T>(c: &mut T) -> &mut T {
        c
    }
}

/// A T a lock protects, reached by one holder at a time - so sent between
/// threads, never shared.
#[repr(transparent)]
pub struct LockCell<T>(UnsafeCell<T>);

unsafe impl<T: Send> Sync for LockCell<T> {}

unsafe impl<T: ::pin_init::Zeroable> ::pin_init::Zeroable for LockCell<T> {}

impl<T> LockCell<T> {
    pub fn get(&self) -> *mut T {
        self.0.get()
    }
}
