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
//!  - vstruct_for_each(), vstruct_next(), vstruct_last(): Flex::entries(),
//!    each entry a Flex bounded by its parent's end; vstruct_next_entry()
//!    until its users are converted
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
/// as long as it says: vstruct_for_each().
pub trait VStructContainer: VStruct {
    type Entry: SelfSized + Plain;
    /// offsetof(start)
    const START: usize;
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
    /// @ty's header says it spans @want bytes, less than the @header bytes
    /// of the header itself: an entry that would never end a walk.
    Short { ty: &'static str, want: usize, header: usize },
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
            Self::Short { ty, want, header } =>
                write!(f, "{ty}: its header says {want} bytes, short of the {header} of the header itself"),
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

impl<'a, T: VStructContainer> Flex<'a, T> {
    /// Its entries, each a Flex bounded by its end: vstruct_for_each(). An
    /// entry that overruns it is an error, and the walk's last item.
    pub fn entries(&self) -> Result<Entries<'a, T::Entry>, FlexError> {
        let bytes = self.as_bytes()?;
        let rest = bytes.get(T::START..).ok_or(FlexError::Short {
            ty: core::any::type_name::<T>(), want: bytes.len(), header: T::START,
        })?;
        Ok(Entries { rest, _entry: core::marker::PhantomData })
    }
}

/// A vstruct's entries: Flex::entries().
pub struct Entries<'a, E> {
    rest:   &'a [u8],
    _entry: core::marker::PhantomData<&'a E>,
}

impl<'a, E: SelfSized + Plain + 'a> Iterator for Entries<'a, E> {
    type Item = Result<Flex<'a, E>, FlexError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.is_empty() {
            return None;
        }
        let entry = Flex::<E>::new(self.rest).and_then(|e| {
            let len = e.bytes();
            if len > self.rest.len() {
                return Err(FlexError::overrun::<E>(len, self.rest.len()));
            }
            if len < size_of::<E>() {
                return Err(FlexError::Short { ty: core::any::type_name::<E>(), want: len,
                                              header: size_of::<E>() });
            }
            Ok((e, len))
        });
        match entry {
            Ok((e, len)) => {
                self.rest = &self.rest[len..];
                Some(Ok(e))
            }
            Err(err) => {
                self.rest = &[];
                Some(Err(err))
            }
        }
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

// vstruct_next(entry) = (u64*)entry._data + le16(entry.u64s)
pub(crate) unsafe fn vstruct_next_entry(entry: *const c::jset_entry) -> *const c::jset_entry {
    let u64s = ((*entry).u64s).get() as usize;
    (entry as *const u8).add(8 + u64s * 8) as *const c::jset_entry
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Le16, Le32};

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
        let es: Vec<_> = j.entries().unwrap().collect::<Result<_, _>>().unwrap();
        assert_eq!(es.len(), 2);
        assert_eq!((es[0].u64s.get(), es[1].u64s.get()), (1, 3));
        assert_eq!(es[1].as_bytes().unwrap().len(), 32);
    }

    #[test]
    fn entries_stop_at_an_overrun() {
        let buf = jset(4);
        let j = Flex::<c::jset>::new(bytes(&buf)).unwrap();
        let es: Vec<_> = j.entries().unwrap().collect();
        assert_eq!(es.len(), 2);
        assert!(es[0].is_ok());
        assert!(matches!(es[1], Err(FlexError::Overrun { want: 40, have: 32, .. })), "{:?}", es[1].as_ref().err());
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
        let es: Vec<_> = clean.entries().unwrap().collect::<Result<_, _>>().unwrap();
        assert_eq!(es.len(), 1);
        assert_eq!(es[0].u64s.get(), 1);
    }

    /// A superblock field of no u64s spans less than its own header: it
    /// would never end a walk.
    #[test]
    fn entries_refuse_a_zero_length_entry() {
        let hdr = size_of::<c::bch_sb>();
        let mut buf = vec![0u64; hdr / 8 + 1];
        let mut sb: c::bch_sb = Default::default();
        sb.u64s = Le32::new(1);
        put(&mut buf, 0, &sb);
        put(&mut buf, hdr, &c::bch_sb_field { u64s: Le32::new(0), ..Default::default() });
        let sb = Flex::<c::bch_sb>::new(bytes(&buf)).unwrap();
        let es: Vec<_> = sb.entries().unwrap().collect();
        assert!(matches!(es[..], [Err(FlexError::Short { want: 0, header: 8, .. })]),
                "{:?}", es.iter().map(|e| e.as_ref().err()).collect::<Vec<_>>());
    }
}
