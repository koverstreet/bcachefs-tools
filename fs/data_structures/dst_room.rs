// SPDX-License-Identifier: GPL-2.0

//! DstRoom<T, B>: one T - an Unsized whose header says how long it is - at
//! the front of its storage B, the rest room for it to grow into: ArrayVec's,
//! for a single value. B is [E; N], its room a type parameter, or [E], a
//! value's: one struct, either dereferencing to its T, and the first coercing
//! to the second.
//!
//! The storage is its own element type, not T's: a T's tail may be bytes
//! under a header of words, as a typed key's value is under its bkey.

use crate::util::vstructs::{from_head_ptr, from_head_ptr_mut, Plain, SelfSized, Unsized};
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};

#[repr(transparent)]
pub struct DstRoom<T: ?Sized, B: ?Sized = [u64]> {
    _t:  PhantomData<fn() -> *const T>,
    buf: B,
}

/// A DstRoom's storage: an array of plain elements, its length a type
/// parameter or a value.
pub trait Storage {
    type Elem: Plain;

    fn elems(&self) -> &[Self::Elem];
    fn elems_mut(&mut self) -> &mut [Self::Elem];
}

impl<E: Plain> Storage for [E] {
    type Elem = E;

    fn elems(&self) -> &[E] {
        self
    }

    fn elems_mut(&mut self) -> &mut [E] {
        self
    }
}

impl<E: Plain, const N: usize> Storage for [E; N] {
    type Elem = E;

    fn elems(&self) -> &[E] {
        self
    }

    fn elems_mut(&mut self) -> &mut [E] {
        self
    }
}

impl<T: ?Sized, E: Plain> DstRoom<T, [E]> {
    /// @buf as a room, holding what it holds.
    pub fn from_elems(buf: &[E]) -> &Self {
        unsafe { &*(core::ptr::from_ref(buf) as *const Self) }
    }

    pub fn from_elems_mut(buf: &mut [E]) -> &mut Self {
        unsafe { &mut *(core::ptr::from_mut(buf) as *mut Self) }
    }
}

impl<T: ?Sized, E: Plain, const N: usize> DstRoom<T, [E; N]> {
    /// All zeroes: what T's header says of that is T's.
    pub const fn zeroed() -> Self {
        DstRoom { _t: PhantomData, buf: unsafe { core::mem::zeroed() } }
    }
}

impl<T: ?Sized, B: ?Sized + Storage> DstRoom<T, B> {
    /// Its room, in bytes: the most the T may grow to.
    pub fn capacity(&self) -> usize {
        size_of_val(self.buf.elems())
    }

    /// All of its storage, the T first: for a T made in place, as by
    /// unpacking one. What reads it checks its header against the room.
    pub fn elems(&self) -> &[B::Elem] {
        self.buf.elems()
    }

    pub fn elems_mut(&mut self) -> &mut [B::Elem] {
        self.buf.elems_mut()
    }
}

impl<T: ?Sized + Unsized, B: ?Sized + Storage> DstRoom<T, B>
where
    T::Head: Plain + SelfSized,
{
    const ALIGNED: () = assert!(align_of::<T::Head>() <= align_of::<B::Elem>(),
                                "a DstRoom's storage, less aligned than what it holds");

    fn check(&self) -> *const T::Head {
        let () = Self::ALIGNED;
        let cap = self.capacity();
        assert!(size_of::<T::Head>() <= cap, "a room of {cap} bytes, short of its header");

        let head = self.buf.elems().as_ptr().cast::<T::Head>();
        let bytes = unsafe { (*head).bytes() };
        assert!(bytes <= cap, "a value of {bytes} bytes in a room of {cap}");
        head
    }
}

impl<T: ?Sized + Unsized, B: ?Sized + Storage> Deref for DstRoom<T, B>
where
    T::Head: Plain + SelfSized,
{
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { from_head_ptr(self.check()) }
    }
}

impl<T: ?Sized + Unsized, B: ?Sized + Storage> DerefMut for DstRoom<T, B>
where
    T::Head: Plain + SelfSized,
{
    fn deref_mut(&mut self) -> &mut T {
        unsafe { from_head_ptr_mut(self.check().cast_mut()) }
    }
}

impl<T: ?Sized, E: Plain + Copy, const N: usize> Clone for DstRoom<T, [E; N]> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: ?Sized, E: Plain + Copy, const N: usize> Copy for DstRoom<T, [E; N]> {}

// plain storage, every bit pattern of which is some T's room
unsafe impl<T: ?Sized, E: Plain, const N: usize> ::pin_init::Zeroable for DstRoom<T, [E; N]> {}
