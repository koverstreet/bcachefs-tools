// SPDX-License-Identifier: GPL-2.0

//! DstRoom<T, B>: one T - SelfSized, its head saying how long it is - at the
//! front of its storage B, the rest room for it to grow into: ArrayVec's, for
//! a single value. B is [E; N], its room a type parameter, or [E], a value's:
//! one struct, either dereferencing to its T, and the first coercing to the
//! second. Or B is an InlineOrBoxed - InlineVec's, for a single value: its
//! room in itself, and once that's outgrown, a boxed slice. Or an
//! AlignedBox<[E], A>, for storage IO needs aligned further. Or a Box of any
//! storage - of MappedPages, a bio's own pages, as words.
//!
//! The storage is its own element type, not T's: a T's tail may be bytes
//! under a header of words, as a typed key's value is under its bkey.

use crate::util::alloc::{flags, AlignedBox, Alloc, AllocError, AllocGfp, Box, Flags, Global};
use crate::util::vstructs::{plain_bytes, plain_bytes_mut, DstHead, Plain, SelfSized};
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

impl<E: Plain, const A: usize> Storage for AlignedBox<[E], A> {
    type Elem = E;

    fn elems(&self) -> &[E] {
        self
    }

    fn elems_mut(&mut self) -> &mut [E] {
        self
    }
}

impl<T: ?Sized, E: Plain + Copy + ::pin_init::Zeroable, const A: usize> DstRoom<T, AlignedBox<[E], A>> {
    /// Its room made @n elements, grown or shrunk: as much of its storage as
    /// fits copied, zeroes past it. The storage, not the T: a T read in parts
    /// - a superblock's header, before the rest - can say it's longer than
    /// its room, which is why that's grown.
    pub fn realloc(&mut self, n: usize, gfp: Flags) -> Result<(), AllocError> {
        let mut new = AlignedBox::<[E], A>::try_new_zeroed_slice_gfp(n, gfp)?;
        let keep = self.buf.len().min(n);
        new[..keep].copy_from_slice(&self.buf[..keep]);
        self.buf = new;
        Ok(())
    }
}

impl<S: ?Sized + Storage, A: Alloc> Storage for Box<S, A> {
    type Elem = S::Elem;

    fn elems(&self) -> &[S::Elem] {
        (**self).elems()
    }

    fn elems_mut(&mut self) -> &mut [S::Elem] {
        (**self).elems_mut()
    }

    fn grow(&mut self, n: usize) -> &mut [S::Elem] {
        (**self).grow(n)
    }
}

/// @N elements in itself, and once they're outgrown, a boxed slice of at
/// least @B - grown __GFP_NOFAIL, doubling past that, so a value growing a
/// word at a time isn't a copy a word. Once boxed it stays boxed. Laid out as
/// a C tagged union, Inline first: zeroed, it's Inline, all zeroes.
#[repr(C, u8)]
pub enum InlineOrBoxed<E, const N: usize, const B: usize> {
    Inline([E; N]),
    Boxed(Box<[E]>),
}

// zeroed, the tag is Inline's, and its elements zeroes
unsafe impl<E: ::pin_init::Zeroable, const N: usize, const B: usize> ::pin_init::Zeroable
    for InlineOrBoxed<E, N, B> {}

impl<E: Plain + Copy + ::pin_init::Zeroable, const N: usize, const B: usize> Storage for InlineOrBoxed<E, N, B> {
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

impl<E: Plain + Copy + ::pin_init::Zeroable, const N: usize, const B: usize> InlineOrBoxed<E, N, B> {
    #[cold]
    #[inline(never)]
    fn grow_slow(&mut self, n: usize) -> &mut [E] {
        let have = self.elems().len();
        if n > have {
            let mut b = Box::try_zeroed_slice_in(n.max(B).max(have * 2), Global.with_flags(flags::NOFAIL))
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

/// T's empty value - its head's Default - and the rest room for it to grow
/// into.
impl<T: ?Sized + SelfSized + DstHead, E: Plain, const N: usize> Default for DstRoom<T, [E; N]>
where
    T::Head: Default + Plain,
{
    fn default() -> Self {
        let mut r: Self = DstRoom { _t: PhantomData, buf: unsafe { core::mem::zeroed() } };
        r.set_head(T::Head::default());
        r
    }
}

impl<T: ?Sized + SelfSized + DstHead, B: Storage + ::pin_init::Zeroable> DstRoom<T, B>
where
    T::Head: Default + Plain,
{
    /// Default's, made in place: for a room too big for the stack.
    pub fn init_default() -> impl ::pin_init::Init<Self> {
        use ::pin_init::Init as _;
        ::pin_init::init!(Self { _t: PhantomData, buf <- ::pin_init::init_zeroed() }).chain(|r: &mut Self| {
            r.set_head(T::Head::default());
            Ok(())
        })
    }
}

/// As the array's: T's empty value in its @N inline elements.
impl<T: ?Sized + SelfSized + DstHead, E: Plain + Copy + ::pin_init::Zeroable, const N: usize, const B: usize> Default
    for DstRoom<T, InlineOrBoxed<E, N, B>>
where
    T::Head: Default + Plain,
{
    fn default() -> Self {
        let mut r: Self = DstRoom { _t: PhantomData, buf: InlineOrBoxed::Inline(::pin_init::zeroed()) };
        r.set_head(T::Head::default());
        r
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
    /// unpacking one.
    pub fn elems(&self) -> &[B::Elem] {
        self.buf.elems()
    }

    pub fn elems_mut(&mut self) -> &mut [B::Elem] {
        self.buf.elems_mut()
    }

    /// What it's stored in: for what it does besides hold the T - its IO.
    pub fn storage(&self) -> &B {
        &self.buf
    }
}

impl<T: ?Sized + SelfSized, B: ?Sized + Storage> DstRoom<T, B> {
    const ALIGNED: () = assert!(T::ALIGN <= align_of::<B::Elem>(),
                                "a DstRoom's storage, less aligned than what it holds");

    /// @h written at the front of its storage, the T's head: what it says
    /// the T is.
    fn set_head(&mut self, h: T::Head)
    where
        T: DstHead,
        T::Head: Plain,
    {
        plain_bytes_mut(self.buf.elems_mut())[..size_of::<T::Head>()].copy_from_slice(plain_bytes(&h));
    }
}

impl<T: ?Sized + SelfSized, B: ?Sized + Storage> Deref for DstRoom<T, B> {
    type Target = T;

    fn deref(&self) -> &T {
        let () = Self::ALIGNED;
        unsafe { &*T::from_thin(self.buf.elems().as_ptr().cast()) }
    }
}

impl<T: ?Sized + SelfSized, B: ?Sized + Storage> DerefMut for DstRoom<T, B> {
    fn deref_mut(&mut self) -> &mut T {
        let () = Self::ALIGNED;
        unsafe { &mut *T::from_thin(self.buf.elems_mut().as_mut_ptr().cast()).cast_mut() }
    }
}

impl<T: ?Sized, E: Plain + Copy, const N: usize> Clone for DstRoom<T, [E; N]> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: ?Sized, E: Plain + Copy, const N: usize> Copy for DstRoom<T, [E; N]> {}

// #[repr(transparent)] over B: zeroed, it's zeroed storage, which holds a T
// if zeroes are one
unsafe impl<T: ?Sized + ::pin_init::Zeroable, B: ::pin_init::Zeroable> ::pin_init::Zeroable for DstRoom<T, B> {}

// #[repr(transparent)] over B: None is its None
unsafe impl<T: ?Sized, B: ::pin_init::ZeroableOption> ::pin_init::ZeroableOption for DstRoom<T, B> {}

impl<T: ?Sized, B: Storage> DstRoom<T, B> {
    /// @buf as a room, holding what it holds.
    pub fn from_storage(buf: B) -> Self {
        DstRoom { _t: PhantomData, buf }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// realloc() keeps the storage, not the T: what a T read in parts has
    /// so far - its header, saying it's longer than its room - is kept, and
    /// never made a T past its room.
    #[test]
    fn realloc_keeps_storage() {
        let mut b = AlignedBox::<[u64], 8>::try_new_zeroed_slice_gfp(2, Flags::empty()).unwrap();
        b.copy_from_slice(&[u64::MAX, 7]);
        let mut r = DstRoom::<crate::c::bch_sb, _>::from_storage(b);

        r.realloc(4, Flags::empty()).unwrap();
        assert_eq!(r.elems(), [u64::MAX, 7, 0, 0]);
        r.realloc(1, Flags::empty()).unwrap();
        assert_eq!(r.elems(), [u64::MAX]);
    }
}
