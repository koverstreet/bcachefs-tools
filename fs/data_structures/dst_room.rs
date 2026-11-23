// SPDX-License-Identifier: GPL-2.0

//! DstRoom<T, B>: one T - an Unsized whose header says how long it is - at
//! the front of its storage B, the rest room for it to grow into: ArrayVec's,
//! for a single value. B is [E; N], its room a type parameter, or [E], a
//! value's: one struct, either dereferencing to its T, and the first coercing
//! to the second. Or B is an InlineOrBoxed - InlineVec's, for a single value:
//! its room in itself, and once that's outgrown, a boxed slice.
//!
//! The storage is its own element type, not T's: a T's tail may be bytes
//! under a header of words, as a typed key's value is under its bkey.

use crate::util::alloc::{flags, AllocGfp, Box, Kmalloc};
use crate::util::vstructs::{FakeDst, Plain, Unsized};
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};

#[repr(transparent)]
pub struct DstRoom<T: ?Sized, B: ?Sized = [u64]> {
    _t:  PhantomData<fn() -> *const T>,
    buf: B,
}

/// A DstRoom's storage: an array of plain elements, its length a type
/// parameter or a value - or an InlineOrBoxed's, which grows.
pub trait Storage {
    type Elem: Plain;

    fn elems(&self) -> &[Self::Elem];
    fn elems_mut(&mut self) -> &mut [Self::Elem];

    /// All of it, grown to at least @n elements if it grows - what it didn't
    /// have zeroed. A fixed room doesn't, and is what it is.
    fn grow(&mut self, _n: usize) -> &mut [Self::Elem] {
        self.elems_mut()
    }
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

/// @N elements in itself, and once they're outgrown, a boxed slice - grown
/// __GFP_NOFAIL, doubling, so a value growing a word at a time isn't a copy
/// a word. Once boxed it stays boxed.
pub enum InlineOrBoxed<E, const N: usize> {
    Inline([E; N]),
    Boxed(Box<[E], Kmalloc>),
}

impl<E: Plain + Copy + ::pin_init::Zeroable, const N: usize> Storage for InlineOrBoxed<E, N> {
    type Elem = E;

    fn elems(&self) -> &[E] {
        match self {
            Self::Inline(a) => a,
            Self::Boxed(b)  => b,
        }
    }

    fn elems_mut(&mut self) -> &mut [E] {
        match self {
            Self::Inline(a) => a,
            Self::Boxed(b)  => b,
        }
    }

    /// Inline, with room, is all that's inline: the rest - growing, or a
    /// room that's grown - is grow_slow()'s.
    #[inline(always)]
    fn grow(&mut self, n: usize) -> &mut [E] {
        if n <= N && matches!(self, Self::Inline(_)) {
            let Self::Inline(a) = self else { unreachable!() };
            return a;
        }
        self.grow_slow(n)
    }
}

impl<E: Plain + Copy + ::pin_init::Zeroable, const N: usize> InlineOrBoxed<E, N> {
    #[cold]
    #[inline(never)]
    fn grow_slow(&mut self, n: usize) -> &mut [E] {
        let have = self.elems().len();
        if n > have {
            let mut b = Box::try_zeroed_slice_in(n.max(have * 2), Kmalloc.with_flags(flags::NOFAIL))
                .expect("a __GFP_NOFAIL allocation");
            b[..have].copy_from_slice(self.elems());
            *self = Self::Boxed(Box::into_unflagged(b));
        }
        self.elems_mut()
    }
}

impl<T: ?Sized, E: Plain> DstRoom<T, [E]> {
    /// @buf as a room, holding what it holds.
    pub fn from_elems(buf: &[E]) -> &Self {
        unsafe { &*(core::ptr::from_ref(buf) as *const Self) }
    }

    pub fn from_elems_mut(buf: &mut [E]) -> &mut Self {
        unsafe { &mut *Self::from_elems_raw(buf) }
    }

    /// @buf as a room: for a box of [E] made one.
    pub fn from_elems_raw(buf: *mut [E]) -> *mut Self {
        buf as *mut Self
    }
}

impl<T: ?Sized, E: Plain, const N: usize> DstRoom<T, [E; N]> {
    /// All zeroes: what T's header says of that is T's.
    pub const fn zeroed() -> Self {
        DstRoom { _t: PhantomData, buf: unsafe { core::mem::zeroed() } }
    }
}

impl<T: ?Sized, E: ::pin_init::Zeroable, const N: usize> DstRoom<T, InlineOrBoxed<E, N>> {
    /// Its @N inline elements, all zeroes: what T's header says of that is
    /// T's.
    pub fn zeroed() -> Self {
        DstRoom { _t: PhantomData, buf: InlineOrBoxed::Inline(::pin_init::zeroed()) }
    }
}

impl<T: ?Sized, B: ?Sized + Storage> DstRoom<T, B> {
    /// Its room, in bytes: the most the T may grow to.
    pub fn capacity(&self) -> usize {
        size_of_val(self.buf.elems())
    }

    /// All of its storage, grown to room for @n elements if it grows:
    /// Storage::grow().
    pub fn grow(&mut self, n: usize) -> &mut [B::Elem] {
        Storage::grow(&mut self.buf, n)
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
    T::Head: Plain + FakeDst,
{
    const ALIGNED: () = assert!(align_of::<T::Head>() <= align_of::<B::Elem>(),
                                "a DstRoom's storage, less aligned than what it holds");

    fn check(&self) -> *const T::Head {
        let () = Self::ALIGNED;
        let cap = self.capacity();
        assert!(T::MIN_BYTES <= cap, "a room of {cap} bytes, short of the {} a {} is at least",
                T::MIN_BYTES, core::any::type_name::<T>());

        let head = self.buf.elems().as_ptr().cast::<T::Head>();
        let bytes = unsafe { (*head).bytes() };
        assert!(bytes <= cap, "a value of {bytes} bytes in a room of {cap}");
        head
    }
}

impl<T: ?Sized + Unsized, B: ?Sized + Storage> Deref for DstRoom<T, B>
where
    T::Head: Plain + FakeDst,
{
    type Target = T;

    fn deref(&self) -> &T {
        let head = self.check();
        // SAFETY: check()'s: a T, as long as its head says, within the room
        unsafe { &*T::from_raw(head.cast(), (*head).bytes()) }
    }
}

impl<T: ?Sized + Unsized, B: ?Sized + Storage> DerefMut for DstRoom<T, B>
where
    T::Head: Plain + FakeDst,
{
    fn deref_mut(&mut self) -> &mut T {
        let head = self.check().cast_mut();
        // SAFETY: as deref()'s, the room ours
        unsafe { &mut *T::from_raw_mut(head.cast(), (*head).bytes()) }
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
