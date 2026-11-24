// SPDX-License-Identifier: GPL-2.0

//! Thin pointers to a SelfSized type - a DST whose head records its length:
//! one word, the value's address, whatever its length.
//!
//! - ThinRef<'a, T>, ThinMut<'a, T>: a &T or &mut T, thin - for a slot that
//!   has room for a word.
//! - ThinBox<T, A>: an owned one, freed as T says (RcuFreeing): at once, or a
//!   grace period on.
//!
//! Each is made from a reference or a box, and derefs back to the T through
//! SelfSized::from_thin(): the value says how long it is, and nothing holding
//! one can change that.

use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::ops::{Deref, DerefMut};
use core::ptr::NonNull;

use crate::util::alloc::{Alloc, Box};
use crate::util::maybe_rcu_alloc::RcuFreeing;
use crate::util::vstructs::SelfSized;

/// A &'a T, thin.
pub struct ThinRef<'a, T: ?Sized + SelfSized> {
    p:  NonNull<()>,
    _t: PhantomData<&'a T>,
}

// a &T's
unsafe impl<T: ?Sized + SelfSized + Sync> Send for ThinRef<'_, T> {}
unsafe impl<T: ?Sized + SelfSized + Sync> Sync for ThinRef<'_, T> {}

impl<T: ?Sized + SelfSized> Clone for ThinRef<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: ?Sized + SelfSized> Copy for ThinRef<'_, T> {}

impl<'a, T: ?Sized + SelfSized> From<&'a T> for ThinRef<'a, T> {
    fn from(r: &'a T) -> Self {
        ThinRef { p: NonNull::from(r).cast(), _t: PhantomData }
    }
}

impl<'a, T: ?Sized + SelfSized> ThinRef<'a, T> {
    /// What a word that's a T's address - a ThinBox's into_raw(), linked from
    /// something readers walk - refers to.
    ///
    /// # Safety
    /// @p is a valid T's address, live and changed only through atomics for
    /// 'a.
    pub unsafe fn from_raw(p: NonNull<()>) -> Self {
        ThinRef { p, _t: PhantomData }
    }

    pub fn get(self) -> &'a T {
        // SAFETY: a &'a T's address
        unsafe { &*T::from_thin(self.p.as_ptr()) }
    }
}

impl<T: ?Sized + SelfSized> Deref for ThinRef<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.get()
    }
}

/// A &'a mut T, thin.
pub struct ThinMut<'a, T: ?Sized + SelfSized> {
    p:  NonNull<()>,
    _t: PhantomData<&'a mut T>,
}

// a &mut T's
unsafe impl<T: ?Sized + SelfSized + Send> Send for ThinMut<'_, T> {}
unsafe impl<T: ?Sized + SelfSized + Sync> Sync for ThinMut<'_, T> {}

impl<'a, T: ?Sized + SelfSized> From<&'a mut T> for ThinMut<'a, T> {
    fn from(r: &'a mut T) -> Self {
        ThinMut { p: NonNull::from(r).cast(), _t: PhantomData }
    }
}

impl<T: ?Sized + SelfSized> Deref for ThinMut<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: a &mut T's address, borrowed
        unsafe { &*T::from_thin(self.p.as_ptr()) }
    }
}

impl<T: ?Sized + SelfSized> DerefMut for ThinMut<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: a &mut T's address, borrowed mutably
        unsafe { &mut *T::from_thin(self.p.as_ptr()).cast_mut() }
    }
}

/// A Box<T, A>, thin: A has no state - stateless() - and T is freed as its
/// markers say, by its reference's size.
#[repr(transparent)]
#[derive(cstruct_macros::NonZeroWord)]
pub struct ThinBox<T: ?Sized + SelfSized, A: Alloc> {
    p:  NonNull<()>,
    _t: PhantomData<(Box<T, A>, A)>,
}

// a Box<T>'s
unsafe impl<T: ?Sized + SelfSized + Send, A: Alloc> Send for ThinBox<T, A> {}
unsafe impl<T: ?Sized + SelfSized + Sync, A: Alloc> Sync for ThinBox<T, A> {}

// None is its pointer null - zeroed
unsafe impl<T: ?Sized + SelfSized, A: Alloc> ::pin_init::ZeroableOption for ThinBox<T, A> {}

impl<T: ?Sized + SelfSized, A: Alloc> ThinBox<T, A> {
    pub fn new(b: Box<T, A>) -> Self {
        const { assert!(size_of::<A>() == 0, "a ThinBox from an allocator with state: it keeps none") };
        // the Box's allocation, ours: A has no state
        let mut b = ManuallyDrop::new(b);
        ThinBox { p: Box::as_non_null(&mut b).cast(), _t: PhantomData }
    }

    /// Its address, for something that holds a word - an atomic, a union's
    /// arm, a field C reads - to hand back to from_raw().
    pub fn into_raw(b: Self) -> NonNull<()> {
        ManuallyDrop::new(b).p
    }

    /// # Safety
    /// @p is a T that's had by nothing else: into_raw()'s, of a ThinBox<T, A>,
    /// this the one from_raw() of it - or A's allocation of a T, at
    /// Layout::for_value()'s size.
    pub unsafe fn from_raw(p: NonNull<()>) -> Self {
        ThinBox { p, _t: PhantomData }
    }

    pub fn as_ref(&self) -> ThinRef<'_, T> {
        ThinRef { p: self.p, _t: PhantomData }
    }

    pub fn as_mut(&mut self) -> ThinMut<'_, T> {
        ThinMut { p: self.p, _t: PhantomData }
    }
}

impl<T: ?Sized + SelfSized, A: Alloc> Deref for ThinBox<T, A> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: a Box<T>'s address
        unsafe { &*T::from_thin(self.p.as_ptr()) }
    }
}

impl<T: ?Sized + SelfSized, A: Alloc> DerefMut for ThinBox<T, A> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: a Box<T>'s address, ours
        unsafe { &mut *T::from_thin(self.p.as_ptr()).cast_mut() }
    }
}

impl<T: ?Sized + SelfSized, A: Alloc> Drop for ThinBox<T, A> {
    fn drop(&mut self) {
        // SAFETY: a Box<T, A>'s, ours
        unsafe { <T as RcuFreeing>::free::<A>(NonNull::new_unchecked(T::from_thin(self.p.as_ptr()).cast_mut())) }
    }
}
