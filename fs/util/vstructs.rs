// SPDX-License-Identifier: GPL-2.0
//! Variable-size structs: vstructs.h's, and C's flexible array members.
//!
//! A C struct ending in a flexible array member - `u8 devs[]`, `u64
//! _data[]` - runs past sizeof, as far as its own count says:
//! vstruct_bytes() from u64s, replicas_entry_bytes() from nr_devs. C trusts
//! the count, having validated it where the bytes came in.
//!
//! Here the bound travels with the reference. A reference claims its bytes,
//! so one is made at the value's own size, from a buffer or from a pointer
//! to its header: never by widening a &T, which reaches only the T, and never
//! from fewer bytes than the header.
//!
//! What a variable-size type can be, most general first:
//!  - FakeDst: sized, its own bytes say how far past them it runs - a DST
//!    typed as its header, to be converted. A #[counted_by] array's
//!    FlexArray is; extent entries aren't - the superblock sizes those.
//!  - VStruct: a FakeDst by a count of u64s, vstructs.h's, 8 byte aligned:
//!    vstruct!().
//!  - Unsized: a DST of ours, its reference fat - an unsized vstruct, a
//!    #[counted] array, a #[whole] or #[grown] value. SelfSized, if its head
//!    says how long it is, as an unsized vstruct's does.
//!  - A container: an unsized vstruct whose entries follow its start[]:
//!    vstruct_container!()'s entries().
//!
//! An unsized vstruct is a Family of definitions, one per State, written by
//! #[unsized_vstruct]; jset<S> picks one. Validated, the default, is what's
//! been read and validated (SelfSized::from_thin()): its counts are trusted.
//! Unvalidated is bytes from outside - off a device, from the kernel
//! (from_bytes()): its count is checked against them, its entries' as
//! they're walked. Each derefs to its header, Head<T>. The count
//! is private, read as u64s(): it changes only through what holds the value,
//! which has the room for it to grow.
//!
//! A key is thin - its size is its k.u64s, not its reference's - and walked
//! as a key, an AnyKey, in its container's state.
//!
//! A value shorter than its type, written before the type grew, can't be a
//! &T: it's a View, reading only its own bytes, the fields past its end zero;
//! or, for a #[grown] family, its DST - the fields every value has, then the
//! bytes there are.
//!
//! vstructs.h, and where it goes:
//!  - vstruct_bytes(), replicas_entry_bytes(): FakeDst::bytes(), SelfSized::bytes()
//!  - devs[]: #[counted]; FlexArray where it's read by stride
//!  - vstruct_for_each(), vstruct_next(), vstruct_last(): entries()
//!
//! What's trusted: Plain, that any bytes are a valid value - which
//! #[derive(Plain)] checks at compile time, and zerocopy's FromBytes for the
//! types we define - and, for a reference from a pointer to its header, the
//! count FakeDst::bytes() reads, which vstruct!() and #[counted_by] generate
//! from the field the struct declares.

use core::marker::PhantomData;
use core::mem::{align_of, size_of, size_of_val, MaybeUninit};
use core::ops::{Deref, DerefMut};

/// Every byte of every value initialized - no padding, and no variant that
/// leaves some unwritten, as a None in a niche can - so a value can be
/// loaded as an integer, as a Volatile's word is. Plain is; so are a bool,
/// a closed enum and a pointer, which not every bit pattern is a value of.
/// #[derive(NoUninit)] checks a struct's fields as #[derive(Plain)] does.
/// A marker, so its impls may overlap: every Plain is one, and so is an
/// array of any.
///
/// # Safety
/// Every byte of every value of Self is initialized.
#[marker]
pub unsafe trait NoUninit {}

unsafe impl<T: Plain + ?Sized> NoUninit for T {}

macro_rules! no_uninit {
    ($($t:ty),*) => { $(unsafe impl NoUninit for $t {})* };
}
no_uninit!(bool, usize, isize);
unsafe impl<T> NoUninit for *mut T {}
unsafe impl<T> NoUninit for *const T {}
unsafe impl<T: NoUninit, const N: usize> NoUninit for [T; N] {}
unsafe impl<T: NoUninit> NoUninit for [T] {}

/// Nothing, at compile time, if @T is NoUninit: what #[derive(NoUninit)]
/// checks a type's fields with.
pub const fn assert_no_uninit<T: NoUninit>() {}

/// Never all zeroes, and all of it says so: an Option of it is as long, None
/// its zeroes - so that's NoUninit too. NonNull, a reference and NonZero are;
/// #[derive(NonZeroWord)] checks a #[repr(transparent)] struct over one.
///
/// # Safety
/// All zeroes isn't a Self, and Option<Self> is laid out as Self with None
/// all zeroes.
pub unsafe trait NonZeroWord {}

unsafe impl<T> NonZeroWord for core::ptr::NonNull<T> {}
unsafe impl<T> NonZeroWord for &T {}

macro_rules! non_zero_word {
    ($($t:ty),*) => { $(unsafe impl NonZeroWord for core::num::NonZero<$t> {})* };
}
non_zero_word!(u8, u16, u32, u64, usize);

unsafe impl<T: NonZeroWord> NoUninit for Option<T> {}

/// Plain data: any bytes are a valid value, and every byte of one is part of
/// a field - so a value can be read from bytes, and its bytes read back.
///
/// # Safety
/// Every bit pattern of size_of::<Self>() bytes must be a valid Self, and
/// Self must have no padding.
pub unsafe trait Plain: NoUninit {}

macro_rules! plain {
    ($($t:ty),*) => { $(unsafe impl Plain for $t {})* };
}
plain!(u8, u16, u32, u64, u128, i8, i16, i32, i64, i128);
// no bytes
unsafe impl<T: ?Sized> Plain for PhantomData<T> {}
unsafe impl<T: Plain, const N: usize> Plain for [T; N] {}
unsafe impl<T: Plain> Plain for [T] {}

// zerocopy's byte order integers: an integer's bytes, any of which are one
macro_rules! plain_byteorder {
    ($($t:ident),*) => { $(unsafe impl<O: zerocopy::byteorder::ByteOrder> Plain for zerocopy::byteorder::$t<O> {})* };
}
plain_byteorder!(U16, U32, U64, U128, I16, I32, I64, I128);

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
native_self!(u8, u16, u32, u64, u128, i8, i16, i32, i64, i128);

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

/// A DST whose head records its length: so a thin pointer, the value's
/// address alone, reaches all of one (util/thin.rs's pointer types). Never a
/// sized type - a pointer to one is already thin, and one that runs past its
/// size_of is a FakeDst.
///
/// # Safety
/// from_thin() of the address of a valid Self is a pointer to all of it. What
/// it reads to say how long it is can't change through a &Self, nor a &mut
/// Self.
pub unsafe trait SelfSized {
    /// The value at @p - a thin pointer, its address - as long as it says.
    ///
    /// # Safety
    /// @p points to a valid Self.
    unsafe fn from_thin(p: *const ()) -> *const Self;

    /// The bytes this value spans: vstruct_bytes().
    fn bytes(&self) -> usize {
        size_of_val(self)
    }

    /// Its 512 byte sectors, whole blocks of 512 << @block_bits:
    /// vstruct_sectors().
    fn sectors(&self, block_bits: u32) -> usize {
        self.bytes().next_multiple_of(512 << block_bits) >> 9
    }
}

impl<T> !SelfSized for T {}

/// A sized type whose fields say how far past its size_of its value runs: a
/// DST typed as its header - a struct ending in a flexible array member, a
/// key - or a family's head, standing in for the DST it heads.
///
/// XXX: what we're converting away from. Each goes as it becomes a DST of its
/// own, SelfSized, or its family's length is read from the family; then this
/// does.
pub trait FakeDst {
    /// vstruct_bytes(), replicas_entry_bytes().
    fn bytes(&self) -> usize;

    /// As SelfSized::sectors().
    fn sectors(&self, block_bits: u32) -> usize {
        self.bytes().next_multiple_of(512 << block_bits) >> 9
    }
}

/// vstructs.h's: a FakeDst by a count of u64s.
pub trait VStruct: FakeDst {
    /// Set the count; panics if its field can't hold @u64s.
    fn set_val_u64s(&mut self, u64s: usize);
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

impl<T: FlexArray> FakeDst for T {
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

// zerocopy's, aligned to 1: a packed struct's
macro_rules! count_zerocopy_le {
    ($($t:ident),*) => { $(impl Count for zerocopy::byteorder::little_endian::$t {
        fn count(&self) -> usize { self.get() as usize }
        fn from_count(n: usize) -> Option<Self> { Some(Self::new(n.try_into().ok()?)) }
    })* };
}
count_zerocopy_le!(U16, U32, U64);

/// floored_by_self!(T, ..): each T is whole in every valid value of it - a
/// header, whose count includes itself - so a View of one derefs to all of
/// it: a container's, or an entry's.
macro_rules! floored_by_self {
    ($($t:ty),* $(,)?) => {
        // SAFETY: Min is Self
        $(unsafe impl $crate::util::projections::Floored for $t {
            type Min = Self;
        })*
    };
}
pub(crate) use floored_by_self;

/// vstruct!(T, u64s): T is a vstruct, @u64s its count of the u64s past its
/// header; vstruct!(T, hdr.u64s), past the header @hdr it embeds first;
/// `start = 0` where the count includes the header, as a superblock
/// field's does.
///
/// vstruct!(unsized T_head, ..), for an unsized vstruct - what
/// #[unsized_vstruct(u64s = ..)] makes, as a family's other attributes make
/// the sized form: the count is its header's, T_head's, as the sized form
/// has it - and each state's length is its reference's, size_of_val(). A
/// count of its own reads as u64s(), on the header, which each state derefs
/// to; it's no field to set but the container's, which has the room for it -
/// VStruct, on the header, builds one.
macro_rules! vstruct {
    (unsized $head:ident, $($args:tt)*) => {
        $crate::util::vstructs::vstruct!($head, $($args)*);
        $crate::util::vstructs::vstruct!(@count $head, $($args)*);

        // its length - a count of u64s from a u64 aligned start, as @impl
        // asserts - is a whole number of its alignment, its head's, if that's
        // no more than 8: so a reference to one, as long as size_of_val(),
        // claims none past it, whatever its tail holds
        const _: () = assert!(::core::mem::align_of::<$head>() <= 8,
            concat!(stringify!($head), ": counted in u64s, so aligned to no more than 8"));
    };
    (@count $head:ident, $count:ident $(, start = $start:expr)?) => {
        impl $head {
            /// Its count, as a usize - to read: it changes only through what
            /// holds the value, which has room for it to grow.
            pub fn $count(&self) -> usize {
                $crate::util::vstructs::Count::count(&{ self.$count })
            }
        }
    };
    // counted past the header it embeds - a btree node's bset, its keys: the
    // count is that header's, read as it
    (@count $head:ident, $($embedded:tt)*) => {};
    ($t:ty, $($count:ident).+, start = $start:expr) => {
        $crate::util::vstructs::vstruct!(@impl $t, $start, $($count).+);
    };
    ($t:ty, $count:ident) => {
        $crate::util::vstructs::vstruct!(@impl $t, ::core::mem::size_of::<$t>(), $count);
    };
    ($t:ty, $hdr:ident . $count:ident) => {
        $crate::util::vstructs::vstruct!(@impl $t,
            ::core::mem::offset_of!($t, $hdr) + $crate::types::field_size(|p: $t| p.$hdr) as usize,
            $hdr.$count);
    };
    (@impl $t:ty, $start:expr, $($count:ident).+) => {
        const _: () = assert!(($start) % 8 == 0, concat!(stringify!($t), ": a vstruct's u64s start u64 aligned"));

        impl $crate::util::vstructs::FakeDst for $t {
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

/// vstruct_container!(T, W, E): T's entries are unsized Es, from the start[]
/// of W, the struct as written - an unsized vstruct's header, or a #[whole]
/// type's struct, as long as what holds it says. T's entries() are E's in T's
/// state - a Validated one's trusted, as C trusts them; an Unvalidated one's
/// each checked against what's left, an error ending the walk. E<S> is the
/// entry in state S. With `thin E`, a Validated one's walk is bkey_next()'s,
/// each entry's length read from its header and on - keys.
macro_rules! vstruct_container {
    ($t:ident, $w:ident, $entry:ident) => {
        $crate::util::vstructs::vstruct_container!(@walks $t, $w, $entry, ValidatedEntries);
    };
    ($t:ident, $w:ident, thin $entry:ident) => {
        $crate::util::vstructs::vstruct_container!(@walks $t, $w, $entry, ViewEntries);
    };
    (@walks $t:ident, $w:ident, $entry:ident, $walk:ident) => {
        impl $t<$crate::util::vstructs::Validated> {
            pub fn entries(&self)
                -> $crate::util::vstructs::$walk<'_, $entry<$crate::util::vstructs::Validated>>
            {
                $crate::util::vstructs::$walk::of(self)
            }
        }

        impl $t<$crate::util::vstructs::Unvalidated> {
            pub fn entries(&self)
                -> $crate::util::vstructs::UnvalidatedEntries<'_, $entry<$crate::util::vstructs::Unvalidated>>
            {
                $crate::util::vstructs::UnvalidatedEntries::new(self.start_bytes())
            }

            pub fn entries_mut(&mut self)
                -> $crate::util::vstructs::UnvalidatedEntriesMut<'_, $entry<$crate::util::vstructs::Unvalidated>>
            {
                $crate::util::vstructs::UnvalidatedEntriesMut::new(self.start_bytes_mut())
            }
        }

        $crate::util::vstructs::vstruct_container!(@start $t, $w, $entry<$crate::util::vstructs::Validated>);
    };
    (@start $t:ident, $w:ident, $entry:ty) => {
        $crate::util::vstructs::vstruct_container!(@start_of $t<$crate::util::vstructs::Unvalidated>, $w);
        $crate::util::vstructs::vstruct_container!(@start_of $t<$crate::util::vstructs::Validated>, $w);

        unsafe impl $crate::util::vstructs::Container for $t<$crate::util::vstructs::Validated> {
            type Entry = $entry;

            fn entry_bytes(&self) -> &[u8] {
                self.start_bytes()
            }

            fn entry_bytes_mut(&mut self) -> &mut [u8] {
                self.start_bytes_mut()
            }
        }
    };
    (@start_of $s:ty, $w:ident) => {
        impl $s {
            /// Its entries' bytes: from its start[] to its end.
            fn start_bytes(&self) -> &[u8] {
                &self.as_bytes()[::core::mem::offset_of!($w, start)..]
            }

            fn start_bytes_mut(&mut self) -> &mut [u8] {
                &mut self.as_bytes_mut()[::core::mem::offset_of!($w, start)..]
            }
        }
    };
}
pub(crate) use vstruct_container;

// ---- VstructRef: bytes whose counts haven't been checked ---------------

/// Bytes that can't be the value read from them: they don't hold its header,
/// aligned, or as many as its count says. What from_bytes() and the checked
/// walks return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VStructError {
    /// They don't hold @ty's header, or what it says: @want bytes, @have.
    Overrun { ty: &'static str, want: usize, have: usize },
    /// They aren't aligned for @ty, which needs @align.
    Misaligned { ty: &'static str, align: usize },
    /// @ty's header says it's @len bytes, fewer than the header itself: a
    /// walk would never get past it.
    Short { ty: &'static str, len: usize },
    /// @ty's header says it's @len bytes, which no @ty is: not a whole number
    /// of its tail's elements, or of its alignment.
    Ragged { ty: &'static str, len: usize },
}

impl core::fmt::Display for VStructError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            Self::Overrun { ty, want, have } =>
                write!(f, "{ty}: {want} bytes, but only {have} hold it"),
            Self::Misaligned { ty, align } =>
                write!(f, "{ty}: bytes not aligned to {align}"),
            Self::Short { ty, len } =>
                write!(f, "{ty}: its header says {len} bytes, fewer than the header"),
            Self::Ragged { ty, len } =>
                write!(f, "{ty}: its header says {len} bytes, not a whole number of its elements or alignment"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for VStructError {}

/// How many of @bytes a T at their start says are its: an error if they
/// don't hold its header, aligned, or that many - or it says fewer than its
/// header.
fn checked_bytes<T: Plain + FakeDst>(bytes: &[u8]) -> Result<usize, VStructError> {
    let ty = core::any::type_name::<T>();
    let len = cast_plain::<T>(bytes)
        .ok_or_else(|| match bytes.len() < size_of::<T>() {
            true  => VStructError::Overrun { ty, want: size_of::<T>(), have: bytes.len() },
            false => VStructError::Misaligned { ty, align: align_of::<T>() },
        })?
        .bytes();
    if len < size_of::<T>() {
        return Err(VStructError::Short { ty, len });
    }
    if len > bytes.len() {
        return Err(VStructError::Overrun { ty, want: len, have: bytes.len() });
    }
    Ok(len)
}

/// The plain T at the start of @b, by value, at any alignment: an error if
/// @b is shorter than it.
pub fn read_plain<T: Plain>(b: &[u8]) -> Result<T, VStructError> {
    if b.len() < size_of::<T>() {
        return Err(VStructError::Overrun { ty: core::any::type_name::<T>(), want: size_of::<T>(), have: b.len() });
    }
    // SAFETY: a T's bytes are there, and any bytes are one
    Ok(unsafe { b.as_ptr().cast::<T>().read_unaligned() })
}

/// A thin entry - a T whose header says how long it is: a key - and its
/// bytes, in state S: its container's, walked out of one, or Unvalidated,
/// bytes from outside - off a device, from the kernel. Its header's count is
/// checked against them when it's made, an overrun an error; then it's those
/// bytes exactly, header and tail, and the header's fields are fields.
pub struct VstructRef<'a, T, S: State = Unvalidated> {
    bytes: &'a [u8],
    _t:    PhantomData<(&'a T, S)>,
}

impl<T, S: State> Clone for VstructRef<'_, T, S> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T, S: State> Copy for VstructRef<'_, T, S> {}

impl<T, S: State> Deref for VstructRef<'_, T, S> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: its maker checked the header is there, aligned, and T is Plain
        unsafe { &*self.bytes.as_ptr().cast::<T>() }
    }
}

impl<'a, T: Plain + FakeDst> VstructRef<'a, T> {
    /// The T at the start of @bytes from outside, as many of them as its
    /// header says.
    pub fn new(bytes: &'a [u8]) -> Result<Self, VStructError> {
        let len = checked_bytes::<T>(bytes)?;
        Ok(Self { bytes: &bytes[..len], _t: PhantomData })
    }
}

impl<'a, T, S: State> VstructRef<'a, T, S> {
    /// Its bytes, header and tail.
    pub fn as_bytes(&self) -> &'a [u8] {
        self.bytes
    }
}

impl<'a, T, S: State> InState for VstructRef<'a, T, S> {
    type In<U: State> = VstructRef<'a, T, U>;
    type State = S;
}

/// An unsized container's thin entries, keys - each an VstructRef of
/// its own bytes, checked against what's left: an Unvalidated
/// jset_entry's entries().
pub struct VstructRefEntries<'a, E> {
    /// None after an error
    rest:   Option<&'a [u8]>,
    at:     usize,
    _entry: PhantomData<&'a E>,
}

impl<'a, E> VstructRefEntries<'a, E> {
    /// The entries in @rest: an unsized container's, from its start[] - or
    /// an ioctl's reply, keys one after another.
    pub fn new(rest: &'a [u8]) -> Self {
        Self { rest: Some(rest), at: 0, _entry: PhantomData }
    }
}

impl<'a, E: Plain + FakeDst + 'a> Iterator for VstructRefEntries<'a, E> {
    type Item = Result<VstructRef<'a, E>, VStructError>;

    fn next(&mut self) -> Option<Self::Item> {
        let rest = self.rest.filter(|r| !r.is_empty())?;
        match VstructRef::<E>::new(rest) {
            Ok(e) => {
                self.rest = Some(&rest[e.bytes.len()..]);
                self.at += e.bytes.len();
                Some(Ok(e))
            }
            Err(err) => {
                self.rest = None;
                Some(Err(err))
            }
        }
    }
}

/// A VstructRef, mutably.
pub struct VstructMut<'a, T, S: State = Unvalidated> {
    bytes: &'a mut [u8],
    _t:    PhantomData<(&'a mut T, S)>,
}

impl<T, S: State> Deref for VstructMut<'_, T, S> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: as VstructRef's
        unsafe { &*self.bytes.as_ptr().cast::<T>() }
    }
}

impl<T, S: State> DerefMut for VstructMut<'_, T, S> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as deref()
        unsafe { &mut *self.bytes.as_mut_ptr().cast::<T>() }
    }
}

impl<'a, T: Plain + FakeDst> VstructMut<'a, T> {
    /// As VstructRef::new(), mutably.
    pub fn new(bytes: &'a mut [u8]) -> Result<Self, VStructError> {
        let len = checked_bytes::<T>(bytes)?;
        Ok(Self { bytes: &mut bytes[..len], _t: PhantomData })
    }
}

impl<T, S: State> VstructMut<'_, T, S> {
    /// A pointer to it that reaches all its bytes: one from the &T reaches
    /// only the header.
    pub fn as_mut_ptr(&mut self) -> *mut T {
        self.bytes.as_mut_ptr().cast()
    }
}

impl<'a, T, S: State> VstructMut<'a, T, S> {
    /// Its bytes, header and tail.
    pub fn into_bytes(self) -> &'a mut [u8] {
        self.bytes
    }
}

impl<'a, T, S: State> InState for VstructMut<'a, T, S> {
    type In<U: State> = VstructMut<'a, T, U>;
    type State = S;
}

/// A walk over entries that knows where it is: offset() is where the entry
/// next() returns starts, in bytes from where the walk did - as an entry's
/// own length is.
pub trait EntryWalk: Iterator + Sized {
    fn offset(&self) -> usize;

    /// Each entry with its offset().
    fn with_offsets(self) -> WithOffsets<Self> {
        WithOffsets(self)
    }
}

pub struct WithOffsets<I>(I);

impl<I: EntryWalk> Iterator for WithOffsets<I> {
    type Item = (usize, I::Item);

    fn next(&mut self) -> Option<Self::Item> {
        let at = self.0.offset();
        self.0.next().map(|e| (at, e))
    }
}

impl<'a, E: Plain + FakeDst + 'a> EntryWalk for VstructRefEntries<'a, E> {
    fn offset(&self) -> usize {
        self.at
    }
}

// ---- Unsized vstructs: a header and its tail, behind one fat reference -----

/// Bytes from outside - off a device, from the kernel: a state, an unsized
/// vstruct's S. Its count was checked against the bytes it was made from;
/// its entries' are checked as they're walked, each an Unvalidated one.
#[derive(Debug, Default, Clone, Copy)]
pub struct Unvalidated;

/// What C read and validated: a state, an unsized vstruct's S - the default.
/// Its counts, and its entries', are trusted, as C trusts them; its entries
/// are Validated ones.
#[derive(Debug, Default, Clone, Copy)]
pub struct Validated;

/// A type's definitions, one per state - written by macro, #[unsized_vstruct],
/// #[counted], #[grown], #[whole] - and X<S>, the type in state S.
///
/// An unsized type's states are laid out alike - its header, then its tail -
/// so X<S> is its definition in each: one DST, generic over S, which says only
/// what's been checked of it.
///
/// A sized type's differ in structure - Whole is the struct, every field
/// there; in place, a DST as long as the value - so each is its own
/// definition, and X<S> is a generic struct over the one for S:
///
///   pub struct AllocV4<S: State = Whole> {
///       _state:   PhantomData<S>,
///       pub repr: <S as State>::Of<AllocV4Family>,
///   }
///
/// #[repr(transparent)], and its last field the definition, a DST or not: a
/// reference to one is a reference to the other, metadata and all. Not an
/// alias - <S as State>::Of<..> is a projection, and an impl for a projection
/// is E0118 if inherent, and breaks other crates' impls of the same trait
/// (E0119) - so impls attach to X<Validated>, X<Whole>, as to any type, and
/// X is what code names in every state. It derefs to the definition; the
/// definitions, named - X_validated, X_whole - are what the macros build it
/// from.
///
/// Every definition is made from its bytes the same way, ArmRef, and none
/// borrows: so a container in state S can name a member as <S as
/// State>::Of<its family> and hand it out in S - tagged_union!'s typed(),
/// for any S. The state propagates to every member.
pub trait Family {
    type Unvalidated: ?Sized + ArmRef + 'static;
    type Validated: ?Sized + ArmRef + 'static;
    /// Validated and extended to the current type: every field there, so
    /// every field a field. An unsized vstruct's is its Validated - its
    /// header never grew.
    type Whole: ?Sized + ArmRef + 'static;
}

/// A state: which of a Family's definitions X<S> is. 'static, as each
/// definition is: X<S> holds only the state's name.
pub trait State: 'static {
    type Of<F: Family>: ?Sized + ArmRef + 'static;

    /// A sized type T, whose definition is the same in every state, in this
    /// one: T, or for Unvalidated a type of its own - so that a value from
    /// outside is still one where it's been read as a T.
    type OfSized<T: 'static>: 'static;
}

impl State for Unvalidated {
    type Of<F: Family> = F::Unvalidated;
    type OfSized<T: 'static> = UnvalidatedSized<T>;
}

impl State for Validated {
    type Of<F: Family> = F::Validated;
    type OfSized<T: 'static> = T;
}

/// A value in place, Validated or Unvalidated: the states a sized family
/// defines alike, X_validated, so X<S> derefs to it in either - for code
/// generic over the state.
pub trait InPlace: State {
    fn def<D: ?Sized, F: Family<Unvalidated = D, Validated = D>>(r: &Self::Of<F>) -> &D;
    fn def_mut<D: ?Sized, F: Family<Unvalidated = D, Validated = D>>(r: &mut Self::Of<F>) -> &mut D;
}

impl InPlace for Unvalidated {
    fn def<D: ?Sized, F: Family<Unvalidated = D, Validated = D>>(r: &Self::Of<F>) -> &D { r }
    fn def_mut<D: ?Sized, F: Family<Unvalidated = D, Validated = D>>(r: &mut Self::Of<F>) -> &mut D { r }
}

impl InPlace for Validated {
    fn def<D: ?Sized, F: Family<Unvalidated = D, Validated = D>>(r: &Self::Of<F>) -> &D { r }
    fn def_mut<D: ?Sized, F: Family<Unvalidated = D, Validated = D>>(r: &mut Self::Of<F>) -> &mut D { r }
}

/// A sized T from outside, Unvalidated: its bytes, as T's - read as one,
/// through Deref.
#[repr(transparent)]
pub struct UnvalidatedSized<T>(T);

impl<T> core::ops::Deref for UnvalidatedSized<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> core::ops::DerefMut for UnvalidatedSized<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

/// What a Validated value of a type that grew is, extended to the current
/// type - see Family::Whole.
#[derive(Debug, Default, Clone, Copy)]
pub struct Whole;

impl State for Whole {
    type Of<F: Family> = F::Whole;
    type OfSized<T: 'static> = T;
}

/// A family's type, X<S>, in another state: In<T> is X<T> - for code generic
/// over X that names it in a state, X<Validated> from X. State is S.
pub trait InState {
    type In<T: State>: ?Sized;
    type State: State;
}

/// What the family macros (cstruct-macros' vstructs.rs) make of a type,
/// beyond what its fields say: a sized type's X<S>, an unsized type's, and
/// each DST definition's impls.
#[doc(hidden)]
#[macro_export]
macro_rules! vstruct_family {
    // A sized type's X<S>, over its definition in S: Whole, the struct
    // @whole, with those of @derives that are X<Whole>'s too; in place,
    // Validated or Unvalidated, its DST @inplace - a typestate, Unvalidated
    // over Validated, if it's a #[grown] type's
    (sized $name:ident, $fam:ident, $inplace:ident, $whole:ident, $doc:literal, [$($derive:ident),*]
     $(, $typestate:ident)?) => {
        $crate::vstruct_family!(@type $name, $fam, $doc);
        $crate::vstruct_family!(@deref $($typestate)? $name, $fam);
        $crate::vstruct_family!(@arm $name, $fam, $inplace);
        $crate::vstruct_family!(@in_place $name, $inplace);
        $crate::vstruct_family!(@whole $name, $whole $(, $derive)*);
    };

    // An unsized type's X<S>, its own definition in every state: a sized
    // union's arm as long as its count says, and made from bytes, or a
    // pointer to its header @head, in the state the caller names
    (unsized $name:ident, $head:ident) => {
        $crate::vstruct_family!(@in_state $name);

        impl<S: $crate::util::vstructs::State> $crate::util::vstructs::ArmAt for $name<S> {
            fn arm_at(b: &[u8]) -> Result<&Self, $crate::util::vstructs::VStructError> {
                $crate::util::vstructs::from_bytes::<Self>(b)
            }
        }

        $crate::vstruct_family!(@state $name, Unvalidated, $head);
        $crate::vstruct_family!(@state $name, Validated, $head);
    };

    // A DST definition, $ty - with impl generics [$g] - its head @head's
    // fields, then its tail of @elem from @tail_at: it derefs to @deref, to
    // read, as long as its reference says.
    //
    // No DerefMut: a &mut to the head would let a count - its own, or an
    // embedded header's - be rewritten, or the whole head assigned or
    // swapped, from outside what holds the value.
    (dst [$($g:tt)*] $ty:ty, $head:ty, $deref:ty, $elem:ty, $tail_at:expr) => { const _: () = {
        use $crate::util::vstructs::*;

        impl<$($g)*> ::core::ops::Deref for $ty {
            type Target = $deref;
            #[inline]
            fn deref(&self) -> &$deref {
                // SAFETY: its first bytes, as many as the head has, are the head's
                // fields, at the head's offsets - asserted where it's defined, each
                // Plain - and any of its tail they reach: it's no shorter than the
                // head, as Unsized says; the target is the head, or
                // #[repr(transparent)] over it
                unsafe { &*(self as *const Self).cast::<$deref>() }
            }
        }

        // SAFETY: its head's fields, each Plain, then its tail at TAIL - as
        // asserted where it's defined - Plain; from_raw() makes the tail as
        // long as @bytes past TAIL
        unsafe impl<$($g)*> Unsized for $ty {
            type Head = $head;
            type Elem = $elem;
            const TAIL: usize = $tail_at;
            const MIN_BYTES: usize = size_of::<$head>();
            #[inline]
            fn from_raw(p: *const u8, bytes: usize) -> *const Self {
                let n = (bytes - Self::TAIL) / size_of::<$elem>();
                ::core::ptr::slice_from_raw_parts(p.cast::<$elem>(), n) as *const Self
            }
            #[inline]
            fn from_raw_mut(p: *mut u8, bytes: usize) -> *mut Self {
                let n = (bytes - Self::TAIL) / size_of::<$elem>();
                ::core::ptr::slice_from_raw_parts_mut(p.cast::<$elem>(), n) as *mut Self
            }
        }

        const _: () = {
            assert_plain::<$elem>();
            assert!($tail_at <= size_of::<$head>());
        };

        // as a tagged union's arm, over the bytes its storage says are its
        impl<$($g)*> ArmRef for $ty {
            const MIN_LEN: usize = size_of::<$head>();

            fn arm_ref(b: &[u8]) -> Option<&Self> {
                cast(b)
            }

            fn arm_mut(b: &mut [u8]) -> Option<&mut Self> {
                cast_mut(b)
            }

            fn whole_len(len: usize) -> Option<usize> {
                whole_len::<Self>(len)
            }
        }

        impl<$($g)*> $ty {
            /// Its bytes, every one.
            pub fn as_bytes(&self) -> &[u8] {
                bytes_of(self)
            }

            /// Its bytes, to edit: for its own walks - a count may be in them.
            #[allow(dead_code)]
            pub(crate) fn as_bytes_mut(&mut self) -> &mut [u8] {
                bytes_of_mut(self)
            }
        }
    }; };

    // A sized type's X<S>: #[repr(transparent)] over its definition in S, so
    // a reference to one is a reference to the other, and impls attach to
    // X<S> for the state they're for
    (@type $name:ident, $fam:ident, $doc:literal) => {
        #[doc = $doc]
        #[repr(transparent)]
        #[allow(non_camel_case_types)]
        pub struct $name<S: $crate::util::vstructs::State = $crate::util::vstructs::Whole> {
            _state:   ::core::marker::PhantomData<S>,
            /// Its definition in state S - for offset_of!(), repr.field.
            pub repr: <S as $crate::util::vstructs::State>::Of<$fam>,
        }

        // transparent over its definition: zeroed when that may be
        unsafe impl<S: $crate::util::vstructs::State> ::pin_init::Zeroable for $name<S>
            where <S as $crate::util::vstructs::State>::Of<$fam>: ::pin_init::Zeroable {}

        $crate::vstruct_family!(@in_state $name);
    };

    (@in_state $name:ident) => {
        impl<S: $crate::util::vstructs::State> $crate::util::vstructs::InState for $name<S> {
            type In<T: $crate::util::vstructs::State> = $name<T>;
            type State = S;
        }
    };

    // To its definition in each state: Whole's, and in place the one both
    // states have, generic over them - or, a typestate, Unvalidated to
    // X<Validated>, where X's methods are, not to their definition: a #[grown]
    // type's, which is one for both, and whose length validate checks before
    // anything reads it
    (@deref $name:ident, $fam:ident) => { const _: () = {
        use $crate::util::vstructs::{Family, InPlace};

        $crate::vstruct_family!(@deref_to_def $name, $fam, Whole);

        impl<S: InPlace> ::core::ops::Deref for $name<S> {
            type Target = <$fam as Family>::Validated;
            #[inline]
            fn deref(&self) -> &Self::Target { S::def::<_, $fam>(&self.repr) }
        }

        impl<S: InPlace> ::core::ops::DerefMut for $name<S> {
            #[inline]
            fn deref_mut(&mut self) -> &mut Self::Target { S::def_mut::<_, $fam>(&mut self.repr) }
        }
    }; };
    (@deref typestate $name:ident, $fam:ident) => { const _: () = {
        use $crate::util::vstructs::{Unvalidated, Validated};

        $crate::vstruct_family!(@deref_to_def $name, $fam, Whole, Validated);

        impl ::core::ops::Deref for $name<Unvalidated> {
            type Target = $name<Validated>;
            #[inline]
            fn deref(&self) -> &Self::Target {
                // SAFETY: #[repr(transparent)], both, over the one definition
                unsafe { &*(self as *const Self as *const Self::Target) }
            }
        }

        impl ::core::ops::DerefMut for $name<Unvalidated> {
            #[inline]
            fn deref_mut(&mut self) -> &mut Self::Target {
                // SAFETY: as deref()'s
                unsafe { &mut *(self as *mut Self as *mut Self::Target) }
            }
        }
    }; };

    (@deref_to_def $name:ident, $fam:ident, $($st:ident),*) => { $(const _: () = {
        use $crate::util::vstructs::{$st, State};

        impl ::core::ops::Deref for $name<$st> {
            type Target = <$st as State>::Of<$fam>;
            #[inline]
            fn deref(&self) -> &Self::Target { &self.repr }
        }

        impl ::core::ops::DerefMut for $name<$st> {
            #[inline]
            fn deref_mut(&mut self) -> &mut Self::Target { &mut self.repr }
        }
    };)* };

    // An arm of what @arm_of, its value in place, is an arm of, in every
    // state - the struct, X<Whole>, is an arm wherever its value in place is
    (@arm $name:ident, $fam:ident, $arm_of:ident) => { const _: () = {
        use $crate::types::ArmOf;
        use $crate::util::vstructs::{ArmAt, ArmRef, State, VStructError};

        impl<U: ?Sized, S: State> ArmOf<U> for $name<S>
        where
            $arm_of: ArmOf<U>,
        {
            type Tag = <$arm_of as ArmOf<U>>::Tag;
            const TAG: Self::Tag = <$arm_of as ArmOf<U>>::TAG;
        }

        // In every state, as its definition in that state is had: the pointer
        // cast keeps the definition's metadata, its last field being it
        impl<S: State> ArmRef for $name<S> {
            const MIN_LEN: usize = <<S as State>::Of<$fam> as ArmRef>::MIN_LEN;

            fn arm_ref(b: &[u8]) -> Option<&Self> {
                // SAFETY: #[repr(transparent)] over its definition in S
                <<S as State>::Of<$fam> as ArmRef>::arm_ref(b)
                    .map(|d| unsafe { &*(d as *const <S as State>::Of<$fam> as *const Self) })
            }

            fn arm_mut(b: &mut [u8]) -> Option<&mut Self> {
                // SAFETY: as arm_ref()'s
                <<S as State>::Of<$fam> as ArmRef>::arm_mut(b)
                    .map(|d| unsafe { &mut *(d as *mut <S as State>::Of<$fam> as *mut Self) })
            }

            fn whole_len(len: usize) -> Option<usize> {
                <<S as State>::Of<$fam> as ArmRef>::whole_len(len)
            }
        }

        impl<S: State> ArmAt for $name<S>
        where
            <S as State>::Of<$fam>: ArmAt,
        {
            fn arm_at(b: &[u8]) -> Result<&Self, VStructError> {
                // SAFETY: as arm_ref()'s
                <<S as State>::Of<$fam> as ArmAt>::arm_at(b)
                    .map(|d| unsafe { &*(d as *const <S as State>::Of<$fam> as *const Self) })
            }
        }
    }; };

    // In state @st, Unsized as its definition there, @def, is
    (@unsized $name:ident, $st:ident, $def:ident) => { const _: () = {
        use $crate::util::vstructs::{$st, Unsized};

        // SAFETY: #[repr(transparent)] over its definition, whose Unsized it is
        unsafe impl Unsized for $name<$st> {
            type Head = <$def as Unsized>::Head;
            type Elem = <$def as Unsized>::Elem;
            const TAIL: usize = <$def as Unsized>::TAIL;
            const MIN_BYTES: usize = <$def as Unsized>::MIN_BYTES;
            #[inline]
            fn from_raw(p: *const u8, bytes: usize) -> *const Self {
                <$def as Unsized>::from_raw(p, bytes) as *const Self
            }
            #[inline]
            fn from_raw_mut(p: *mut u8, bytes: usize) -> *mut Self {
                <$def as Unsized>::from_raw_mut(p, bytes) as *mut Self
            }
        }
    }; };

    // In place, Validated or Unvalidated: its DST, @def, as long as its
    // reference says
    (@in_place $name:ident, $def:ident) => {
        $crate::vstruct_family!(@in_place $name, $def, Validated, Unvalidated);
    };
    (@in_place $name:ident, $def:ident, $($st:ident),*) => { $(
        $crate::vstruct_family!(@unsized $name, $st, $def);
    )* };

    // An unsized type in state @st: made from a pointer to its header, @head,
    // or bytes, or from the other state
    (@state $name:ident, $st:ident, $head:ident) => { const _: () = {
        use $crate::util::vstructs::*;

        // SAFETY: its header's count says how long it is, and that's a
        // private field - Deref is to the header, to read
        unsafe impl SelfSized for $name<$st> {
            unsafe fn from_thin(p: *const ()) -> *const Self {
                <Self as Unsized>::from_raw(p.cast(), unsafe { (*p.cast::<$head>()).bytes() })
            }
        }

        // every family's way in, whether or not this one's used
        #[allow(dead_code)]
        impl $name<$st> {
            $crate::vstruct_family!(@state_only $name, $st);
        }
    }; };

    (@state_only $name:ident, Unvalidated) => {
        /// It at the start of @bytes - off a device, from the kernel - as
        /// many as its count says: an error if they don't hold its header,
        /// aligned, or that many.
        pub fn from_bytes(bytes: &[u8]) -> Result<&Self, VStructError> {
            from_bytes::<Self>(bytes)
        }

        /// from_bytes(), mutably.
        pub fn from_bytes_mut(bytes: &mut [u8]) -> Result<&mut Self, VStructError> {
            from_bytes_mut::<Self>(bytes)
        }

        /// It, Validated - the unsafe way up: both states are laid out
        /// alike, so it's the same bytes, at the same length.
        ///
        /// # Safety
        /// What Validated says of it is true: validate passed.
        pub unsafe fn assume_validated(&self) -> &$name<Validated> {
            let p = <$name<Validated> as Unsized>::from_raw((self as *const Self).cast(), size_of_val(self));
            // SAFETY: its bytes, as long as they are; the caller's, that
            // they're Validated
            unsafe { &*p }
        }

        /// assume_validated(), mutably.
        ///
        /// # Safety
        /// As assume_validated()'s.
        pub unsafe fn assume_validated_mut(&mut self) -> &mut $name<Validated> {
            let p = <$name<Validated> as Unsized>::from_raw_mut((self as *mut Self).cast(), size_of_val(self));
            // SAFETY: as assume_validated()'s
            unsafe { &mut *p }
        }
    };

    (@state_only $name:ident, Validated) => {
        /// It, Unvalidated - the safe way down: both states are laid out
        /// alike, and an Unvalidated one trusts nothing - for what reads
        /// one that may not be valid, a printer.
        pub fn as_unvalidated(&self) -> &$name<Unvalidated> {
            let p = <$name<Unvalidated> as Unsized>::from_raw((self as *const Self).cast(), size_of_val(self));
            // SAFETY: its bytes, as long as they are
            unsafe { &*p }
        }
    };

    // X<Whole>, a sized type's: the struct @whole, with the value traits it
    // derives that are X<Whole>'s too
    (@whole $name:ident, $whole:ident $(, $derive:ident)*) => { const _: () = {
        use $crate::util::vstructs::*;

        $crate::vstruct_family!(@unsized $name, Whole, $whole);
        $($crate::vstruct_family!(@whole_derive $derive, $name, $whole);)*
    }; };

    (@whole_derive Clone, $name:ident, $whole:ident) => {
        impl Clone for $name<Whole> {
            fn clone(&self) -> Self {
                $name { _state: ::core::marker::PhantomData, repr: self.repr.clone() }
            }
        }
    };
    (@whole_derive Copy, $name:ident, $whole:ident) => {
        impl Copy for $name<Whole> {}
    };
    (@whole_derive Default, $name:ident, $whole:ident) => {
        impl Default for $name<Whole> {
            fn default() -> Self {
                $name { _state: ::core::marker::PhantomData, repr: Default::default() }
            }
        }
    };
    (@whole_derive Plain, $name:ident, $whole:ident) => {
        // SAFETY: #[repr(transparent)] over the struct, which is Plain
        unsafe impl Plain for $name<Whole> {}
    };
    (@whole_derive TypeInfo, $name:ident, $whole:ident) => {
        impl $crate::typeinfo::TypeInfo for $name<Whole> {
            const INFO: &'static $crate::typeinfo::StructInfo = <$whole as $crate::typeinfo::TypeInfo>::INFO;
        }
    };
}

/// The head of a #[counted] family - a header, and an array it counts in
/// elements: the family's type in state S, X<S>, by its head - for what
/// holds a value's bytes to hand it out as: an ioctl's buffer, Unvalidated,
/// the count checked against them; a tagged union whose arm it is, in the
/// union's state.
pub trait CountedHead: FlexArray + Plain {
    type Of<S: State>: ?Sized;
}

/// Unsized type T's head, by T's name: for an unsized vstruct or a
/// #[counted] type, the struct as written - the header, its tail a
/// zero-length flexible array. What a pointer to one, and a struct that
/// embeds one, names.
pub type Head<T> = <T as Unsized>::Head;

/// A DST of ours, in a state: a sized head's fields, inline, then its tail -
/// a slice. Its references are fat, with the tail's length, so a reference
/// reaches every byte of the value and none past it. It derefs to its head,
/// to read; never mutably, so no count - the head's, or an embedded
/// header's - can be rewritten from outside what holds the value, and its
/// fields that hold one are private. An unsized vstruct's head is the
/// struct as written, its header, whose count says how long the value is
/// (#[unsized_vstruct]); a grown type's is its floor, the value as long as
/// what holds it says (#[grown]).
///
/// A key value's, whose header is the struct as written, rounded up into its
/// tail's first bytes - bch_inode's fields end at 14, its size is 16 - has its
/// tail from where it starts, TAIL, inside its head (#[whole(tail = ..)]).
///
/// # Safety
/// Self is Head's fields, every bit pattern of each valid, then its tail at
/// TAIL - size_of::<Head>(), or within it; from_raw() and from_raw_mut() make
/// a pointer to all of @bytes, which are no fewer than MIN_BYTES - a sized
/// Self's, to its first size_of::<Self>(), its MIN_BYTES: a reference from it
/// claims them, and none past, if they're a whole number of Self's alignment
/// long too. MIN_BYTES is no fewer than Head's.
pub unsafe trait Unsized {
    /// What precedes the tail: what it derefs to.
    type Head;

    /// What its tail holds.
    type Elem: Plain;

    /// Where its tail starts.
    const TAIL: usize;

    /// The fewest bytes a reference from it claims: a sized Self's size, an
    /// unsized one's head's.
    const MIN_BYTES: usize;

    /// The value at @p, @bytes long.
    fn from_raw(p: *const u8, bytes: usize) -> *const Self;

    /// The value at @p, @bytes long, mutably.
    fn from_raw_mut(p: *mut u8, bytes: usize) -> *mut Self;
}

/// The T at the start of @bytes, as many as its count says - an error if they
/// don't hold its header, aligned, or that many, or it says fewer than its
/// header: T::from_bytes(), for an Unvalidated T.
pub fn from_bytes<T>(bytes: &[u8]) -> Result<&T, VStructError>
where
    T: Unsized + ?Sized,
    T::Head: Plain + FakeDst,
{
    let len = checked_bytes::<T::Head>(bytes)?;
    cast(&bytes[..len]).ok_or(VStructError::Ragged { ty: core::any::type_name::<T>(), len })
}

/// from_bytes(), mutably: T::from_bytes_mut().
pub fn from_bytes_mut<T>(bytes: &mut [u8]) -> Result<&mut T, VStructError>
where
    T: Unsized + ?Sized,
    T::Head: Plain + FakeDst,
{
    let len = checked_bytes::<T::Head>(bytes)?;
    cast_mut(&mut bytes[..len]).ok_or(VStructError::Ragged { ty: core::any::type_name::<T>(), len })
}

/// Whether @b can be a T, every byte of it: at least its head, aligned for
/// it, and a length a reference to one has - a whole number of its tail's
/// elements past the head, and of its alignment, the head's, so
/// size_of_val() is @b's length and the reference claims none past.
fn fits<T: Unsized + ?Sized>(b: &[u8]) -> bool {
    let (head, align) = (size_of::<T::Head>(), align_of::<T::Head>());
    b.len() >= head
        && b.as_ptr() as usize % align == 0
        && b.len() % align == 0
        && (b.len() - T::TAIL) % size_of::<T::Elem>() == 0
}

/// The longest of @len bytes a T can be: its head and as many whole tail
/// elements as fit, a multiple of its alignment - what cast() of a buffer
/// with a partial element at its end would take. None if not even its head
/// fits.
pub fn whole_len<T: Unsized + ?Sized>(len: usize) -> Option<usize> {
    let (head, align, elem) = (size_of::<T::Head>(), align_of::<T::Head>(), size_of::<T::Elem>());
    let mut n = len.checked_sub(T::TAIL)?.checked_div(elem).unwrap_or(0);
    loop {
        let l = T::TAIL + n * elem;
        if l >= head && l % align == 0 {
            return Some(l);
        }
        n = n.checked_sub(1)?;
    }
}

/// @b as a T, every byte of it - an arm, as long as what holds it says: None
/// if it can't be one, fits().
pub fn cast<T: Unsized + ?Sized>(b: &[u8]) -> Option<&T> {
    // SAFETY: fits(): its head is there, aligned, and the reference claims
    // exactly @b; any bytes are a T's
    fits::<T>(b).then(|| unsafe { &*T::from_raw(b.as_ptr(), b.len()) })
}

/// cast(), mutably.
pub fn cast_mut<T: Unsized + ?Sized>(b: &mut [u8]) -> Option<&mut T> {
    // SAFETY: as cast()
    fits::<T>(b).then(|| unsafe { &mut *T::from_raw_mut(b.as_mut_ptr(), b.len()) })
}

/// The sized @T at the start of @b - an extent entry's arm, from the entry's
/// bytes: None if they're too few for one, or not aligned for it.
pub fn cast_plain<T: Plain>(b: &[u8]) -> Option<&T> {
    // SAFETY: a T's bytes are there, aligned; T is Plain, so any bytes are one
    (b.len() >= size_of::<T>() && b.as_ptr() as usize % align_of::<T>() == 0)
        .then(|| unsafe { &*b.as_ptr().cast::<T>() })
}

/// cast_plain(), mutably: what's written through it is a T's fields, every
/// byte, so @b is still bytes.
pub fn cast_plain_mut<T: Plain>(b: &mut [u8]) -> Option<&mut T> {
    // SAFETY: as cast_plain(); Plain has no padding, so writing a T leaves
    // no byte of @b uninitialized
    (b.len() >= size_of::<T>() && b.as_ptr() as usize % align_of::<T>() == 0)
        .then(|| unsafe { &mut *b.as_mut_ptr().cast::<T>() })
}

/// @b as a slice of Plain @T: None if they're not aligned for one, or not a
/// whole number of them.
pub fn cast_plain_slice<T: Plain>(b: &[u8]) -> Option<&[T]> {
    // SAFETY: aligned, and exactly that many T's bytes; T is Plain, so any
    // bytes are one
    (b.as_ptr() as usize % align_of::<T>() == 0 && b.len() % size_of::<T>() == 0)
        .then(|| unsafe { core::slice::from_raw_parts(b.as_ptr().cast::<T>(), b.len() / size_of::<T>()) })
}

/// cast_plain_slice(), mutably.
pub fn cast_plain_slice_mut<T: Plain>(b: &mut [u8]) -> Option<&mut [T]> {
    // SAFETY: as cast_plain_slice(); Plain has no padding, so writing a T
    // leaves no byte of @b uninitialized
    (b.as_ptr() as usize % align_of::<T>() == 0 && b.len() % size_of::<T>() == 0)
        .then(|| unsafe { core::slice::from_raw_parts_mut(b.as_mut_ptr().cast::<T>(), b.len() / size_of::<T>()) })
}

/// A Plain @v's bytes, every one - to write it into bytes that needn't be
/// aligned for it; or a slice's, to read records out of a buffer of u64s.
pub fn plain_bytes<T: Plain + ?Sized>(v: &T) -> &[u8] {
    // SAFETY: Plain - every byte a field's
    unsafe { core::slice::from_raw_parts((v as *const T).cast(), size_of_val(v)) }
}

/// plain_bytes(), to write: any bytes written are a Plain value.
pub fn plain_bytes_mut<T: Plain + ?Sized>(v: &mut T) -> &mut [u8] {
    let len = size_of_val(v);
    // SAFETY: Plain - every byte a field's, and any bytes a value
    unsafe { core::slice::from_raw_parts_mut((v as *mut T).cast(), len) }
}

/// @b, every byte of which has been written, as bytes: std's
/// <[MaybeUninit<T>]>::assume_init_ref(), which the 1.85 MSRV hasn't.
///
/// # Safety
/// Every byte of @b is initialized.
pub unsafe fn assume_init_ref(b: &[core::mem::MaybeUninit<u8>]) -> &[u8] {
    unsafe { &*(core::ptr::from_ref(b) as *const [u8]) }
}

/// assume_init_ref(), to write.
///
/// # Safety
/// As assume_init_ref()'s.
pub unsafe fn assume_init_mut(b: &mut [core::mem::MaybeUninit<u8>]) -> &mut [u8] {
    unsafe { &mut *(core::ptr::from_mut(b) as *mut [u8]) }
}

/// @v's bytes, every one: what its reference reaches - T::as_bytes().
pub fn bytes_of<T: Unsized + ?Sized>(v: &T) -> &[u8] {
    // SAFETY: the value's own bytes: Unsized's are its head's and its tail's,
    // both Plain
    unsafe { core::slice::from_raw_parts((v as *const T).cast::<u8>(), size_of_val(v)) }
}

/// bytes_of(), mutably - for the walks over a container's entries, not for
/// its owner to hand out: through them, a count could change.
pub(crate) fn bytes_of_mut<T: Unsized + ?Sized>(v: &mut T) -> &mut [u8] {
    let len = size_of_val(v);
    // SAFETY: as bytes_of()
    unsafe { core::slice::from_raw_parts_mut((v as *mut T).cast::<u8>(), len) }
}

/// What holds entries, one after another: vstruct_container!()'s, a
/// transaction's sub-buffer. What its walks are made from - and only it - so
/// its entries are in its state, as Entry says.
///
/// # Safety
/// entry_bytes() are its entries, each an Entry as long as its header says,
/// one after another from the first.
pub unsafe trait Container {
    type Entry: ?Sized;

    fn entry_bytes(&self) -> &[u8];

    fn entry_bytes_mut(&mut self) -> &mut [u8];
}

/// A Validated container's entries, each an &E of exactly its own bytes:
/// vstruct_for_each(), its counts trusted, as C trusts them. One that
/// doesn't fit, or says it's shorter than its header - which would never end
/// the walk - ends it, rather than being read past.
pub struct ValidatedEntries<'a, E: ?Sized> {
    rest:   &'a [u8],
    _entry: PhantomData<&'a E>,
}

impl<'a, E: ?Sized> ValidatedEntries<'a, E> {
    /// @c's entries.
    pub fn of<C: Container<Entry = E> + ?Sized>(c: &'a C) -> Self {
        Self { rest: c.entry_bytes(), _entry: PhantomData }
    }
}

impl<'a, E: Unsized<Head: Plain + FakeDst> + ?Sized + 'a> Iterator for ValidatedEntries<'a, E> {
    type Item = &'a E;

    fn next(&mut self) -> Option<&'a E> {
        let Ok(len) = checked_bytes::<E::Head>(self.rest) else {
            self.rest = &[];
            return None;
        };
        let (e, rest) = self.rest.split_at(len);
        self.rest = rest;
        // SAFETY: checked_bytes(): its header is there, aligned, and the
        // bytes it says are its - any bytes are an E
        Some(unsafe { &*E::from_raw(e.as_ptr(), len) })
    }
}

/// ValidatedEntries, mutably.
pub struct ValidatedEntriesMut<'a, E: ?Sized> {
    rest:   &'a mut [u8],
    _entry: PhantomData<&'a mut E>,
}

impl<'a, E: ?Sized> ValidatedEntriesMut<'a, E> {
    /// @c's entries, to edit.
    pub fn of<C: Container<Entry = E> + ?Sized>(c: &'a mut C) -> Self {
        Self { rest: c.entry_bytes_mut(), _entry: PhantomData }
    }
}

impl<'a, E: Unsized<Head: Plain + FakeDst> + ?Sized + 'a> Iterator for ValidatedEntriesMut<'a, E> {
    type Item = &'a mut E;

    fn next(&mut self) -> Option<&'a mut E> {
        let Ok(len) = checked_bytes::<E::Head>(self.rest) else {
            self.rest = &mut [];
            return None;
        };
        let (e, rest) = core::mem::take(&mut self.rest).split_at_mut(len);
        self.rest = rest;
        Some(unsafe { &mut *E::from_raw_mut(e.as_mut_ptr(), len) })
    }
}

/// An Unvalidated container's entries: each checked against what's left of
/// its bytes - an error at one that doesn't fit, which ends the walk.
pub struct UnvalidatedEntries<'a, E: ?Sized> {
    /// None after an error
    rest:   Option<&'a [u8]>,
    _entry: PhantomData<&'a E>,
}

impl<'a, E: ?Sized> UnvalidatedEntries<'a, E> {
    /// The entries in @rest: a container's, from its start[] - or an ioctl's
    /// reply, one after another.
    pub fn new(rest: &'a [u8]) -> Self {
        Self { rest: Some(rest), _entry: PhantomData }
    }
}

impl<'a, E: Unsized<Head: Plain + FakeDst> + ?Sized + 'a> Iterator for UnvalidatedEntries<'a, E> {
    type Item = Result<&'a E, VStructError>;

    fn next(&mut self) -> Option<Self::Item> {
        let rest = self.rest.filter(|r| !r.is_empty())?;
        match from_bytes::<E>(rest) {
            Ok(e) => {
                self.rest = Some(&rest[size_of_val(e)..]);
                Some(Ok(e))
            }
            Err(err) => {
                self.rest = None;
                Some(Err(err))
            }
        }
    }
}

/// UnvalidatedEntries, mutably.
pub struct UnvalidatedEntriesMut<'a, E: ?Sized> {
    /// None after an error
    rest:   Option<&'a mut [u8]>,
    _entry: PhantomData<&'a mut E>,
}

impl<'a, E: ?Sized> UnvalidatedEntriesMut<'a, E> {
    /// The entries in @rest: a container's, from its start[].
    pub(crate) fn new(rest: &'a mut [u8]) -> Self {
        Self { rest: Some(rest), _entry: PhantomData }
    }
}

impl<'a, E: Unsized<Head: Plain + FakeDst> + ?Sized + 'a> Iterator for UnvalidatedEntriesMut<'a, E> {
    type Item = Result<&'a mut E, VStructError>;

    fn next(&mut self) -> Option<Self::Item> {
        let rest = self.rest.take().filter(|r| !r.is_empty())?;
        let len = match checked_bytes::<E::Head>(rest) {
            Ok(len) => len,
            Err(err) => return Some(Err(err)),
        };
        let (e, rest) = rest.split_at_mut(len);
        let Some(e) = cast_mut(e) else {
            return Some(Err(VStructError::Ragged { ty: core::any::type_name::<E>(), len }));
        };
        self.rest = Some(rest);
        Some(Ok(e))
    }
}

/// A Validated container's entries, each an &E of its own bytes, in its
/// state: keys, a Validated jset_entry's entries(). Validated, so the walk is
/// bkey_next()'s: each entry's length, read from its header, and on.
pub struct ViewEntries<'a, E: ?Sized> {
    rest:   &'a [u8],
    at:     usize,
    _entry: PhantomData<&'a E>,
}

/// The length of the entry at the start of @rest, as its header says.
///
/// # Safety
/// @rest starts with a whole, aligned E, which says it's at least its header
/// and no longer than @rest.
#[inline(always)]
unsafe fn validated_len<E: FakeDst + Plain>(rest: &[u8]) -> usize {
    let len = unsafe { &*rest.as_ptr().cast::<E>() }.bytes();
    debug_assert!(len >= size_of::<E>() && len <= rest.len(), "a validated entry of {len} bytes, in {}", rest.len());
    len
}

impl<'a, E: ?Sized> ViewEntries<'a, E> {
    /// @c's entries.
    pub fn of<C: Container<Entry = E> + ?Sized>(c: &'a C) -> Self {
        Self { rest: c.entry_bytes(), at: 0, _entry: PhantomData }
    }
}

impl<'a, E: Unsized<Head: Plain + FakeDst> + ?Sized + 'a> Iterator for ViewEntries<'a, E> {
    type Item = &'a E;

    #[inline]
    fn next(&mut self) -> Option<&'a E> {
        // the end, or a section's padding past its last entry
        if self.rest.len() < size_of::<E::Head>() {
            return None;
        }
        let len = unsafe { validated_len::<E::Head>(self.rest) };
        let (e, rest) = self.rest.split_at(len);
        self.rest = rest;
        self.at += len;
        Some(unsafe { &*E::from_raw(e.as_ptr(), len) })
    }
}

impl<'a, E: Unsized<Head: Plain + FakeDst> + ?Sized + 'a> EntryWalk for ViewEntries<'a, E> {
    fn offset(&self) -> usize {
        self.at
    }
}

/// ViewEntries, mutably.
pub struct ViewEntriesMut<'a, E: ?Sized> {
    rest:   &'a mut [u8],
    at:     usize,
    _entry: PhantomData<&'a mut E>,
}

impl<'a, E: ?Sized> ViewEntriesMut<'a, E> {
    /// @c's entries, to edit.
    pub fn of<C: Container<Entry = E> + ?Sized>(c: &'a mut C) -> Self {
        Self { rest: c.entry_bytes_mut(), at: 0, _entry: PhantomData }
    }
}

impl<'a, E: Unsized<Head: Plain + FakeDst> + ?Sized + 'a> Iterator for ViewEntriesMut<'a, E> {
    type Item = &'a mut E;

    #[inline]
    fn next(&mut self) -> Option<&'a mut E> {
        if self.rest.len() < size_of::<E::Head>() {
            return None;
        }
        let len = unsafe { validated_len::<E::Head>(self.rest) };
        let (e, rest) = core::mem::take(&mut self.rest).split_at_mut(len);
        self.rest = rest;
        self.at += len;
        Some(unsafe { &mut *E::from_raw_mut(e.as_mut_ptr(), len) })
    }
}

impl<'a, E: Unsized<Head: Plain + FakeDst> + ?Sized + 'a> EntryWalk for ViewEntriesMut<'a, E> {
    fn offset(&self) -> usize {
        self.at
    }
}


/// The @T at byte @at of @rest - a field a grown type gained, from the bytes
/// past its floor (#[grown]) - or as much of it as there is, the rest zero,
/// as for a value written before the type had it.
pub fn read_past<T: Plain>(rest: &[u8], at: usize) -> T {
    let have = rest.get(at..).unwrap_or_default();
    let n = have.len().min(size_of::<T>());
    let mut v = MaybeUninit::<T>::zeroed();
    // SAFETY: @n of @T's bytes, from @have, which has them; T is Plain, so
    // any bytes - these, then zeroes - are one
    unsafe {
        core::ptr::copy_nonoverlapping(have.as_ptr(), v.as_mut_ptr().cast::<u8>(), n);
        v.assume_init()
    }
}

/// What a tagged union's arm is handed out as, from the bytes its storage
/// says are its - as many as there are, for one written before its type
/// grew. Every arm's type is a family's, and says which by its declaration:
/// one that never grew (#[whole]), all of it, at the value's length; a grown
/// type (#[grown]), its floor and the bytes there are; an unsized vstruct
/// (#[unsized_vstruct]), as long as its count says. None if the bytes can't
/// be one: fewer than every valid one has.
pub trait ArmRef {
    /// Every valid one's bytes, at least: its head, or a grown type's floor.
    const MIN_LEN: usize;

    fn arm_ref(b: &[u8]) -> Option<&Self>;
    fn arm_mut(b: &mut [u8]) -> Option<&mut Self>;

    /// How much of @len bytes arm_ref() takes, at most: an unsized vstruct
    /// takes only whole elements - whole_len() - and every other arm all of
    /// them.
    fn whole_len(len: usize) -> Option<usize> {
        Some(len)
    }
}

/// A sized arm: all of it, aligned, at the start of the bytes its storage
/// says are its - one longer than it, written by a version since, is still
/// all of it. A key value that's in place whole (SizedVal), a #[whole] or
/// #[grown] type's struct.
///
/// # Safety
/// Any bytes are a Self where it's read: its fields are each Plain, and if
/// it has padding, WRITABLE is false.
pub unsafe trait SizedArm: Sized + 'static {
    /// Whether it's edited in place: not if it has padding - a key value's
    /// header, rounded up into its tail - which a write would put over the
    /// tail.
    const WRITABLE: bool = true;
}

impl<T: SizedArm> ArmRef for T {
    const MIN_LEN: usize = size_of::<Self>();

    fn arm_ref(b: &[u8]) -> Option<&Self> {
        cast(b.get(..size_of::<T>())?)
    }

    /// None if it has padding, which a write would put over what follows.
    fn arm_mut(b: &mut [u8]) -> Option<&mut Self> {
        T::WRITABLE.then_some(())?;
        cast_mut(b.get_mut(..size_of::<T>())?)
    }
}

// SAFETY: a reference from it claims size_of::<T>() of @bytes, the first -
// which a valid one has, being all of it - not all of them
unsafe impl<T: SizedArm> Unsized for T {
    type Head = T;
    type Elem = u8;
    const TAIL: usize = size_of::<T>();
    const MIN_BYTES: usize = size_of::<T>();

    fn from_raw(p: *const u8, _bytes: usize) -> *const Self {
        p.cast()
    }

    fn from_raw_mut(p: *mut u8, _bytes: usize) -> *mut Self {
        p.cast()
    }
}

/// An arm of a sized tagged union, from the storage's bytes past the tag:
/// the storage doesn't say how long the arm is, as an unsized one's header
/// does, so it's as long as the arm says - a plain one its size, a
/// #[counted] one as many as its count says. An error if the bytes don't
/// hold that many.
pub trait ArmAt: ArmRef {
    fn arm_at(b: &[u8]) -> Result<&Self, VStructError>;
}

/// A sized tagged union @U holding @arm, its arms @at bytes in: the payload
/// into zeroed storage, then @D marks it - stabby's order, so a tag that's
/// bits of the payload is the last thing written, not overwritten by it.
/// Never a typed copy, which leaves the bytes past a short payload undefined.
///
/// # Safety
/// @U is plain data, and @A one of its arms, plain data in bounds @at.
pub unsafe fn from_arm<U, D, A>(arm: A, at: usize) -> U
where D: crate::types::Determinant<U>, A: crate::types::ArmOf<U, Tag = D::Tag>
{
    let mut u = unsafe { with_payload(arm, at) };
    D::set(&mut u, A::TAG);
    u
}

/// from_arm() of an arm ending in a flexible array, and that array - as many
/// elements as the arm's header says: an error if @U can't hold them.
///
/// # Safety
/// As from_arm()'s.
pub unsafe fn from_arm_tailed<U, D, A>(arm: A, tail: &[A::Elem], at: usize) -> Result<U, VStructError>
where D: crate::types::Determinant<U>, A: crate::types::ArmOf<U, Tag = D::Tag> + FlexArray
{
    assert_eq!(arm.nr(), tail.len(), "{}: its header's count, and the elements given",
               core::any::type_name::<A>());
    let (tail_at, len) = (at + A::TAIL, size_of_val(tail));
    if tail_at + len > size_of::<U>() {
        return Err(VStructError::Overrun { ty: core::any::type_name::<A>(), want: A::TAIL + len,
                                           have: size_of::<U>() - at });
    }
    let mut u: U = unsafe { with_payload(arm, at) };
    // in bounds, as checked above, and the elements are plain data
    unsafe { core::ptr::copy_nonoverlapping(tail.as_ptr().cast::<u8>(), (&raw mut u).cast::<u8>().add(tail_at), len) };
    D::set(&mut u, A::TAG);
    Ok(u)
}

/// Zeroed @U holding @arm's payload @at, not yet marked as holding it.
///
/// # Safety
/// As from_arm()'s.
unsafe fn with_payload<U, A>(arm: A, at: usize) -> U {
    let mut u: U = unsafe { core::mem::zeroed() };
    unsafe { (&raw mut u).cast::<u8>().add(at).cast::<A>().write_unaligned(arm) };
    u
}

/// Why an Unvalidated tagged union has no arm to hand out: its tag, @T,
/// isn't one this version knows, or the arm is longer than the bytes there
/// are - its header says so, or for one as long as its arm (size: arm), its
/// type does. Short: too few bytes for even the tag - only an entry read off
/// bytes, size: arm, can be; a sized tagged union's storage holds its tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArmError<T> {
    Unknown(T),
    Bad { tag: T, err: VStructError },
    Short(VStructError),
}

impl<T: core::fmt::Debug> core::fmt::Display for ArmError<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Unknown(tag) => write!(f, "unknown type {tag:?}"),
            Self::Bad { tag, err } => write!(f, "type {tag:?}: {err}"),
            Self::Short(err) => write!(f, "{err}"),
        }
    }
}

#[cfg(feature = "std")]
impl<T: core::fmt::Debug> std::error::Error for ArmError<T> {}

/// A tagged union read off bytes, each value as long as the arm its tag
/// selects - tagged_union!'s `size: arm`: an ACL's entries. Every arm starts
/// with Head, the tag in it.
pub trait ArmSized: Sized {
    type Tag;
    type Head: Plain + Copy;
    /// The tag's, over the head.
    type Det: crate::types::Determinant<Self::Head>;

    /// The value at the start of @b: an error if its tag isn't one of ours -
    /// how long it is, only its arm says - or @b is shorter than its arm.
    fn read(b: &[u8]) -> Result<Self, ArmError<Self::Tag>>;

    /// Its arm's bytes, its tag among them as its arm has it.
    fn arm_bytes(&self) -> &[u8];

    /// Its tag, as the head holds it: its variant's.
    fn tag_raw(&self) -> <Self::Det as crate::types::Determinant<Self::Head>>::Tag;

    /// How long it is: its arm's size.
    fn bytes(&self) -> usize {
        self.arm_bytes().len()
    }

    /// Its head, which every arm starts with: the fields they share.
    fn head(&self) -> Self::Head {
        *cast_plain(self.arm_bytes()).expect("an arm starts with the head")
    }

    /// Write it at the start of @out - its arm's bytes, then its tag, last,
    /// as a sized union's new() marks it - and give back the rest of @out.
    /// Panics if @out is shorter than it: bytes() sizes @out.
    fn write<'o>(&self, out: &'o mut [u8]) -> &'o mut [u8] {
        let b = self.arm_bytes();
        let (to, rest) = out.split_at_mut(b.len());
        to.copy_from_slice(b);
        <Self::Det as crate::types::Determinant<Self::Head>>::set(
            cast_plain_mut(to).expect("an arm starts with the head"), self.tag_raw());
        rest
    }

    /// The values @b holds, one after another.
    fn walk(b: &[u8]) -> ArmWalk<'_, Self> {
        ArmWalk::new(b)
    }
}

/// The ArmSized values @b holds, one after another, each as long as its
/// arm: one that can't be read - its tag unknown, so there's no stepping
/// past it, or cut off - is an error, which ends the walk.
#[derive(Clone)]
pub struct ArmWalk<'a, E> {
    /// None after an error
    rest:   Option<&'a [u8]>,
    at:     usize,
    _entry: PhantomData<E>,
}

impl<'a, E> ArmWalk<'a, E> {
    pub fn new(b: &'a [u8]) -> Self {
        Self { rest: Some(b), at: 0, _entry: PhantomData }
    }
}

impl<E: ArmSized> Iterator for ArmWalk<'_, E> {
    type Item = Result<E, ArmWalkError<E::Tag>>;

    fn next(&mut self) -> Option<Self::Item> {
        let rest = self.rest.filter(|r| !r.is_empty())?;
        match E::read(rest) {
            Ok(e) => {
                self.rest = Some(&rest[e.bytes()..]);
                self.at += e.bytes();
                Some(Ok(e))
            }
            Err(err) => {
                self.rest = None;
                Some(Err(ArmWalkError { at: self.at, err }))
            }
        }
    }
}

/// An ArmWalk's error: the entry @at bytes into the walk, and what's wrong
/// with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArmWalkError<T> {
    pub at:  usize,
    pub err: ArmError<T>,
}

impl<T: core::fmt::Debug> core::fmt::Display for ArmWalkError<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "entry at byte {}: {}", self.at, self.err)
    }
}

#[cfg(feature = "std")]
impl<T: core::fmt::Debug> std::error::Error for ArmWalkError<T> {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::c;
    use crate::types::{Le32, Le64};
    use crate::util::projections::{View, ViewMut};
    use cstruct_macros::{Plain, View};

    /// @v's bytes, at @at in @buf.
    fn put<T: Plain>(buf: &mut [u64], at: usize, v: &T) {
        let b = plain_bytes(v);
        plain_bytes_mut(buf)[at..at + b.len()].copy_from_slice(b);
    }

    fn bytes(buf: &[u64]) -> &[u8] {
        plain_bytes(buf)
    }

    fn entry(u64s: usize) -> Head<c::jset_entry> {
        let mut e = Head::<c::jset_entry>::default();
        e.set_val_u64s(u64s);
        e
    }

    /// A jset of two jset_entries, of 1 and 3 u64s past their headers: and,
    /// with @second's u64s for the second, the jset's u64s saying 1 + 1 + 1
    /// + 3 still.
    fn jset(second: usize) -> Vec<u64> {
        let hdr = size_of::<Head<c::jset>>();
        let mut buf = vec![0u64; hdr / 8 + 6];
        let mut j = Head::<c::jset>::default();
        j.set_val_u64s(6);
        put(&mut buf, 0, &j);
        put(&mut buf, hdr, &entry(1));
        put(&mut buf, hdr + 16, &entry(second));
        buf
    }

    /// A validated jset - as from_thin() makes one - is as long as its count
    /// says, and so is each of its entries.
    #[test]
    fn entries_walk_variable_size_entries() {
        let buf = jset(3);
        // SAFETY: the buffer's, as long as its count says
        let j = unsafe { &*c::jset::<Validated>::from_thin(buf.as_ptr().cast()) };
        assert_eq!(j.as_bytes().len(), buf.len() * 8);
        let es: Vec<_> = j.entries().collect();
        assert_eq!(es.len(), 2);
        assert_eq!((es[0].u64s(), es[1].u64s()), (1, 3));
        assert_eq!(es[1].bytes(), 32);
    }

    /// A data_usage entry is as long as its u64s say, its arm what's past its
    /// header; r's devs are in the arm's tail. One shorter than the arm's
    /// fields isn't one.
    #[test]
    fn data_usage_entry() {
        let mut buf = [0u64; 3];
        let mut e = entry(2);
        e.type_ = c::JsetEntryType::data_usage.0 as u8;
        put(&mut buf, 0, &e);
        buf[1] = 77;                            // v
        buf[2] = 1 | 2 << 8 | 1 << 16 | 3 << 24 | 5 << 32; // r: data_type, nr_devs 2, nr_required, devs 3, 5
        // SAFETY: the buffer's, as long as its count says
        let e = unsafe { &*c::jset_entry::<Validated>::from_thin(buf.as_ptr().cast()) };
        let Ok(c::JsetEntryRef::data_usage(u)) = e.typed() else { panic!("a data_usage entry") };
        assert_eq!((u.v.get(), u.as_bytes().len()), (77, 16));
        assert_eq!(&u._rest[..u.r.nr_devs as usize], [3, 5]);

        let mut e = entry(1);
        e.type_ = c::JsetEntryType::data_usage.0 as u8;
        put(&mut buf, 0, &e);
        // SAFETY: as above
        let e = unsafe { &*c::jset_entry::<Validated>::from_thin(buf.as_ptr().cast()) };
        assert_eq!(e.as_bytes().len(), 16);
        assert!(e.typed().is_err());
    }

    /// An entry past its container's end ends the walk, unread.
    #[test]
    fn entries_stop_at_an_overrun() {
        let buf = jset(4);
        // SAFETY: the buffer's, as long as its count says
        let j = unsafe { &*c::jset::<Validated>::from_thin(buf.as_ptr().cast()) };
        assert_eq!(j.entries().count(), 1);
    }

    /// Where a vstruct's u64s count from: past its header, from its start
    /// (start = 0), past the header it embeds.
    #[test]
    fn vstruct_bytes() {
        let mut e = entry(0);
        e.set_val_u64s(3);
        assert_eq!(e.bytes(), 8 + 24);

        let mut f = Head::<c::bch_sb_field>::default();
        f.set_val_u64s(3);
        assert_eq!(f.bytes(), 24);

        let mut b = ::pin_init::zeroed::<Head<c::btree_node_entry>>();
        b.set_val_u64s(3);
        assert_eq!(b.bytes(), 16 + 24 + 24);
    }

    /// A superblock of one u64 past its header, and in it a field of @u64s.
    fn sb(field_u64s: usize) -> Vec<u64> {
        let hdr = size_of::<Head<c::bch_sb>>();
        let mut buf = vec![0u64; hdr / 8 + 1];
        let mut sb = ::pin_init::zeroed::<Head<c::bch_sb>>();
        sb.set_val_u64s(1);
        put(&mut buf, 0, &sb);
        let mut f = Head::<c::bch_sb_field>::default();
        f.set_val_u64s(field_u64s);
        put(&mut buf, hdr, &f);
        buf
    }

    /// A superblock field of no u64s spans less than its own header: it
    /// would never end a walk, so it ends it.
    #[test]
    fn entries_end_at_a_zero_length_entry() {
        let buf = sb(0);
        // SAFETY: the buffer's, as long as its count says
        let sb = unsafe { &*c::bch_sb::<Validated>::from_thin(buf.as_ptr().cast()) };
        assert_eq!(sb.entries().count(), 0);

        let buf = self::sb(1);
        // SAFETY: as above
        let sb = unsafe { &*c::bch_sb::<Validated>::from_thin(buf.as_ptr().cast()) };
        assert_eq!(sb.entries().count(), 1);
    }

    /// Bytes nothing has validated: a count past them is an error when the
    /// value's made, and an entry past its container's an error in the walk,
    /// which ends there.
    #[test]
    fn unvalidated_counts_are_checked() {
        let buf = jset(3);
        let j = c::jset::<Unvalidated>::from_bytes(bytes(&buf)).unwrap();
        let es = j.entries().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!((es[0].u64s(), es[1].u64s()), (1, 3));
        assert_eq!(es[1].as_bytes().len(), 32);

        let buf = jset(4);
        let j = c::jset::<Unvalidated>::from_bytes(bytes(&buf)).unwrap();
        let es: Vec<_> = j.entries().collect();
        assert!(matches!(es[..], [Ok(_), Err(VStructError::Overrun { want: 40, have: 32, .. })]));

        let hdr = size_of::<Head<c::jset>>();
        assert!(matches!(c::jset::<Unvalidated>::from_bytes(&bytes(&buf)[..hdr + 8]),
                         Err(VStructError::Overrun { want, have, .. }) if want == hdr + 48 && have == hdr + 8));
    }

    /// A superblock field of no u64s says it's shorter than its own header:
    /// an error, not a walk that never ends.
    #[test]
    fn unvalidated_short_entry() {
        let buf = sb(0);
        let sb = c::bch_sb::<Unvalidated>::from_bytes(bytes(&buf)).unwrap();
        let es: Vec<_> = sb.entries().collect();
        assert!(matches!(es[..], [Err(VStructError::Short { len: 0, .. })]));
    }

    /// A section counts from its start, its header's included; its arm is
    /// what's past the header, its entries from the arm's start[] - unsized
    /// ones, each as long as it says.
    #[test]
    fn entries_start_at_start() {
        let (hdr, start) = (size_of::<Head<c::bch_sb_field>>(), size_of::<c::bch_sb_field_clean>());
        let mut buf = vec![0u64; (hdr + start) / 8 + 2];
        let mut f = Head::<c::bch_sb_field>::default();
        f.set_val_u64s(buf.len());
        f.type_ = Le32::new(c::SbFieldType::clean.0);
        put(&mut buf, 0, &f);
        put(&mut buf, hdr + start, &entry(1));
        // SAFETY: the buffer's, as long as its count says
        let f = unsafe { &*c::bch_sb_field::<Validated>::from_thin(buf.as_ptr().cast()) };
        let clean = f.arm::<c::bch_sb_field_clean<Validated>>().expect("a clean section");
        assert_eq!(clean.as_bytes().len(), buf.len() * 8 - hdr);
        let es: Vec<_> = clean.entries().collect();
        assert_eq!(es.len(), 1);
        assert_eq!((es[0].u64s(), es[0].as_bytes().len()), (1, 16));
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
        let mut v = ViewMut::<Grown>::new(plain_bytes_mut(&mut short[..]));
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

#[cfg(test)]
#[allow(non_camel_case_types)]
mod unsized_tests {
    use super::*;
    use crate::types::{Le16, Le32};
    use cstruct_macros::{unsized_vstruct, MaybeZeroable, Plain};

    /// A container of tents: 8 bytes of header, its u64s counting what follows.
    #[unsized_vstruct(u64s = u64s, entries = tent)]
    #[repr(C)]
    #[derive(Default, MaybeZeroable, Plain)]
    pub struct tcont {
        u64s:      Le32,
        pub tag:   u32,
        pub start: [Head<tent>; 0],
        pub _data: [u64; 0],
    }

    /// An entry, its u64s counting itself, as a superblock field's does.
    #[unsized_vstruct(u64s = u64s, start = 0)]
    #[repr(C)]
    #[derive(Default, MaybeZeroable, Plain)]
    pub struct tent {
        u64s:      Le16,
        pub ty:    u16,
        pub pad:   u32,
        pub _data: [u64; 0],
    }

    /// A tcont of entries @sizes long, in u64s, typed 1, 2, ...: and @extra
    /// u64s past it, all ones - what's next, not its.
    fn buf(sizes: &[u16], extra: usize) -> Vec<u64> {
        let total: usize = sizes.iter().map(|&s| s as usize).sum();
        let mut buf = vec![0u64; 1 + total + extra];
        let mut h = Head::<tcont>::default();
        h.set_val_u64s(total);
        h.tag = 7;
        buf[0] = u64::from_le_bytes(plain_bytes(&h).try_into().unwrap());
        let mut at = 1;
        for (i, &s) in sizes.iter().enumerate() {
            let mut e = Head::<tent>::default();
            e.set_val_u64s(s as usize);
            e.ty = i as u16 + 1;
            buf[at] = u64::from_le_bytes(plain_bytes(&e).try_into().unwrap());
            at += s as usize;
        }
        buf[1 + total..].fill(u64::MAX);
        buf
    }

    fn bytes(buf: &[u64]) -> &[u8] {
        plain_bytes(buf)
    }

    /// A reference has the value's length - every byte of it, none past -
    /// and its walk the entries', in its state.
    #[test]
    fn unsized_reference_is_the_value() {
        let buf = buf(&[1, 3, 2], 2);

        let c = tcont::<Unvalidated>::from_bytes(bytes(&buf)).unwrap();
        assert_eq!((c.u64s(), c.tag, c._data.len(), size_of_val(c)), (6, 7, 6, 56));
        let e: Vec<(u16, usize)> = c.entries().map(|e| e.map(|e| (e.ty, e.as_bytes().len()))).collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(e, [(1, 8), (2, 24), (3, 16)]);

        // from a pointer to its header - validated, trusted
        // SAFETY: the buffer's, as long as its count says
        let c = unsafe { &*tcont::<Validated>::from_thin(buf.as_ptr().cast()) };
        assert_eq!(c.as_bytes().len(), 56);
        assert_eq!(c.entries().map(|e| e.ty).collect::<Vec<_>>(), [1, 2, 3]);
        assert_eq!(c.as_bytes().as_ptr(), buf.as_ptr().cast::<u8>());
        let e = c.entries().nth(1).unwrap();
        assert_eq!((e.u64s(), e._data.len()), (3, 2));
    }

    /// An Unvalidated container's count past its bytes, or an entry's past
    /// the container's, is an error - and a Validated one's ends the walk.
    #[test]
    fn unsized_overrun() {
        let buf = buf(&[1, 3], 0);
        assert!(matches!(tcont::<Unvalidated>::from_bytes(&bytes(&buf)[..24]),
                         Err(VStructError::Overrun { want: 40, have: 24, .. })));

        // the second entry says 5, in 3
        let mut buf = buf;
        buf[2] = 5 | (2 << 16);
        let c = tcont::<Unvalidated>::from_bytes(bytes(&buf)).unwrap();
        let r: Vec<_> = c.entries().collect();
        assert!(matches!(r.as_slice(), [Ok(_), Err(VStructError::Overrun { want: 40, have: 24, .. })]));

        // SAFETY: the buffer's, as long as its count says
        let c = unsafe { &*tcont::<Validated>::from_thin(buf.as_ptr().cast()) };
        assert_eq!(c.entries().count(), 1);
    }

    /// Walked mutably, each entry is its own bytes - and an error ends it.
    #[test]
    fn unsized_entries_mut() {
        let mut buf = buf(&[2, 1], 1);
        let mut b = bytes(&buf).to_vec();
        let c = tcont::<Unvalidated>::from_bytes_mut(&mut b).unwrap();
        for e in c.entries_mut() {
            e.unwrap()._data.fill(0x5a);
        }
        buf[2] = 0x5a;
        assert_eq!(b, bytes(&buf));
    }
}
