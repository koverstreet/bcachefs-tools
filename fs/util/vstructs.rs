// SPDX-License-Identifier: GPL-2.0
//! Variable-size structs: vstructs.h's, and C's flexible array members.
//!
//! A C struct that ends in a flexible array member - `u8 devs[]`, `u64
//! _data[]` - has values that run past sizeof, as far as their own fields
//! say: replicas_entry_bytes() from nr_devs, vstruct_bytes() from u64s. A
//! pointer to one says nothing about the bytes past it, so C trusts the
//! count, having validated it wherever the bytes came in.
//!
//! Here the bound travels with the reference: a reference is a claim about
//! bytes - a &T, that size_of::<T>() of them are there and are a T - so one
//! is made at the value's own size, where its bytes are, from a buffer or
//! from C's pointer: never by widening a &T, which reaches only as far as the
//! T, and never from fewer bytes than its header.
//!
//! An unsized vstruct is a family of definitions, one per state - each its
//! own, as states that differ in structure are different types - written by
//! #[unsized_vstruct] from C's, and `jset<S>` picks one (Family). A reference
//! to one, &jset<S>, is fat: it has the tail's length, and reaches every byte
//! of the value and none past. Its state says what's known of its counts, and
//! its entries are walked in the same state:
//!  - Validated, the default: what C read and validated, from C's pointer
//!    (from_ffi()). Its counts are trusted, as C trusts them.
//!  - Unvalidated: bytes from outside - off a device, from the kernel
//!    (from_bytes()). Its count was checked against them, an overrun an
//!    error; its entries' are checked as they're walked.
//! Each state's definition is the header, then the tail, and derefs to the
//! header: C's struct, Ffi<T> - its tail zero-length, C's flexible array -
//! which is what Rust names wherever it meets C. A count is no field to set
//! through &mut: it changes only through what holds the value, which has the
//! room for it to grow - so an unsized vstruct's count is private, read as
//! u64s().
//!
//! Sized types with a tail, not yet unsized - an ioctl's reply, a replicas
//! entry: a FlexArray, its tail an array the header counts, reached as an
//! UnvalidatedRef (or UnvalidatedMut) of the bytes its count was checked
//! against.
//!
//! A value shorter than its type - written by an older version, before the
//! type grew - can't be a &T at all: it's a View, reading only its own bytes,
//! its fields past its end zero, as bch2_sb_field_get_minsize() and
//! bkey_val_copy_pad() zero them in C.
//!
//! A sized tagged_union!'s arms - extent entries, accounting positions - are
//! Flexes: a &T and the bytes it may extend into, the storage's, its count
//! checked against them each time it's used.
//!
//! What a variable-size type is, from the most general:
//!  - SelfSized, bytes(): its own bytes say how big it is - a #[counted_by]
//!    array's FlexArray is. Not extent entries, sized by the superblock's
//!    extent_type_u64s.
//!  - VStruct: SelfSized by a count of u64s, vstructs.h's - 8 byte aligned.
//!    vstruct!().
//!  - VStructContainer: a VStruct of entries, one after another from its
//!    start[]: vstruct_for_each(). vstruct_container!().
//!  - Unsized: an unsized vstruct, its reference fat.
//!
//! vstructs.h, and where it goes:
//!  - vstruct_bytes(), replicas_entry_bytes(): SelfSized::bytes()
//!  - a counted array - devs[]: FlexArray, UnvalidatedRef::tail()
//!  - vstruct_for_each(), vstruct_next(), vstruct_last(): an unsized
//!    vstruct's entries(), trusting a Validated one's counts as C does,
//!    checking an Unvalidated one's
//!
//! What's trusted: Plain, that any bytes are a valid value - which
//! #[derive(Plain)] (cstruct-macros) checks at compile time, zerocopy's
//! FromBytes for the types we define - and, for a reference made from C's
//! pointer (from_ffi()), the count: SelfSized::bytes(), which vstruct!() and
//! #[counted_by] generate from the field the struct declares as its count.

use core::marker::PhantomData;
use core::mem::{align_of, size_of, size_of_val, MaybeUninit};
use core::ops::{Deref, DerefMut};

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

    /// Its 512 byte sectors, whole blocks of 512 << @block_bits:
    /// vstruct_sectors().
    fn sectors(&self, block_bits: u32) -> usize {
        self.bytes().next_multiple_of(512 << block_bits) >> 9
    }
}

/// vstructs.h's: SelfSized by a count of u64s.
pub trait VStruct: SelfSized {
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

/// A sized vstruct of entries - a superblock section of them, clean's - one
/// after another from its start[] to its end, each as long as it says:
/// walked from a View of its bytes, which a &Self doesn't reach,
/// View::entries(). An unsized container's are vstruct_container!(unsized
/// ..)'s, on the container.
pub trait VStructContainer: VStruct {
    type Entry: ?Sized;
    /// offsetof(start)
    const START: usize;
}

/// floored_by_self!(T, ..): each T is whole in every valid value of it - a
/// header, whose count includes itself - so a View of one derefs to all of
/// it: a container's, or an entry's.
macro_rules! floored_by_self {
    ($($t:ty),* $(,)?) => {
        // SAFETY: Min is Self
        $(unsafe impl $crate::util::vstructs::Floored for $t {
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
/// vstruct!(unsized T, u64s), for an unsized vstruct, #[unsized_vstruct]'s
/// family: the count is its header's, T__ffi's - and each state's length is
/// its reference's, size_of_val(). Its count reads as u64s(), on the header,
/// which each state derefs to; it's no field to set but the container's,
/// which has the room for it - VStruct, on the header, builds one.
macro_rules! vstruct {
    (unsized $t:ident, $count:ident) => { $crate::paste! {
        $crate::util::vstructs::vstruct!(@impl [<$t __ffi>],
            ::core::mem::size_of::<[<$t __ffi>]>(), $count);
        $crate::util::vstructs::vstruct!(@unsized $t, $count);
    } };
    // counted by the header it embeds last - a btree_node's bset, its keys:
    // the count is that header's, and its own u64s() reads it
    (unsized $t:ident, $hdr:ident . $count:ident) => { $crate::paste! {
        $crate::util::vstructs::vstruct!([<$t __ffi>], $hdr.$count);
    } };
    (unsized $t:ident, $count:ident, start = $start:expr) => { $crate::paste! {
        $crate::util::vstructs::vstruct!(@impl [<$t __ffi>], $start, $count);
        $crate::util::vstructs::vstruct!(@unsized $t, $count);
    } };
    (@unsized $t:ident, $count:ident) => { $crate::paste! {
        impl [<$t __ffi>] {
            /// Its count, as a usize - to read: it changes only through what
            /// holds the value, which has room for it to grow.
            #[allow(dead_code)]
            pub fn $count(&self) -> usize {
                $crate::util::vstructs::Count::count(&{ self.$count })
            }
        }
    } };
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
///
/// vstruct_container!(unsized T, E), for an unsized vstruct and its unsized
/// entries: T's entries() are E's in T's state - a Validated one's trusted,
/// as C trusts them; an Unvalidated one's each checked against what's left,
/// an error ending the walk. With `thin E`, entries that aren't unsized -
/// keys - as a View, or an UnvalidatedRef(Mut), of their own bytes.
macro_rules! vstruct_container {
    (unsized $t:ident, $entry:ident) => { $crate::paste! {
        impl [<$t __validated>] {
            pub fn entries(&self)
                -> $crate::util::vstructs::ValidatedEntries<'_, $entry<$crate::util::vstructs::Validated>>
            {
                $crate::util::vstructs::ValidatedEntries::new(self.start_bytes())
            }
        }

        impl [<$t __unvalidated>] {
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

        $crate::util::vstructs::vstruct_container!(@start $t);
    } };
    (unsized $t:ident, thin $entry:ty) => { $crate::paste! {
        impl [<$t __validated>] {
            pub fn entries(&self) -> $crate::util::vstructs::ViewEntries<'_, $entry> {
                $crate::util::vstructs::ViewEntries::new(self.start_bytes())
            }
        }

        impl [<$t __unvalidated>] {
            pub fn entries(&self) -> $crate::util::vstructs::UnvalidatedRefEntries<'_, $entry> {
                $crate::util::vstructs::UnvalidatedRefEntries::new(self.start_bytes())
            }

            pub fn entries_mut(&mut self) -> $crate::util::vstructs::UnvalidatedRefEntriesMut<'_, $entry> {
                $crate::util::vstructs::UnvalidatedRefEntriesMut::new(self.start_bytes_mut())
            }
        }

        $crate::util::vstructs::vstruct_container!(@start $t);
    } };
    (@start $t:ident) => { $crate::paste! {
        $crate::util::vstructs::vstruct_container!(@start_of [<$t __unvalidated>], [<$t __ffi>]);
        $crate::util::vstructs::vstruct_container!(@start_of [<$t __validated>], [<$t __ffi>]);

        impl [<$t __unvalidated>] {
            /// start_bytes(), mutably: for entries_mut().
            fn start_bytes_mut(&mut self) -> &mut [u8] {
                &mut self.as_bytes_mut()[::core::mem::offset_of!([<$t __ffi>], start)..]
            }
        }
    } };
    (@start_of $s:ident, $ffi:ident) => {
        impl $s {
            /// Its entries' bytes: from its start[] to its end.
            fn start_bytes(&self) -> &[u8] {
                &self.as_bytes()[::core::mem::offset_of!($ffi, start)..]
            }
        }
    };
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

impl<'a, T: SelfSized> Flex<'a, T> {
    /// Its bytes, header and tail - what C copies with vstruct_bytes() - if
    /// what holds it holds them.
    pub fn as_bytes(&self) -> Result<&'a [u8], FlexError> {
        let want = self.head.bytes();
        self.bytes.get(..want).ok_or_else(|| FlexError::overrun::<T>(want, self.bytes.len()))
    }
}

// ---- UnvalidatedRef: bytes whose counts haven't been checked ---------------

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
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for VStructError {}

/// How many of @bytes a T at their start says are its: an error if they
/// don't hold its header, aligned, or that many - or it says fewer than its
/// header.
fn checked_bytes<T: Plain + SelfSized>(bytes: &[u8]) -> Result<usize, VStructError> {
    let ty = core::any::type_name::<T>();
    if bytes.len() < size_of::<T>() {
        return Err(VStructError::Overrun { ty, want: size_of::<T>(), have: bytes.len() });
    }
    if bytes.as_ptr() as usize % align_of::<T>() != 0 {
        return Err(VStructError::Misaligned { ty, align: align_of::<T>() });
    }
    // SAFETY: the header is there, aligned, and any bytes are a T's
    let len = unsafe { &*bytes.as_ptr().cast::<T>() }.bytes();
    if len < size_of::<T>() {
        return Err(VStructError::Short { ty, len });
    }
    if len > bytes.len() {
        return Err(VStructError::Overrun { ty, want: len, have: bytes.len() });
    }
    Ok(len)
}

/// A T and its bytes, from bytes nothing has validated it against - off a
/// device, from the kernel. Its header's count is checked against them when
/// it's made, an overrun an error; then it's those bytes exactly, header and
/// tail, and the header's fields are fields.
pub struct UnvalidatedRef<'a, T> {
    bytes: &'a [u8],
    _t:    PhantomData<&'a T>,
}

impl<T> Clone for UnvalidatedRef<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for UnvalidatedRef<'_, T> {}

impl<T> Deref for UnvalidatedRef<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: new() checked the header is there, aligned, and T is Plain
        unsafe { &*self.bytes.as_ptr().cast::<T>() }
    }
}

impl<'a, T: Plain + SelfSized> UnvalidatedRef<'a, T> {
    /// The T at the start of @bytes, as many of them as its header says.
    pub fn new(bytes: &'a [u8]) -> Result<Self, VStructError> {
        let len = checked_bytes::<T>(bytes)?;
        Ok(Self { bytes: &bytes[..len], _t: PhantomData })
    }

    /// Its bytes, header and tail.
    pub fn as_bytes(&self) -> &'a [u8] {
        self.bytes
    }
}

impl<'a, T: FlexArray + Plain> UnvalidatedRef<'a, T> {
    /// Its flexible array, as many elements as its header says: new() checked
    /// its bytes hold them.
    pub fn tail(&self) -> &'a [T::Elem] {
        // SAFETY: in its bytes, as new() checked; the array is a field of T,
        // so aligned where T is; any bytes are an Elem
        unsafe { core::slice::from_raw_parts(self.bytes[T::TAIL..].as_ptr().cast(), self.nr()) }
    }
}

/// An unsized container's thin entries, keys - each an UnvalidatedRef of
/// its own bytes, checked against what's left: an Unvalidated
/// jset_entry's entries().
pub struct UnvalidatedRefEntries<'a, E> {
    /// None after an error
    rest:   Option<&'a [u8]>,
    _entry: PhantomData<&'a E>,
}

impl<'a, E> UnvalidatedRefEntries<'a, E> {
    /// The entries in @rest: an unsized container's, from its start[].
    pub(crate) fn new(rest: &'a [u8]) -> Self {
        Self { rest: Some(rest), _entry: PhantomData }
    }
}

impl<'a, E: Plain + SelfSized + 'a> Iterator for UnvalidatedRefEntries<'a, E> {
    type Item = Result<UnvalidatedRef<'a, E>, VStructError>;

    fn next(&mut self) -> Option<Self::Item> {
        let rest = self.rest.filter(|r| !r.is_empty())?;
        match UnvalidatedRef::<E>::new(rest) {
            Ok(e) => {
                self.rest = Some(&rest[e.bytes.len()..]);
                Some(Ok(e))
            }
            Err(err) => {
                self.rest = None;
                Some(Err(err))
            }
        }
    }
}

/// An UnvalidatedRef, mutably.
pub struct UnvalidatedMut<'a, T> {
    bytes: &'a mut [u8],
    _t:    PhantomData<&'a mut T>,
}

impl<T> Deref for UnvalidatedMut<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: as UnvalidatedRef's
        unsafe { &*self.bytes.as_ptr().cast::<T>() }
    }
}

impl<T> DerefMut for UnvalidatedMut<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as deref()
        unsafe { &mut *self.bytes.as_mut_ptr().cast::<T>() }
    }
}

impl<'a, T: Plain + SelfSized> UnvalidatedMut<'a, T> {
    /// As UnvalidatedRef::new(), mutably.
    pub fn new(bytes: &'a mut [u8]) -> Result<Self, VStructError> {
        let len = checked_bytes::<T>(bytes)?;
        Ok(Self { bytes: &mut bytes[..len], _t: PhantomData })
    }

    /// A pointer to it that reaches all its bytes, for C: one from the &T
    /// reaches only the header.
    pub fn as_mut_ptr(&mut self) -> *mut T {
        self.bytes.as_mut_ptr().cast()
    }
}

impl<'a, T: FlexArray + Plain> UnvalidatedMut<'a, T> {
    /// Its flexible array, mutably, for good: as UnvalidatedRef::tail() - as
    /// many elements as new() checked its bytes hold, not as its header says
    /// now, which DerefMut may have changed since.
    pub fn into_tail(self) -> &'a mut [T::Elem] {
        let nr = (self.bytes.len() - T::TAIL) / size_of::<T::Elem>();
        // SAFETY: as UnvalidatedRef::tail()'s
        unsafe { core::slice::from_raw_parts_mut(self.bytes[T::TAIL..].as_mut_ptr().cast(), nr) }
    }
}

/// UnvalidatedRefEntries, mutably: each an UnvalidatedMut.
pub struct UnvalidatedRefEntriesMut<'a, E> {
    rest:   Option<&'a mut [u8]>,
    _entry: PhantomData<&'a mut E>,
}

impl<'a, E> UnvalidatedRefEntriesMut<'a, E> {
    /// The entries in @rest: an unsized container's, from its start[].
    pub(crate) fn new(rest: &'a mut [u8]) -> Self {
        Self { rest: Some(rest), _entry: PhantomData }
    }
}

impl<'a, E: Plain + SelfSized + 'a> Iterator for UnvalidatedRefEntriesMut<'a, E> {
    type Item = Result<UnvalidatedMut<'a, E>, VStructError>;

    fn next(&mut self) -> Option<Self::Item> {
        let rest = self.rest.take().filter(|r| !r.is_empty())?;
        match checked_bytes::<E>(rest) {
            Ok(len) => {
                let (e, rest) = rest.split_at_mut(len);
                self.rest = Some(rest);
                Some(Ok(UnvalidatedMut { bytes: e, _t: PhantomData }))
            }
            Err(err) => Some(Err(err)),
        }
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

/// A type's definitions, one per state: states that differ in structure are
/// different types, so each is its own definition - written by macro,
/// #[unsized_vstruct] - and X<S>, an alias, picks one:
///
///   pub type jset<S = Validated> = <S as State>::Of<jset__family>;
///
/// The definitions are named - X__ffi, X__unvalidated, X__validated - and an
/// impl, hand-written or generated, is always for one of those by name:
/// X<Validated> is a projection, and an impl for a projection is E0118 if
/// inherent, and breaks other crates' impls of the same trait (E0119) - to
/// their coherence check it could be any type.
pub trait Family {
    type Unvalidated: ?Sized;
    type Validated: ?Sized;
    /// Validated and extended to the current type: every field there, so
    /// every field a field. An unsized vstruct's is its Validated - its
    /// header never grew.
    type Whole: ?Sized;
}

/// A state: which of a Family's definitions X<S> is.
pub trait State {
    type Of<F: Family>: ?Sized;
}

impl State for Unvalidated {
    type Of<F: Family> = F::Unvalidated;
}

impl State for Validated {
    type Of<F: Family> = F::Validated;
}

/// What a Validated value of a type that grew is, extended to the current
/// type - see Family::Whole.
#[derive(Debug, Default, Clone, Copy)]
pub struct Whole;

impl State for Whole {
    type Of<F: Family> = F::Whole;
}

/// What C has of a family's type: its struct - an unsized vstruct's header,
/// its tail zero-length, C's flexible array; a grown type's whole struct.
/// The family's X__ffi. Implemented for each state's definition.
pub trait HasFfi {
    /// C's struct. An impl is never for Ffi<T> itself - a projection - but
    /// for the family's definition by name, X__ffi.
    type Ffi: Plain;
}

/// What C has of family type T, by T's name: its struct - see HasFfi. What
/// Rust names wherever it meets C: an extern's parameters, a pointer field,
/// a struct that embeds one.
pub type Ffi<T> = <T as HasFfi>::Ffi;

/// A DST of ours, in a state: a sized head, which it derefs to, then its
/// tail - a slice. Its references are fat, with the tail's length, so a
/// reference reaches every byte of the value and none past it. An unsized
/// vstruct's head is C's struct, its header, whose count says how long the
/// value is (#[unsized_vstruct]); a grown type's is its floor, the value as
/// long as what holds it says (#[grown]).
///
/// # Safety
/// Self is Head then its tail, at size_of::<Head>(), every bit pattern of it
/// valid; from_raw() and from_raw_mut() make a pointer to all of @bytes,
/// which are no fewer than Head's - a reference from it claims them, and
/// none past, if they're a whole number of Self's alignment long too.
pub unsafe trait Unsized {
    /// What precedes the tail: what it derefs to.
    type Head: Plain;

    /// What its tail holds.
    type Elem: Plain;

    /// The value at @p, @bytes long.
    fn from_raw(p: *const u8, bytes: usize) -> *const Self;

    /// The value at @p, @bytes long, mutably.
    fn from_raw_mut(p: *mut u8, bytes: usize) -> *mut Self;
}

/// The T C's pointer @p is to, as long as its count says - not widened from
/// a reference to its header, which reaches only that: T::from_ffi(). For a
/// T whose head says how long it is, an unsized vstruct's.
///
/// # Safety
/// @p points to a T whose count is right for its state, and those bytes stay
/// valid, and unwritten, for 'a.
pub unsafe fn from_ffi<'a, T>(p: *const T::Head) -> &'a T
where
    T: Unsized + ?Sized,
    T::Head: SelfSized,
{
    // SAFETY: the caller's
    unsafe { &*T::from_raw(p.cast(), (*p).bytes()) }
}

/// from_ffi(), mutably: T::from_ffi_mut().
///
/// # Safety
/// As from_ffi(), and nothing else touches those bytes for 'a.
pub unsafe fn from_ffi_mut<'a, T>(p: *mut T::Head) -> &'a mut T
where
    T: Unsized + ?Sized,
    T::Head: SelfSized,
{
    // SAFETY: the caller's
    unsafe { &mut *T::from_raw_mut(p.cast(), (*p).bytes()) }
}

/// The T at the start of @bytes, as many as its count says - an error if they
/// don't hold its header, aligned, or that many, or it says fewer than its
/// header: T::from_bytes(), for an Unvalidated T.
pub fn from_bytes<T>(bytes: &[u8]) -> Result<&T, VStructError>
where
    T: Unsized + ?Sized,
    T::Head: SelfSized,
{
    let len = checked_bytes::<T::Head>(bytes)?;
    // SAFETY: that many are there, aligned, and any bytes are a T
    Ok(unsafe { &*T::from_raw(bytes.as_ptr(), len) })
}

/// from_bytes(), mutably: T::from_bytes_mut().
pub fn from_bytes_mut<T>(bytes: &mut [u8]) -> Result<&mut T, VStructError>
where
    T: Unsized + ?Sized,
    T::Head: SelfSized,
{
    let len = checked_bytes::<T::Head>(bytes)?;
    // SAFETY: as from_bytes()
    Ok(unsafe { &mut *T::from_raw_mut(bytes.as_mut_ptr(), len) })
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

/// A Validated container's entries, each an &E of exactly its own bytes:
/// vstruct_for_each(), its counts trusted, as C trusts them. One that
/// doesn't fit, or says it's shorter than its header - which would never end
/// the walk - ends it, rather than being read past.
pub struct ValidatedEntries<'a, E: ?Sized> {
    rest:   &'a [u8],
    _entry: PhantomData<&'a E>,
}

impl<'a, E: ?Sized> ValidatedEntries<'a, E> {
    /// The entries in @rest: a container's, from its start[].
    pub(crate) fn new(rest: &'a [u8]) -> Self {
        Self { rest, _entry: PhantomData }
    }
}

impl<'a, E: Unsized<Head: SelfSized> + ?Sized + 'a> Iterator for ValidatedEntries<'a, E> {
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

/// An Unvalidated container's entries: each checked against what's left of
/// its bytes - an error at one that doesn't fit, which ends the walk.
pub struct UnvalidatedEntries<'a, E: ?Sized> {
    /// None after an error
    rest:   Option<&'a [u8]>,
    _entry: PhantomData<&'a E>,
}

impl<'a, E: ?Sized> UnvalidatedEntries<'a, E> {
    /// The entries in @rest: a container's, from its start[].
    pub(crate) fn new(rest: &'a [u8]) -> Self {
        Self { rest: Some(rest), _entry: PhantomData }
    }
}

impl<'a, E: Unsized<Head: SelfSized> + ?Sized + 'a> Iterator for UnvalidatedEntries<'a, E> {
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

impl<'a, E: Unsized<Head: SelfSized> + ?Sized + 'a> Iterator for UnvalidatedEntriesMut<'a, E> {
    type Item = Result<&'a mut E, VStructError>;

    fn next(&mut self) -> Option<Self::Item> {
        let rest = self.rest.take().filter(|r| !r.is_empty())?;
        match checked_bytes::<E::Head>(rest) {
            Ok(len) => {
                let (e, rest) = rest.split_at_mut(len);
                self.rest = Some(rest);
                // SAFETY: as UnvalidatedEntries'
                Some(Ok(unsafe { &mut *E::from_raw_mut(e.as_mut_ptr(), len) }))
            }
            Err(err) => Some(Err(err)),
        }
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

impl<'a, C: VStructContainer<Entry: Unsized> + Plain> View<'a, C> {
    /// Its entries - a section's, of a superblock that's validated - in that
    /// state: vstruct_for_each(), as far as its count says, ValidatedEntries'
    /// walk.
    pub fn entries(&self) -> ValidatedEntries<'a, C::Entry> {
        let bytes = self.as_bytes();
        let len = self.whole().map_or(0, |c| c.bytes().min(bytes.len()));
        ValidatedEntries::new(bytes[..len].get(C::START..).unwrap_or_default())
    }
}

/// An unsized container's thin entries, keys - each a View of its own
/// bytes: a Validated jset_entry's entries(). One that doesn't fit, or says
/// it's shorter than its header - which would never end the walk - ends it,
/// rather than being read past.
pub struct ViewEntries<'a, E> {
    rest:   &'a [u8],
    _entry: PhantomData<&'a E>,
}

impl<'a, E> ViewEntries<'a, E> {
    /// The entries in @rest: a validated unsized container's, from its
    /// start[].
    pub(crate) fn new(rest: &'a [u8]) -> Self {
        Self { rest, _entry: PhantomData }
    }
}

impl<'a, E: SelfSized + Plain + 'a> Iterator for ViewEntries<'a, E> {
    type Item = View<'a, E>;

    fn next(&mut self) -> Option<Self::Item> {
        let rest = self.rest;
        let whole = rest.len() >= size_of::<E>() && rest.as_ptr() as usize % align_of::<E>() == 0;
        // SAFETY: its header is there, aligned, and any bytes are an E
        let len = whole.then(|| unsafe { &*rest.as_ptr().cast::<E>() }.bytes())
            .filter(|&len| len >= size_of::<E>() && len <= rest.len());
        let Some(len) = len else {
            self.rest = &[];
            return None;
        };
        let (e, rest) = rest.split_at(len);
        self.rest = rest;
        Some(View::new(e))
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
/// grew. A sized arm's is a View, its fields past a short one's end zero; a
/// grown type's, its Validated definition - #[grown]'s - at the value's
/// length. None if the bytes can't be one: fewer than every valid one has.
pub trait ArmRef {
    type Ref<'a> where Self: 'a;
    type Mut<'a> where Self: 'a;

    fn arm_ref(b: &[u8]) -> Option<Self::Ref<'_>>;
    fn arm_mut(b: &mut [u8]) -> Option<Self::Mut<'_>>;
}

impl<A: Plain> ArmRef for A {
    type Ref<'a> = View<'a, A> where A: 'a;
    type Mut<'a> = ViewMut<'a, A> where A: 'a;

    fn arm_ref(b: &[u8]) -> Option<View<'_, A>> {
        Some(View::new(b))
    }

    fn arm_mut(b: &mut [u8]) -> Option<ViewMut<'_, A>> {
        Some(ViewMut::new(b))
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
    use crate::types::{Le32, Le64};
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

    fn entry(u64s: usize) -> Ffi<c::jset_entry> {
        let mut e = Ffi::<c::jset_entry>::default();
        e.set_val_u64s(u64s);
        e
    }

    /// A jset of two jset_entries, of 1 and 3 u64s past their headers: and,
    /// with @second's u64s for the second, the jset's u64s saying 1 + 1 + 1
    /// + 3 still.
    fn jset(second: usize) -> Vec<u64> {
        let hdr = size_of::<Ffi<c::jset>>();
        let mut buf = vec![0u64; hdr / 8 + 6];
        let mut j = Ffi::<c::jset>::default();
        j.set_val_u64s(6);
        put(&mut buf, 0, &j);
        put(&mut buf, hdr, &entry(1));
        put(&mut buf, hdr + 16, &entry(second));
        buf
    }

    /// A validated jset - as C's pointer has one - is as long as its count
    /// says, and so is each of its entries.
    #[test]
    fn entries_walk_variable_size_entries() {
        let buf = jset(3);
        // SAFETY: the buffer's, as long as its count says
        let j = unsafe { c::jset::<Validated>::from_ffi(buf.as_ptr().cast()) };
        assert_eq!(j.as_bytes().len(), buf.len() * 8);
        let es: Vec<_> = j.entries().collect();
        assert_eq!(es.len(), 2);
        assert_eq!((es[0].u64s(), es[1].u64s()), (1, 3));
        assert_eq!(es[1].bytes(), 32);
    }

    /// An entry past its container's end ends the walk, unread.
    #[test]
    fn entries_stop_at_an_overrun() {
        let buf = jset(4);
        // SAFETY: the buffer's, as long as its count says
        let j = unsafe { c::jset::<Validated>::from_ffi(buf.as_ptr().cast()) };
        assert_eq!(j.entries().count(), 1);
    }

    /// Where a vstruct's u64s count from: past its header, from its start
    /// (start = 0), past the header it embeds.
    #[test]
    fn vstruct_bytes() {
        let mut e = entry(0);
        e.set_val_u64s(3);
        assert_eq!(e.bytes(), 8 + 24);

        let mut f = Ffi::<c::bch_sb_field>::default();
        f.set_val_u64s(3);
        assert_eq!(f.bytes(), 24);

        let mut l: c::jset_entry_log = Default::default();
        l.set_val_u64s(2);
        assert_eq!(l.bytes(), 8 + 16);

        let mut b = Ffi::<c::btree_node_entry>::default();
        b.set_val_u64s(3);
        assert_eq!(b.bytes(), 16 + 24 + 24);
    }

    /// A superblock of one u64 past its header, and in it a field of @u64s.
    fn sb(field_u64s: usize) -> Vec<u64> {
        let hdr = size_of::<Ffi<c::bch_sb>>();
        let mut buf = vec![0u64; hdr / 8 + 1];
        let mut sb = Ffi::<c::bch_sb>::default();
        sb.set_val_u64s(1);
        put(&mut buf, 0, &sb);
        let mut f = Ffi::<c::bch_sb_field>::default();
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
        let sb = unsafe { c::bch_sb::<Validated>::from_ffi(buf.as_ptr().cast()) };
        assert_eq!(sb.entries().count(), 0);

        let buf = self::sb(1);
        // SAFETY: as above
        let sb = unsafe { c::bch_sb::<Validated>::from_ffi(buf.as_ptr().cast()) };
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

        let hdr = size_of::<Ffi<c::jset>>();
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

    /// A section counts from its start; its entries start at its start[] -
    /// unsized ones, each as long as it says.
    #[test]
    fn entries_start_at_start() {
        let hdr = size_of::<c::bch_sb_field_clean>();
        let mut buf = vec![0u64; hdr / 8 + 2];
        let mut clean: c::bch_sb_field_clean = Default::default();
        clean.set_val_u64s(hdr / 8 + 2);
        put(&mut buf, 0, &clean);
        put(&mut buf, hdr, &entry(1));
        let clean = View::<c::bch_sb_field_clean>::new(bytes(&buf));
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

#[cfg(test)]
#[allow(non_camel_case_types)]
mod unsized_tests {
    use super::*;
    use crate::types::{Le16, Le32};
    use cstruct_macros::{unsized_vstruct, CStruct, Plain};

    /// A container of tents: 8 bytes of header, its u64s counting what follows.
    #[unsized_vstruct]
    #[repr(C)]
    #[derive(Default, CStruct, Plain)]
    pub struct tcont {
        u64s:      Le32,
        pub tag:   u32,
        pub start: [Ffi<tent>; 0],
        pub _data: [u64; 0],
    }
    vstruct!(unsized tcont, u64s);
    vstruct_container!(unsized tcont, tent);

    /// An entry, its u64s counting itself, as a superblock field's does.
    #[unsized_vstruct]
    #[repr(C)]
    #[derive(Default, CStruct, Plain)]
    pub struct tent {
        u64s:      Le16,
        pub ty:    u16,
        pub pad:   u32,
        pub _data: [u64; 0],
    }
    vstruct!(unsized tent, u64s, start = 0);

    /// A tcont of entries @sizes long, in u64s, typed 1, 2, ...: and @extra
    /// u64s past it, all ones - what's next, not its.
    fn buf(sizes: &[u16], extra: usize) -> Vec<u64> {
        let total: usize = sizes.iter().map(|&s| s as usize).sum();
        let mut buf = vec![0u64; 1 + total + extra];
        let mut h = Ffi::<tcont>::default();
        h.set_val_u64s(total);
        h.tag = 7;
        buf[0] = u64::from_le_bytes(as_bytes(&h).try_into().unwrap());
        let mut at = 1;
        for (i, &s) in sizes.iter().enumerate() {
            let mut e = Ffi::<tent>::default();
            e.set_val_u64s(s as usize);
            e.ty = i as u16 + 1;
            buf[at] = u64::from_le_bytes(as_bytes(&e).try_into().unwrap());
            at += s as usize;
        }
        buf[1 + total..].fill(u64::MAX);
        buf
    }

    fn as_bytes<T: Plain>(v: &T) -> &[u8] {
        // SAFETY: Plain - every byte a field's
        unsafe { core::slice::from_raw_parts((v as *const T).cast(), size_of::<T>()) }
    }

    fn bytes(buf: &[u64]) -> &[u8] {
        // SAFETY: u64s, which any bytes are
        unsafe { core::slice::from_raw_parts(buf.as_ptr().cast(), buf.len() * 8) }
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

        // as C's pointer has it - validated, trusted
        // SAFETY: the buffer's, as long as its count says
        let c = unsafe { tcont::<Validated>::from_ffi(buf.as_ptr().cast()) };
        assert_eq!(c.as_bytes().len(), 56);
        assert_eq!(c.entries().map(|e| e.ty).collect::<Vec<_>>(), [1, 2, 3]);
        assert_eq!(c.as_ffi().cast::<u64>(), buf.as_ptr());
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
        let c = unsafe { tcont::<Validated>::from_ffi(buf.as_ptr().cast()) };
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
