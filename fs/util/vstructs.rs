// SPDX-License-Identifier: GPL-2.0
//! Variable-size structs: vstructs.h's, and C's flexible array members.
//!
//! A C struct that ends in a flexible array member - `u8 devs[]`, `u64
//! _data[]` - has values that run past sizeof, as far as their own fields
//! say: replicas_entry_bytes() from nr_devs, vstruct_bytes() from u64s. A
//! pointer to one says nothing about the bytes past it, so C trusts the
//! count, having validated it wherever the bytes came in.
//!
//! Here the bound travels with the reference. A Flex<'a, T> is a &T and the
//! bytes it may extend into: whatever holds it - a tagged_union!'s storage, a
//! key's value, a journal buffer. What its header claims is checked against
//! those bytes each time it's used, so a wrong count is an error, never a
//! read past the end - and a type's statement of its layout, VStruct or
//! FlexArray, is a plain trait: nothing's soundness rests on it being right.
//! What does need trusting is Plain: that any bytes are a valid value.
//! zerocopy's FromBytes is its general form, but bindgen's types can't derive
//! that.
//!
//! vstructs.h, and where it goes:
//!  - vstruct_bytes(), replicas_entry_bytes(): VStruct::bytes()
//!  - the array itself - _data[], devs[]: FlexArray, Flex::tail()
//!  - vstruct_for_each(), vstruct_next(), vstruct_last(): an iterator over a
//!    Flex's entries, each a Flex bounded by its parent's end; for now,
//!    vstruct_next_entry()
//!  - vstruct_idx(), and building in place - journal entries, bsets: a Flex
//!    over mutable bytes, with push() and retain()

use core::mem::{align_of, size_of};
use core::ops::Deref;

use crate::c;

/// Plain data: any bytes are a valid value, and every byte of one is part of
/// a field - so a value can be read from bytes, and its bytes read back.
///
/// # Safety
/// Every bit pattern of size_of::<Self>() bytes must be a valid Self, and
/// Self must have no padding.
pub unsafe trait Plain {}

macro_rules! plain {
    ($($t:ty),*) => { $(unsafe impl Plain for $t {})* };
}
plain!(u8, u16, u32, u64, i8, i16, i32, i64);
unsafe impl<T: Plain, const N: usize> Plain for [T; N] {}

/// A type whose values run past size_of::<Self>(), as far as their own fields
/// say.
pub trait VStruct {
    /// The bytes this value spans, its header's included: vstruct_bytes(),
    /// replicas_entry_bytes().
    fn bytes(&self) -> usize;
}

/// A type ending in a flexible array member, C's `T name[]`.
pub trait FlexArray {
    /// What the array holds.
    type Elem: Plain;
    /// Where it starts: offset_of!(), not size_of - a flexible array can
    /// start in the struct's trailing padding.
    const TAIL: usize;
    /// How many elements it holds, by the header's own fields.
    fn nr(&self) -> usize;
}

impl<T: FlexArray> VStruct for T {
    fn bytes(&self) -> usize {
        T::TAIL + self.nr() * size_of::<T::Elem>()
    }
}

/// A Flex's bytes don't hold what it says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlexError {
    /// @ty's header says it spans @want bytes; what holds it has @have.
    Overrun { ty: &'static str, want: usize, have: usize },
    /// The bytes aren't aligned for @ty, which needs @align.
    Misaligned { ty: &'static str, align: usize },
    /// @ty's header says @nr elements; @given were given.
    Count { ty: &'static str, nr: usize, given: usize },
}

impl core::fmt::Display for FlexError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            Self::Overrun { ty, want, have } =>
                write!(f, "{ty}: its header says {want} bytes, but only {have} hold it"),
            Self::Misaligned { ty, align } =>
                write!(f, "{ty}: bytes not aligned to {align}"),
            Self::Count { ty, nr, given } =>
                write!(f, "{ty}: its header says {nr} elements, but {given} were given"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for FlexError {}

impl FlexError {
    fn overrun<T>(want: usize, have: usize) -> Self {
        Self::Overrun { ty: core::any::type_name::<T>(), want, have }
    }
}

/// A T, and the bytes it may extend into: from its first byte to the end of
/// whatever holds it - the bound a &T doesn't carry.
pub struct Flex<'a, T> {
    head:  &'a T,
    bytes: &'a [u8],
}

impl<T> Clone for Flex<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for Flex<'_, T> {}

impl<T> Deref for Flex<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.head
    }
}

impl<'a, T> Flex<'a, T> {
    /// The T at the start of @bytes, which it may extend to their end.
    pub fn new(bytes: &'a [u8]) -> Result<Self, FlexError>
    where
        T: Plain,
    {
        if bytes.len() < size_of::<T>() {
            return Err(FlexError::overrun::<T>(size_of::<T>(), bytes.len()));
        }
        if bytes.as_ptr() as usize % align_of::<T>() != 0 {
            return Err(FlexError::Misaligned { ty: core::any::type_name::<T>(),
                                               align: align_of::<T>() });
        }
        // SAFETY: as checked above, and any bytes are a T
        Ok(unsafe { Self::new_unchecked(bytes) })
    }

    /// The T at the start of @bytes, which it may extend to their end.
    ///
    /// # Safety
    /// @bytes must start with a valid T, aligned for it.
    pub unsafe fn new_unchecked(bytes: &'a [u8]) -> Self {
        debug_assert!(bytes.len() >= size_of::<T>());
        // SAFETY: the caller's
        Self { head: unsafe { &*bytes.as_ptr().cast::<T>() }, bytes }
    }

    /// The T, for as long as the bytes are borrowed.
    pub fn head(&self) -> &'a T {
        self.head
    }
}

impl<'a, T: FlexArray> Flex<'a, T> {
    /// The flexible array: as many elements as the header says, if the bytes
    /// hold them.
    pub fn tail(&self) -> Result<&'a [T::Elem], FlexError> {
        let nr = self.head.nr();
        let want = nr.checked_mul(size_of::<T::Elem>())
            .and_then(|b| b.checked_add(T::TAIL))
            .unwrap_or(usize::MAX);
        if want > self.bytes.len() {
            return Err(FlexError::overrun::<T>(want, self.bytes.len()));
        }
        let p = self.bytes[T::TAIL..].as_ptr();
        if p as usize % align_of::<T::Elem>() != 0 {
            return Err(FlexError::Misaligned { ty: core::any::type_name::<T::Elem>(),
                                               align: align_of::<T::Elem>() });
        }
        // SAFETY: in bounds and aligned, as checked above, and any bytes are an Elem
        Ok(unsafe { core::slice::from_raw_parts(p.cast::<T::Elem>(), nr) })
    }
}

impl<'a, T: VStruct> Flex<'a, T> {
    /// Its bytes, header and tail - what C copies with vstruct_bytes() - if
    /// what holds it holds them.
    pub fn as_bytes(&self) -> Result<&'a [u8], FlexError> {
        let want = self.head.bytes();
        self.bytes.get(..want).ok_or_else(|| FlexError::overrun::<T>(want, self.bytes.len()))
    }
}

// vstruct_next(entry) = (u64*)entry._data + le16(entry.u64s)
pub(crate) unsafe fn vstruct_next_entry(entry: *const c::jset_entry) -> *const c::jset_entry {
    let u64s = u16::from_le((*entry).u64s) as usize;
    (entry as *const u8).add(8 + u64s * 8) as *const c::jset_entry
}
