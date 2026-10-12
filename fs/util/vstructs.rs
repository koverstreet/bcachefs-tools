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
//! read past the end - and a type's statement of its layout, SelfSized or
//! FlexArray, is a plain trait: nothing's soundness rests on it being right.
//! What does need trusting is Plain: that any bytes are a valid value - which
//! #[derive(Plain)] (cstruct-macros) checks at compile time, zerocopy's
//! FromBytes for the types we define.
//!
//! What a variable-size type is, from the most general:
//!  - SelfSized, bytes(): its own bytes say how big it is - a #[counted_by]
//!    array's FlexArray is. Not extent entries, sized by the superblock's
//!    extent_type_u64s.
//!  - VStruct: SelfSized by a count of u64s, vstructs.h's - 8 byte aligned.
//!    vstruct!().
//!  - VStructContainer: a VStruct of entries, one after another from its
//!    start[]: vstruct_for_each(). vstruct_container!().
//!
//! vstructs.h, and where it goes:
//!  - vstruct_bytes(), replicas_entry_bytes(): SelfSized::bytes()
//!  - a counted array - devs[]: FlexArray, Flex::tail()
//!  - vstruct_for_each(), vstruct_next(), vstruct_last():
//!    VStructContainer::entries(), trusting the count, as C does
//!
//! And a value shorter than its type - written by an older version, before
//! the type grew - is a View: its fields read as zero past its end, as
//! bch2_sb_field_get_minsize() and bkey_val_copy_pad() zero them in C.

use core::marker::PhantomData;
use core::mem::{align_of, size_of, MaybeUninit};
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
plain!(u8, u16, u32, u64, u128, i8, i16, i32, i64, i128);
unsafe impl<T: Plain, const N: usize> Plain for [T; N] {}

// zerocopy's byte order integers: an integer's bytes, any of which are one
macro_rules! plain_byteorder {
    ($($t:ident),*) => { $(unsafe impl<O: zerocopy::byteorder::ByteOrder> Plain for zerocopy::byteorder::$t<O> {})* };
}
plain_byteorder!(U16, U32, U64, U128, I16, I32, I64, I128);

// SAFETY: 16 bytes - the shim's own type, the kernel's uuid_t - sized here to
// check there's nothing else
unsafe impl Plain for c::__uuid_t {}
const _: () = assert!(size_of::<c::__uuid_t>() == 16);

/// Nothing, at compile time, if @T is Plain: what #[derive(Plain)] and
/// #[bitfield] check a type's fields with (cstruct-macros' vstructs.rs).
pub const fn assert_plain<T: Plain>() {}

/// What a value reads as: a little-endian integer's integer, an array's
/// elements' - itself, for anything else. The one conversion a type has,
/// where From and Into allow any number, so generic code can make it without
/// being told the type: a View's get() returns it and set() takes it, and a
/// field reads as a u32, not a Le32 to get() again.
pub trait Native: Sized {
    type Native;
    fn to_native(self) -> Self::Native;
    fn from_native(n: Self::Native) -> Self;
}

macro_rules! native_self {
    ($($t:ty),*) => { $(impl Native for $t {
        type Native = Self;
        fn to_native(self) -> Self { self }
        fn from_native(n: Self) -> Self { n }
    })* };
}
native_self!(u8, u16, u32, u64, u128, i8, i16, i32, i64, i128, c::__uuid_t);

impl<T: Native, const N: usize> Native for [T; N] {
    type Native = [T::Native; N];
    fn to_native(self) -> Self::Native { self.map(T::to_native) }
    fn from_native(n: Self::Native) -> Self { n.map(T::from_native) }
}

macro_rules! native_byteorder {
    ($($t:ident: $n:ty),*) => { $(
        impl<O: zerocopy::byteorder::ByteOrder> Native for zerocopy::byteorder::$t<O> {
            type Native = $n;
            fn to_native(self) -> $n { self.get() }
            fn from_native(n: $n) -> Self { Self::new(n) }
        }
    )* };
}
native_byteorder!(U16: u16, U32: u32, U64: u64, U128: u128, I16: i16, I32: i32, I64: i64, I128: i128);

/// A type whose values run past size_of::<Self>(), as far as their own fields
/// say.
pub trait SelfSized {
    /// The bytes this value spans, its header's included: vstruct_bytes(),
    /// replicas_entry_bytes().
    fn bytes(&self) -> usize;
}

/// vstructs.h's: SelfSized by a count of u64s.
pub trait VStruct: SelfSized {
    /// Set the count; panics if its field can't hold @u64s.
    fn set_val_u64s(&mut self, u64s: usize);

    /// Its 512 byte sectors, whole blocks of 512 << @block_bits:
    /// vstruct_sectors().
    fn sectors(&self, block_bits: u32) -> usize {
        self.bytes().next_multiple_of(512 << block_bits) >> 9
    }
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

impl<T: FlexArray> SelfSized for T {
    #[inline]
    fn bytes(&self) -> usize {
        T::TAIL + self.nr() * size_of::<T::Elem>()
    }
}

/// A count a header's field holds - u64s, nr_devs - as a usize: a plain
/// integer, or one of C's little-endian ones.
pub trait Count: Sized {
    fn count(&self) -> usize;
    /// @n as this count - None if it doesn't fit.
    fn from_count(n: usize) -> Option<Self>;
}

macro_rules! count {
    ($($t:ty),*) => { $(impl Count for $t {
        fn count(&self) -> usize { *self as usize }
        fn from_count(n: usize) -> Option<Self> { n.try_into().ok() }
    })* };
}
count!(u8, u16, u32, u64);

macro_rules! count_le {
    ($($t:ident),*) => { $(impl Count for crate::types::$t {
        fn count(&self) -> usize { self.get() as usize }
        fn from_count(n: usize) -> Option<Self> { Some(Self::new(n.try_into().ok()?)) }
    })* };
}
count_le!(Le16, Le32, Le64);

/// A vstruct of entries, one after another from its start[] to its end, each
/// as long as it says.
pub trait VStructContainer: VStruct {
    type Entry: SelfSized + Plain;
    /// offsetof(start)
    const START: usize;

    /// Its entries: vstruct_for_each(). Trusting its count, as C does a
    /// validated vstruct's; an entry that doesn't fit, or says it's shorter
    /// than its own header - which would never end the walk - ends it,
    /// rather than being read.
    ///
    /// XXX: technically undefined behaviour until these are proper DSTs: the
    /// entries are read through a pointer from &self, which covers only the
    /// header.
    fn entries(&self) -> Entries<'_, Self::Entry> {
        // SAFETY: its count is right - validated - so its bytes follow it;
        // but see above
        let bytes = unsafe { core::slice::from_raw_parts((self as *const Self).cast::<u8>(), self.bytes()) };
        Entries { rest: bytes.get(Self::START..).unwrap_or_default(), _entry: core::marker::PhantomData }
    }

    /// entries(), mutably.
    fn entries_mut(&mut self) -> EntriesMut<'_, Self::Entry> {
        let len = self.bytes();
        // SAFETY: as entries()
        let bytes = unsafe { core::slice::from_raw_parts_mut((self as *mut Self).cast::<u8>(), len) };
        EntriesMut { rest: bytes.get_mut(Self::START..).unwrap_or_default(), _entry: core::marker::PhantomData }
    }
}

/// vstruct!(T, u64s): T is a vstruct, @u64s its count of the u64s past its
/// header; vstruct!(T, hdr.u64s), past the header @hdr it embeds first;
/// `start = 0` where the count includes the header, as a superblock
/// field's does.
macro_rules! vstruct {
    ($t:ty, $($count:ident).+, start = $start:expr) => {
        $crate::util::vstructs::vstruct!(@impl $t, $start, $($count).+);
    };
    ($t:ty, $count:ident) => {
        $crate::util::vstructs::vstruct!(@impl $t, ::core::mem::size_of::<$t>(), $count);
    };
    ($t:ty, $hdr:ident . $count:ident) => {
        $crate::util::vstructs::vstruct!(@impl $t,
            ::core::mem::offset_of!($t, $hdr) + $crate::types::field_size(
                |p: *const $t| unsafe { ::core::ptr::addr_of!((*p).$hdr) }) as usize,
            $hdr.$count);
    };
    (@impl $t:ty, $start:expr, $($count:ident).+) => {
        const _: () = assert!(($start) % 8 == 0, concat!(stringify!($t), ": a vstruct's u64s start u64 aligned"));

        impl $crate::util::vstructs::SelfSized for $t {
            #[inline]
            fn bytes(&self) -> usize {
                // the count by value: a packed struct's field can't be borrowed
                ($start) + $crate::util::vstructs::Count::count(&{ self.$($count).+ }) * 8
            }
        }

        impl $crate::util::vstructs::VStruct for $t {
            fn set_val_u64s(&mut self, u64s: usize) {
                self.$($count).+ = $crate::util::vstructs::Count::from_count(u64s)
                    .unwrap_or_else(|| panic!(concat!(stringify!($t), ": {} u64s don't fit in its ",
                                                      stringify!($($count).+)), u64s));
            }
        }
    };
}
pub(crate) use vstruct;

/// vstruct_container!(T, E): vstruct T's entries are Es, from its start[].
macro_rules! vstruct_container {
    ($t:ty, $entry:ty) => {
        impl $crate::util::vstructs::VStructContainer for $t {
            type Entry = $entry;
            const START: usize = ::core::mem::offset_of!($t, start);
        }
    };
}
pub(crate) use vstruct_container;

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

/// The bytes of the entry at the start of @rest, if a whole one is there:
/// what a walk steps by.
fn entry_bytes<E: SelfSized + Plain>(rest: &[u8]) -> Option<usize> {
    let len = Flex::<E>::new(rest).ok()?.bytes();
    (len >= size_of::<E>() && len <= rest.len()).then_some(len)
}

/// A vstruct's entries: VStructContainer::entries().
pub struct Entries<'a, E> {
    rest:   &'a [u8],
    _entry: core::marker::PhantomData<&'a E>,
}

impl<'a, E: SelfSized + Plain + 'a> Iterator for Entries<'a, E> {
    type Item = &'a E;

    fn next(&mut self) -> Option<Self::Item> {
        let Some(len) = entry_bytes::<E>(self.rest) else {
            self.rest = &[];
            return None;
        };
        let (e, rest) = self.rest.split_at(len);
        self.rest = rest;
        // SAFETY: entry_bytes() checked an E is there, aligned, and it's Plain
        Some(unsafe { &*e.as_ptr().cast::<E>() })
    }
}

/// A vstruct's entries, mutably: VStructContainer::entries_mut().
pub struct EntriesMut<'a, E> {
    rest:   &'a mut [u8],
    _entry: core::marker::PhantomData<&'a mut E>,
}

impl<'a, E: SelfSized + Plain + 'a> Iterator for EntriesMut<'a, E> {
    type Item = &'a mut E;

    fn next(&mut self) -> Option<Self::Item> {
        let Some(len) = entry_bytes::<E>(self.rest) else {
            self.rest = Default::default();
            return None;
        };
        let (e, rest) = core::mem::take(&mut self.rest).split_at_mut(len);
        self.rest = rest;
        // SAFETY: as Entries', and the entries don't overlap
        Some(unsafe { &mut *e.as_mut_ptr().cast::<E>() })
    }
}

impl<'a, T: SelfSized> Flex<'a, T> {
    /// Its bytes, header and tail - what C copies with vstruct_bytes() - if
    /// what holds it holds them.
    pub fn as_bytes(&self) -> Result<&'a [u8], FlexError> {
        let want = self.head.bytes();
        self.bytes.get(..want).ok_or_else(|| FlexError::overrun::<T>(want, self.bytes.len()))
    }
}

// ---- Views: values shorter than their type ---------------------------------

/// ViewMut::set() on a field past the end of a view's bytes: one a shorter,
/// older version of the type it's in doesn't have. Growing the value is its
/// owner's job, not a field write's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShortField {
    /// The type the bytes are a value of.
    pub ty:    &'static str,
    /// The field, if one was projected to - and where it is in @ty.
    pub field: &'static str,
    pub at:    usize,
    pub size:  usize,
    /// How many of @ty's bytes there are.
    pub len:   usize,
}

impl core::fmt::Display for ShortField {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let Self { ty, field, at, size, len } = *self;
        let field = if field.is_empty() { "bytes" } else { field };
        write!(f, "{ty}: {field} at {at}..{} is past its end ({len} bytes); grow it first", at + size)
    }
}

#[cfg(feature = "std")]
impl std::error::Error for ShortField {}

/// A T, and as many of its bytes as there are - fewer, for one written before
/// T grew: a superblock section, journal entry or key value from an older
/// version. One field of a View is a View of that field: #[derive(View)]'s
/// projections.
///
/// Read, a field past the end is zero - an older version's missing fields
/// are, as C's get_minsize() and bkey_val_copy_pad() zero them - and nothing
/// past the end is ever read: those bytes are whatever comes next.
///
/// That a short value is an older version, not a truncated one, rests on its
/// count having been validated: the bytes a View is made over are as many as
/// its header says. Not for untrusted bytes - dump's, off a damaged disk.
///
/// A Floored type's fields that every value has are fields of its View
/// already: s.subvol, not s.subvol().
///
/// Field projections will replace this when the language has them:
/// e.btrees_lost_data() becomes e.btrees_lost_data.
pub struct View<'a, T> {
    /// T's first byte: the value's, @at further on - and maybe past its end
    p:   *const u8,
    at:  usize,
    /// How many of the value's bytes there are
    end: usize,
    _t:  PhantomData<&'a T>,
}

impl<T> Clone for View<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for View<'_, T> {}

impl<'a, T: Plain> View<'a, T> {
    /// The T at the start of @bytes, as much of it as they hold.
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { p: bytes.as_ptr(), at: 0, end: bytes.len(), _t: PhantomData }
    }

    /// How many of its bytes there are, up to its size.
    fn len(&self) -> usize {
        self.end.saturating_sub(self.at).min(size_of::<T>())
    }

    /// The U at @off in it, its field @_field - named as ViewMut's are, for
    /// set()'s errors: what #[derive(View)]'s projections return. A wrong @off
    /// reads the wrong bytes, never bytes that aren't there.
    pub fn project<U: Plain>(&self, off: usize, _field: &'static str) -> View<'a, U> {
        View { p: self.p.wrapping_add(off), at: self.at.saturating_add(off), end: self.end, _t: PhantomData }
    }

    /// Its value, as T: what's past the end reads as zero.
    fn read(&self) -> T {
        let mut v = MaybeUninit::<T>::zeroed();
        // SAFETY: len() of its bytes are there, into a zeroed T - and any
        // bytes are a T
        unsafe {
            core::ptr::copy_nonoverlapping(self.p, v.as_mut_ptr().cast::<u8>(), self.len());
            v.assume_init()
        }
    }

    /// Its bytes and the value's past them - a flexible array's, a message's
    /// - to the value's end: Flex::as_bytes().
    pub fn as_bytes(&self) -> &'a [u8] {
        // SAFETY: the value's bytes, from ours to its end - none if we're past it
        unsafe { core::slice::from_raw_parts(self.p, self.end.saturating_sub(self.at)) }
    }

    /// It, whole: None if any of it is past the end, or it isn't aligned.
    pub fn whole(&self) -> Option<&'a T> {
        (self.len() == size_of::<T>() && self.p as usize % align_of::<T>() == 0)
            // SAFETY: all of it is there, aligned - and any bytes are a T
            .then(|| unsafe { &*self.p.cast::<T>() })
    }
}

impl<T: Plain + Native> View<'_, T> {
    /// Its value - the integer, for a little-endian one: what's past the end
    /// reads as zero.
    pub fn get(&self) -> T::Native {
        self.read().to_native()
    }
}

/// A View that can set fields - those there are: one past the end is an
/// error, not a write.
pub struct ViewMut<'a, T> {
    p:     *mut u8,
    at:    usize,
    end:   usize,
    ty:    &'static str,
    field: &'static str,
    _t:    PhantomData<&'a mut T>,
}

impl<'a, T: Plain> ViewMut<'a, T> {
    /// The T at the start of @bytes, as much of it as they hold.
    pub fn new(bytes: &'a mut [u8]) -> Self {
        Self { p: bytes.as_mut_ptr(), at: 0, end: bytes.len(), ty: core::any::type_name::<T>(),
               field: "", _t: PhantomData }
    }

    /// It, read-only.
    pub fn as_view(&self) -> View<'_, T> {
        View { p: self.p, at: self.at, end: self.end, _t: PhantomData }
    }

    /// As View::project().
    pub fn project<U: Plain>(&mut self, off: usize, field: &'static str) -> ViewMut<'_, U> {
        ViewMut { p: self.p.wrapping_add(off), at: self.at.saturating_add(off), end: self.end, ty: self.ty,
                  field, _t: PhantomData }
    }

    /// As View::whole(), mutably.
    pub fn whole_mut(self) -> Option<&'a mut T> {
        (self.as_view().len() == size_of::<T>() && self.p as usize % align_of::<T>() == 0)
            // SAFETY: as View::whole()
            .then(|| unsafe { &mut *self.p.cast::<T>() })
    }
}

impl<T: Plain + Native> ViewMut<'_, T> {
    /// As View::get().
    pub fn get(&self) -> T::Native {
        self.as_view().get()
    }

    /// Set it to @v - the integer, for a little-endian one - if all of it is
    /// there.
    pub fn set(&mut self, v: T::Native) -> Result<(), ShortField> {
        if self.as_view().len() < size_of::<T>() {
            return Err(ShortField { ty: self.ty, field: self.field, at: self.at,
                                    size: size_of::<T>(), len: self.end });
        }
        let v = T::from_native(v);
        // SAFETY: all of it is there; @v is Plain, so all its bytes are
        // initialized - and the value they land in is Plain, as new() required
        unsafe { core::ptr::copy_nonoverlapping((&raw const v).cast::<u8>(), self.p, size_of::<T>()) };
        Ok(())
    }
}

/// A type whose values may be shorter than it, but never shorter than Min:
/// the fields every valid one has. A View of it derefs to them, so they're
/// fields - v.subvol - and only the later ones are projected, reading as
/// zero past the end. A key type's value is one: Min is it up to the field
/// BCH_BKEY_TYPES() names, from #[derive(View)]'s __<name>_min!(), or all of
/// it for `_`.
///
/// # Safety
/// Min is Self's first fields, at the same offsets.
pub unsafe trait Floored {
    type Min;
}

impl<'a, T: Plain + Floored> View<'a, T>
where
    T::Min: Plain,
{
    /// Its fields every valid one has. A View of fewer bytes than those
    /// wasn't made by what floors it - BkeySC::view() - and is a bug.
    fn min(&self) -> &'a T::Min {
        let (ty, len) = (core::any::type_name::<T>(), self.len());
        assert!(len >= size_of::<T::Min>(),
                "{ty}: {len} bytes, fewer than every valid one has ({})", size_of::<T::Min>());
        assert!(self.p as usize % align_of::<T::Min>() == 0,
                "{ty} at {:p}: not aligned to {}", self.p, align_of::<T::Min>());
        // SAFETY: that many bytes are there, aligned - and any bytes are a Min
        unsafe { &*self.p.cast::<T::Min>() }
    }
}

impl<T: Plain + Floored> Deref for View<'_, T>
where
    T::Min: Plain,
{
    type Target = T::Min;

    fn deref(&self) -> &T::Min {
        self.min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Le16, Le32, Le64};
    use cstruct_macros::{Plain, View};

    /// @v's bytes, at @at in @buf.
    fn put<T: Plain>(buf: &mut [u64], at: usize, v: &T) {
        // SAFETY: Plain - every byte of it is a field's
        let b = unsafe { core::slice::from_raw_parts((v as *const T).cast::<u8>(), size_of::<T>()) };
        // SAFETY: u64s, which any bytes are
        let dst = unsafe { core::slice::from_raw_parts_mut(buf.as_mut_ptr().cast::<u8>(), buf.len() * 8) };
        dst[at..at + b.len()].copy_from_slice(b);
    }

    fn bytes(buf: &[u64]) -> &[u8] {
        // SAFETY: u64s, which any bytes are
        unsafe { core::slice::from_raw_parts(buf.as_ptr().cast(), buf.len() * 8) }
    }

    fn entry(u64s: u16) -> c::jset_entry {
        c::jset_entry { u64s: Le16::new(u64s), ..Default::default() }
    }

    /// A jset of two jset_entries, of 1 and 3 u64s past their headers: and,
    /// with @second's u64s for the second, the jset's u64s saying 1 + 1 + 1
    /// + 3 still.
    fn jset(second: u16) -> Vec<u64> {
        let hdr = size_of::<c::jset>();
        let mut buf = vec![0u64; hdr / 8 + 6];
        put(&mut buf, 0, &c::jset { u64s: Le32::new(6), ..Default::default() });
        put(&mut buf, hdr, &entry(1));
        put(&mut buf, hdr + 16, &entry(second));
        buf
    }

    #[test]
    fn entries_walk_variable_size_entries() {
        let buf = jset(3);
        let j = Flex::<c::jset>::new(bytes(&buf)).unwrap();
        let es: Vec<_> = j.entries().collect();
        assert_eq!(es.len(), 2);
        assert_eq!((es[0].u64s.get(), es[1].u64s.get()), (1, 3));
        assert_eq!(es[1].bytes(), 32);
    }

    /// An entry past its container's end ends the walk, unread.
    #[test]
    fn entries_stop_at_an_overrun() {
        let buf = jset(4);
        let j = Flex::<c::jset>::new(bytes(&buf)).unwrap();
        assert_eq!(j.entries().count(), 1);
    }

    /// Where a vstruct's u64s count from: past its header, from its start
    /// (start = 0), past the header it embeds.
    #[test]
    fn vstruct_bytes() {
        let mut e = entry(0);
        e.set_val_u64s(3);
        assert_eq!(e.bytes(), 8 + 24);

        let mut f: c::bch_sb_field = Default::default();
        f.set_val_u64s(3);
        assert_eq!(f.bytes(), 24);

        let mut l: c::jset_entry_log = Default::default();
        l.set_val_u64s(2);
        assert_eq!(l.bytes(), 8 + 16);

        let mut b: c::btree_node_entry = Default::default();
        b.set_val_u64s(3);
        assert_eq!(b.bytes(), 16 + 24 + 24);
    }

    /// A section counts from its start; its entries start at its start[].
    #[test]
    fn entries_start_at_start() {
        let hdr = size_of::<c::bch_sb_field_clean>();
        let mut buf = vec![0u64; hdr / 8 + 2];
        let mut clean: c::bch_sb_field_clean = Default::default();
        clean.set_val_u64s(hdr / 8 + 2);
        put(&mut buf, 0, &clean);
        put(&mut buf, hdr, &entry(1));
        let clean = Flex::<c::bch_sb_field_clean>::new(bytes(&buf)).unwrap();
        let es: Vec<_> = clean.entries().collect();
        assert_eq!(es.len(), 1);
        assert_eq!(es[0].u64s.get(), 1);
    }

    /// A superblock field of no u64s spans less than its own header: it
    /// would never end a walk, so it ends it.
    #[test]
    fn entries_end_at_a_zero_length_entry() {
        let hdr = size_of::<c::bch_sb>();
        let mut buf = vec![0u64; hdr / 8 + 1];
        let mut sb: c::bch_sb = Default::default();
        sb.u64s = Le32::new(1);
        put(&mut buf, 0, &sb);
        put(&mut buf, hdr, &c::bch_sb_field { u64s: Le32::new(0), ..Default::default() });
        let sb = Flex::<c::bch_sb>::new(bytes(&buf)).unwrap();
        assert_eq!(sb.entries().count(), 0);
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, PartialEq, Plain, View)]
    struct Hdr { u64s: Le32, ty: Le32 }

    /// A section that grew: 40 bytes now, 24 once - before @next and @last.
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, PartialEq, Plain, View)]
    struct Grown { hdr: Hdr, old: [Le64; 2], next: Le64, last: Le64, _tail: [u64; 0] }

    /// A Grown of @u64s: its header, then 2, 3, ... in each u64 after.
    fn grown(u64s: usize) -> Vec<u64> {
        let mut buf: Vec<u64> = (0..u64s as u64).map(|i| i + 1).collect();
        put(&mut buf, 0, &Hdr { u64s: Le32::new(u64s as u32), ty: Le32::new(7) });
        buf
    }

    fn bytes_mut(buf: &mut [u64]) -> &mut [u8] {
        // SAFETY: u64s, which any bytes are
        unsafe { core::slice::from_raw_parts_mut(buf.as_mut_ptr().cast(), buf.len() * 8) }
    }

    /// The fields an older, shorter version has read as they are; the ones
    /// it lacks, as zero.
    #[test]
    fn view_reads_zero_past_the_end() {
        let full = grown(5);
        let v = View::<Grown>::new(bytes(&full));
        assert_eq!((v.next().get(), v.last().get()), (4, 5));
        assert_eq!(v.whole(), Some(&v.get()));

        let short = grown(3);
        let v = View::<Grown>::new(bytes(&short));
        assert_eq!(v.hdr().ty().get(), 7);
        assert_eq!(v.old().get(), [2, 3]);
        assert_eq!((v.next().get(), v.last().get()), (0, 0));
        assert_eq!(v.get(), Grown { hdr: Hdr { u64s: Le32::new(3), ty: Le32::new(7) },
                                    old: [Le64::new(2), Le64::new(3)], ..Default::default() });
        assert_eq!(v.whole(), None);
        assert_eq!((v.as_bytes().len(), v.old().as_bytes().len(), v.last().as_bytes().len()),
                   (24, 16, 0));

        // half an array there: the half there is read, the rest is zero
        let half = grown(2);
        assert_eq!(View::<Grown>::new(bytes(&half)).old().get(), [2, 0]);

        // nothing there at all
        assert_eq!(View::<Grown>::new(&[]).hdr().ty().get(), 0);
    }

    /// A field the bytes have can be set; one past the end is an error, and
    /// writes nothing.
    #[test]
    fn view_set_past_the_end_errors() {
        let mut short = grown(3);
        let mut v = ViewMut::<Grown>::new(bytes_mut(&mut short));
        v.old().set([20, 30]).unwrap();
        assert_eq!(v.get().old, [Le64::new(20), Le64::new(30)]);

        let e = v.last().set(9).unwrap_err();
        assert_eq!((e.field, e.at, e.size, e.len), ("last", 32, 8, 24));
        assert!(e.to_string().ends_with("Grown: last at 32..40 is past its end (24 bytes); grow it first"),
                "{e}");
        assert!(v.whole_mut().is_none());
        assert_eq!(short, grown(3).iter().enumerate()
                   .map(|(i, &x)| match i { 1 => 20, 2 => 30, _ => x }).collect::<Vec<_>>());
    }
}
