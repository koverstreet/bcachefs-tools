// SPDX-License-Identifier: GPL-2.0

//! core::cell::RefCell, made in place: by an initializer for its value, or
//! zeroed - for a value too big to build on the stack and move.

use core::cell::{Cell, UnsafeCell};
use core::ops::{Deref, DerefMut};
use pin_init::{Init, Zeroable};

pub struct RefCell<T> {
    /* readers, or -1 for the writer */
    borrow: Cell<isize>,
    value:  UnsafeCell<T>,
}

// zeroed, not borrowed: as its value is
unsafe impl<T: Zeroable> Zeroable for RefCell<T> {}

impl<T> RefCell<T> {
    pub const fn new(v: T) -> Self {
        RefCell { borrow: Cell::new(0), value: UnsafeCell::new(v) }
    }

    /// A RefCell holding the value @init makes, in place.
    pub fn init<E>(init: impl Init<T, E>) -> impl Init<Self, E> {
        unsafe {
            ::pin_init::init_from_closure(move |slot: *mut Self| {
                (&raw mut (*slot).borrow).write(Cell::new(0));
                crate::util::alloc::init_at(init, UnsafeCell::raw_get(&raw const (*slot).value))
            })
        }
    }

    #[track_caller]
    pub fn borrow(&self) -> Ref<'_, T> {
        let b = self.borrow.get();
        assert!(b >= 0, "a RefCell borrowed while it's borrowed mutably");
        self.borrow.set(b + 1);
        Ref(self)
    }

    #[track_caller]
    pub fn borrow_mut(&self) -> RefMut<'_, T> {
        assert!(self.borrow.get() == 0, "a RefCell borrowed mutably while it's borrowed");
        self.borrow.set(-1);
        RefMut(self)
    }

    pub fn get_mut(&mut self) -> &mut T {
        self.value.get_mut()
    }
}

pub struct Ref<'a, T>(&'a RefCell<T>);

impl<T> Deref for Ref<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.0.value.get() }
    }
}

impl<T> Drop for Ref<'_, T> {
    fn drop(&mut self) {
        self.0.borrow.set(self.0.borrow.get() - 1);
    }
}

pub struct RefMut<'a, T>(&'a RefCell<T>);

impl<T> Deref for RefMut<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.0.value.get() }
    }
}

impl<T> DerefMut for RefMut<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.0.value.get() }
    }
}

impl<T> Drop for RefMut<'_, T> {
    fn drop(&mut self) {
        self.0.borrow.set(0);
    }
}
