// SPDX-License-Identifier: GPL-2.0

//! The types the format and the structs C once shared are built from: C's
//! little-endian integers, the containers C laid out (GenRadix, Fifo), and
//! what C reads that Rust defines - CStatic, an ops table.

/// The size of the field @f projects to - by its type, so a field of a packed
/// struct needs no reference. @f is never called.
pub const fn field_size<S, F>(_f: fn(*const S) -> *const F) -> u64 {
    core::mem::size_of::<F>() as u64
}

// ---- C's little-endian integers -------------------------------------------

/// __le16, __le32, __le64 as C has them: little-endian, and aligned as the
/// integer is - on every target, i386's 4-aligned u64 included, since they're
/// the integer underneath. zerocopy's le::U16..le::U64 are aligned to 1, so a
/// field of one is placed, and aligns its struct, as no C __le field is.
/// The same new(), get(), set(), From conversions and comparison with the
/// integer.
///
/// Or what the field holds, stored as the integer: Le16<MetadataVersion>'s
/// get() is a MetadataVersion. Le8 is a byte that holds one - a u8 field of
/// an enum that's a u32 elsewhere.
macro_rules! le_int {
    ($($name:ident: $t:ty),*) => { $(
        #[repr(transparent)]
        #[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
        pub struct $name<T = $t>($t, core::marker::PhantomData<T>);

        impl LeValue<$t> for $t {
            fn from_int(v: $t) -> Self { v }
            fn into_int(self) -> $t { self }
        }

        // SAFETY: the integer, and nothing else
        unsafe impl<T> crate::util::vstructs::Plain for $name<T> {}

        // SAFETY: the integer, and zero is one
        unsafe impl<T> ::pin_init::Zeroable for $name<T> {}

        unsafe impl<T: Copy> crate::util::projections::TearSafe for $name<T> {}

        impl crate::util::vstructs::Native for $name {
            type Native = $t;
            fn to_native(self) -> $t { self.get() }
            fn from_native(n: $t) -> Self { Self::new(n) }
        }

        impl $name {
            pub const fn new(v: $t) -> Self { Self(v.to_le(), core::marker::PhantomData) }
        }

        impl<T> $name<T> {
            /// The integer it's stored as, whatever it holds - const, as
            /// get() can't be.
            pub const fn to_int(self) -> $t { <$t>::from_le(self.0) }
        }

        impl<T: LeValue<$t>> $name<T> {
            pub fn get(self) -> T { T::from_int(self.to_int()) }
            pub fn set(&mut self, v: T) { self.0 = v.into_int().to_le() }
            pub fn update(&mut self, f: impl FnOnce(T) -> T) { self.set(f(self.get())) }
        }

        impl<T: LeValue<$t>> From<T> for $name<T> {
            fn from(v: T) -> Self { Self(v.into_int().to_le(), core::marker::PhantomData) }
        }

        /// Bits set: OR is the same in either byte order.
        impl core::ops::BitOrAssign<$t> for $name {
            fn bitor_assign(&mut self, v: $t) { self.0 |= v.to_le() }
        }

        /// Bits cleared: as is AND.
        impl core::ops::BitAndAssign<$t> for $name {
            fn bitand_assign(&mut self, v: $t) { self.0 &= v.to_le() }
        }

        impl From<$name> for $t {
            fn from(v: $name) -> Self { v.get() }
        }

        impl<T: LeValue<$t> + PartialEq> PartialEq<T> for $name<T> {
            fn eq(&self, v: &T) -> bool { self.get() == *v }
        }

        impl<T: LeValue<$t> + core::fmt::Debug> core::fmt::Debug for $name<T> {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                self.get().fmt(f)
            }
        }

        impl core::fmt::LowerHex for $name {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                core::fmt::LowerHex::fmt(&self.get(), f)
            }
        }

        #[cfg(feature = "std")]
        impl zeroize::Zeroize for $name {
            fn zeroize(&mut self) { self.0.zeroize() }
        }
    )* };
}
le_int!(Le8: u8, Le16: u16, Le32: u32, Le64: u64);

/// What an LeN field holds, stored as @I: its integer, or a type made from
/// one.
pub trait LeValue<I>: Copy {
    fn from_int(v: I) -> Self;
    fn into_int(self) -> I;
}

/// How a tagged_union! says which arm it holds: stabby's IDeterminant
/// (docs.rs/stabby), n-way, over a layout that's given rather than chosen.
///
/// get() is the tag - a function of the whole value; tagged_union!'s is its
/// stored tag field. A tagged union is written payload first, then set()
/// marks it as holding that arm - stabby's order, so that a tag that's bits
/// of the payload would be written after the payload that would otherwise
/// overwrite it.
pub trait Determinant<U: ?Sized> {
    type Tag: Copy + PartialEq;
    fn get(u: &U) -> Self::Tag;
    fn set(u: &mut U, tag: Self::Tag);
}

/// An arm of tagged_union! U, by its type: the tag that selects it - for what's
/// generic over the arms, U::from_arm(). tagged_union! implements it for each
/// arm, so two arms of one type are an error.
pub trait ArmOf<U: ?Sized> {
    type Tag;
    const TAG: Self::Tag;
}

// ---- Rust's own, that C reads ------------------------------------------------

/// What one borrow lends out of another, @A, for as long as it lives: a
/// transaction's paths.
pub trait Lent<'a, A: ?Sized>: Sized {
    /// # Safety
    /// Nothing else has what it lends while it lives.
    unsafe fn lent(from: &'a A) -> Self;
}

/// A static C reads, defined in Rust: an ops table. Sync for the pointers a C
/// struct holds, which aren't: C reads it from anywhere, and nothing writes
/// it.
#[repr(transparent)]
pub struct CStatic<T>(T);

// SAFETY: new()'s
unsafe impl<T> Sync for CStatic<T> {}

impl<T> CStatic<T> {
    /// # Safety
    /// Nothing in @v is written through a shared reference - no Cell, no
    /// atomic: its pointers are its only parts that aren't Sync.
    pub const unsafe fn new(v: T) -> Self {
        CStatic(v)
    }
}

impl<T> core::ops::Deref for CStatic<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

/// GENRADIX(T) - linux/generic-radix-tree.h. C's type[0] is __aligned(1):
/// the element type only, not its alignment.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct GenRadix<T> {
    pub tree:  crate::c::__genradix,
    pub _type: core::marker::PhantomData<T>,
}

// SAFETY: zeroed is empty - genradix_init() is a memset
unsafe impl<T> ::pin_init::Zeroable for GenRadix<T> {}

unsafe impl<T: Send> Send for GenRadix<T> {}

// Opaque to TypeInfo, as bindgen's were: size only.
macro_rules! typeinfo_opaque {
    ($name:ident <$($g:tt),*>, $($bound:tt)*) => {
        impl<$($bound)*> crate::typeinfo::TypeInfo for $name<$($g),*> {
            const INFO: &'static crate::typeinfo::StructInfo = &crate::typeinfo::StructInfo {
                name:   stringify!($name),
                size:   core::mem::size_of::<Self>(),
                shape:  crate::typeinfo::Shape::Struct,
                fields: &[],
            };
        }
    };
}
typeinfo_opaque!(GenRadix<T>, T);
typeinfo_opaque!(Fifo<T, I>, T, I: crate::data_structures::fifo::FifoIdx);

/// FIFO(T), FIFO_U16_IDX(T)... - data_structures/fifo.h: __FIFO(T, I). Its methods are
/// data_structures/fifo.rs's.
#[repr(C)]
pub struct Fifo<T, I: crate::data_structures::fifo::FifoIdx = usize> {
    pub front: I,
    pub back:  I,
    pub size:  I,
    pub mask:  I,
    pub data:  *mut T,
}

// SAFETY: zeroed is empty, nothing allocated - as before init_fifo()
unsafe impl<T, I: crate::data_structures::fifo::FifoIdx + ::pin_init::Zeroable> ::pin_init::Zeroable for Fifo<T, I> {}

/// XXX migration only: rip this out once the types are Rust's alone. It's
/// here so the converted types keep the C layout exactly, where a run of C
/// bitfields happens to end mid-word; Rust's own layout won't need it.
///
/// The storage of a run of C bitfields N bytes long, N not an integer's
/// size - 3, say: C puts what follows the run right after the bytes it uses.
/// The low N bytes of a u32 or u64, in native byte order, as C's: a
/// #[bitfield(u32, repr = NeBytes<3>, from = NeBytes::<3>::from_u32, into =
/// NeBytes::<3>::to_u32)] type.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NeBytes<const N: usize>(pub [u8; N]);

// SAFETY: bytes, and nothing else
unsafe impl<const N: usize> crate::util::vstructs::Plain for NeBytes<N> {}

impl<const N: usize> crate::util::vstructs::Native for NeBytes<N> {
    type Native = Self;
    fn to_native(self) -> Self { self }
    fn from_native(n: Self) -> Self { n }
}

impl<const N: usize> Default for NeBytes<N> {
    fn default() -> Self { Self([0; N]) }
}

impl<const N: usize> NeBytes<N> {
    // where the low N bytes of a W byte integer are
    const fn low(w: usize) -> usize {
        if cfg!(target_endian = "little") { 0 } else { w - N }
    }

    pub const fn from_u32(v: u32) -> Self {
        let b = v.to_ne_bytes();
        let mut out = [0u8; N];
        let mut i = 0;
        while i < N {
            out[i] = b[Self::low(4) + i];
            i += 1;
        }
        Self(out)
    }

    pub const fn to_u32(self) -> u32 {
        let mut b = [0u8; 4];
        let mut i = 0;
        while i < N {
            b[Self::low(4) + i] = self.0[i];
            i += 1;
        }
        u32::from_ne_bytes(b)
    }

    pub const fn from_u64(v: u64) -> Self {
        let b = v.to_ne_bytes();
        let mut out = [0u8; N];
        let mut i = 0;
        while i < N {
            out[i] = b[Self::low(8) + i];
            i += 1;
        }
        Self(out)
    }

    pub const fn to_u64(self) -> u64 {
        let mut b = [0u8; 8];
        let mut i = 0;
        while i < N {
            b[Self::low(8) + i] = self.0[i];
            i += 1;
        }
        u64::from_ne_bytes(b)
    }
}

/// The storage of u64 bitfields C aligns to 8 - __aligned(8), which i386
/// needs said: its u64 is 4-aligned. #[bitfield(u64, align = 8)] uses it.
#[repr(C, align(8))]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Align8U64(pub u64);

// SAFETY: a u64, aligned to its own size - so nothing else
unsafe impl crate::util::vstructs::Plain for Align8U64 {}

impl crate::util::vstructs::Native for Align8U64 {
    type Native = Self;
    fn to_native(self) -> Self { self }
    fn from_native(n: Self) -> Self { n }
}
const _: () = assert!(core::mem::size_of::<Align8U64>() == 8);

impl Align8U64 {
    pub const fn from_u64(v: u64) -> Self { Self(v) }
    pub const fn to_u64(self) -> u64 { self.0 }
}

/// Bit @i of a run of one-bit C bitfields, an x-macro list's - stored as bytes,
/// @b, from the run's start. C allocates bitfields from the low bit of each
/// unit on a little-endian target, from the high bit on a big-endian one; a
/// unit is stored low byte or high byte first to match, so either way bit i is
/// in byte i / 8, whatever the unit's size.
const fn c_bit_mask(i: usize) -> u8 {
    if cfg!(target_endian = "little") { 1 << (i % 8) } else { 0x80 >> (i % 8) }
}

pub fn c_bit(b: &[u8], i: usize) -> bool {
    b[i / 8] & c_bit_mask(i) != 0
}

pub const fn set_c_bit(b: &mut [u8], i: usize, v: bool) {
    if v {
        b[i / 8] |= c_bit_mask(i);
    } else {
        b[i / 8] &= !c_bit_mask(i);
    }
}

/// BITS_TO_LONGS(): the unsigned longs a bitmap of @nr bits takes, for an
/// array dimension - C's `unsigned long d[BITS_TO_LONGS(nr)]`.
pub const fn bits_to_longs(nr: usize) -> usize {
    nr.div_ceil(core::ffi::c_ulong::BITS as usize)
}

/// SMP_CACHE_BYTES, as an alignment: a CacheAligned field - zero-sized -
/// aligns what follows it, and its struct, as C's __aligned(SMP_CACHE_BYTES)
/// does; repr(align) takes only a literal. The generator writes C's
/// __aligned() from it: the struct's, for a first field, or the next
/// member's. The kernel's L1_CACHE_BYTES for each target; the generated
/// headers assert it, so a wrong or missing target fails its build.
#[cfg_attr(any(target_arch = "x86_64", target_arch = "x86", target_arch = "aarch64",
               target_arch = "arm", target_arch = "riscv64", target_arch = "loongarch64"),
           repr(align(64)))]
#[cfg_attr(target_arch = "powerpc64", repr(align(128)))]
#[cfg_attr(target_arch = "s390x", repr(align(256)))]
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct CacheAligned([u8; 0]);

// SAFETY: no bytes
unsafe impl ::pin_init::Zeroable for CacheAligned {}

pub const SMP_CACHE_BYTES: usize = align_of::<CacheAligned>();
pub const L1_CACHE_BYTES: usize = SMP_CACHE_BYTES;

/// Default for a type C shares: all zeroes, C's `= {}` - as bindgen's was. For
/// types whose every field is C's, to which all zeroes is a valid value - an
/// unsized vstruct's C definition too, by name: c_default!(T_head).
macro_rules! c_default {
    ($name:ty) => {
        impl Default for $name {
            fn default() -> Self {
                // SAFETY: every field is a C type, valid all zeroes
                unsafe { core::mem::zeroed() }
            }
        }
    };
}
pub(crate) use c_default;

/// A type C declares, `struct NAME;`, and has only behind pointers: one a .c
/// file defines - Rust sees it only behind pointers too - or one Rust does,
/// in a module of its own, that C passes around. Not Send, Sync or Unpin -
/// the defining side's to say.
macro_rules! c_opaque {
    ($name:ident) => {
        #[repr(C)]
        pub struct $name {
            _opaque: [u8; 0],
            _marker: core::marker::PhantomData<(*mut u8, core::marker::PhantomPinned)>,
        }
    };
}
pub(crate) use c_opaque;
