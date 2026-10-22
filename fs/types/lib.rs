// SPDX-License-Identifier: GPL-2.0

//! Types defined in Rust that C shares: the records their C is written from.
//!
//! A struct C shares is a normal #[repr(C)] Rust struct, #[derive(CStruct)]
//! (fs/cstruct-macros); its fields with a C rendering are C's too, and the
//! others - a Lock<c::mutex, KVVec<u32>>, say - Rust's alone, which C
//! reserves opaque storage for. Enums, constants, typedefs, x-macro lists,
//! bitmask accessors and C not yet converted are macros there too: c_enum!,
//! c_const!, c_typedef!, c_xmacro!, c_bitmask!, c_verbatim!.
//!
//! And C's container types, which the types hold: DArray, GenRadix, Fifo,
//! MinHeap - and c_opaque!, for the types only C's .c files define.
//!
//! Only rustc knows a Rust type's layout, and only for the target it compiles
//! for - so the C is written from what the target build of this crate says.
//! Built with --cfg bch_cstruct_records, each of those macros puts a record in
//! the object's .discard.bch_cstruct section: the item's source and, for a
//! struct, its layout, each number const-evaluated for the target. The build
//! extracts the section with objcopy, and rust_types_gen (main.rs), a host
//! program of its own, writes the headers from it. The object is never run, so
//! a cross build works the same as a native one. The section is named so that
//! linking a module drops it: the kernel build reads the records from mod.o
//! itself, and the module linker script discards .discard.*.
//!
//! A record, every number little-endian:
//!
//!   magic     "CSR1"
//!   len       u32, the whole record's
//!   line      u32, the macro's line in its source file
//!   file      u32 length, then file!() - the macro's source file
//!   text      u32 length, then the item as source - which cstruct.rs parses
//!   nums      u32 count, then u64s: for a struct, its size and alignment,
//!             then each field's offset, size and alignment, in order
//!
//! A source file's records, ordered by line, are a header: foo/types.rs
//! gives foo/types_gen.h, foo/bar_types.rs foo/bar_gen.h - and foo/bar.rs's
//! #[c_export]s follow bar_types.rs's there. By file, not module, as mod.rs's
//! #[path]s make the two differ.

pub const RECORD_MAGIC: [u8; 4] = *b"CSR1";

/// The length of the record for @file, @text and @nums numbers.
pub const fn record_len(file: &str, text: &str, nums: usize) -> usize {
    4 + 4 + 4 + 4 + file.len() + 4 + text.len() + 4 + 8 * nums
}

const fn put<const LEN: usize>(mut out: [u8; LEN], mut at: usize, b: &[u8]) -> ([u8; LEN], usize) {
    let mut i = 0;
    while i < b.len() {
        out[at] = b[i];
        at += 1;
        i += 1;
    }
    (out, at)
}

/// The record: LEN is record_len() of the same arguments.
pub const fn record<const LEN: usize>(file: &str, line: u32, text: &str, nums: &[u64]) -> [u8; LEN] {
    let out = [0u8; LEN];
    let (out, at) = put(out, 0, &RECORD_MAGIC);
    let (out, at) = put(out, at, &(LEN as u32).to_le_bytes());
    let (out, at) = put(out, at, &line.to_le_bytes());
    let (out, at) = put(out, at, &(file.len() as u32).to_le_bytes());
    let (out, at) = put(out, at, file.as_bytes());
    let (out, at) = put(out, at, &(text.len() as u32).to_le_bytes());
    let (out, at) = put(out, at, text.as_bytes());
    let (mut out, mut at) = put(out, at, &(nums.len() as u32).to_le_bytes());
    let mut i = 0;
    while i < nums.len() {
        (out, at) = put(out, at, &nums[i].to_le_bytes());
        i += 1;
    }
    assert!(at == LEN, "CStruct record: length isn't record_len()'s");
    out
}

/// The size of the field @f projects to - by its type, so a field of a packed
/// struct needs no reference. @f is never called.
pub const fn field_size<S, F>(_f: fn(*const S) -> *const F) -> u64 {
    core::mem::size_of::<F>() as u64
}

/// The alignment of the field @f projects to's type.
pub const fn field_align<S, F>(_f: fn(*const S) -> *const F) -> u64 {
    core::mem::align_of::<F>() as u64
}

// ---- C's little-endian integers -------------------------------------------

/// __le16, __le32, __le64 as C has them: little-endian, and aligned as the
/// integer is - on every target, i386's 4-aligned u64 included, since they're
/// the integer underneath. zerocopy's le::U16..le::U64 are aligned to 1, so a
/// field of one is placed, and aligns its struct, as no C __le field is.
/// The same new(), get(), set(), From conversions and comparison with the
/// integer.
macro_rules! le_int {
    ($($name:ident: $t:ty),*) => { $(
        #[repr(transparent)]
        #[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
        pub struct $name($t);

        // SAFETY: the integer, and nothing else
        unsafe impl crate::util::vstructs::Plain for $name {}

        // SAFETY: the integer, and zero is one
        unsafe impl ::pin_init::Zeroable for $name {}

        impl crate::util::vstructs::Native for $name {
            type Native = $t;
            fn to_native(self) -> $t { self.get() }
            fn from_native(n: $t) -> Self { Self::new(n) }
        }

        impl $name {
            pub const fn new(v: $t) -> Self { Self(v.to_le()) }
            pub const fn get(self) -> $t { <$t>::from_le(self.0) }
            pub fn set(&mut self, v: $t) { self.0 = v.to_le() }
        }

        impl From<$t> for $name {
            fn from(v: $t) -> Self { Self::new(v) }
        }

        /// Bits set: OR is the same in either byte order.
        impl core::ops::BitOrAssign<$t> for $name {
            fn bitor_assign(&mut self, v: $t) { self.0 |= v.to_le() }
        }

        impl From<$name> for $t {
            fn from(v: $name) -> Self { v.get() }
        }

        impl PartialEq<$t> for $name {
            fn eq(&self, v: &$t) -> bool { self.get() == *v }
        }

        impl core::fmt::Debug for $name {
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
le_int!(Le16: u16, Le32: u32, Le64: u64);

// ---- C's containers ------------------------------------------------------
//
// Laid out as the C macros make them; C is the code that uses them, Rust only
// holds them, so they're opaque - darray's layout is C's, not Vec's. A field
// of one is declared in C with its macro - the generator's container_c().

/// DARRAY(T) - DARRAY_PREALLOCATED(T, N) - util/darray.h. Not Clone or
/// Copy: it owns its allocation, and a copy would be a second owner.
#[repr(C)]
pub struct DArray<T, const N: usize = 0> {
    pub nr:           usize,
    pub size:         usize,
    pub data:         *mut T,
    pub preallocated: [T; N],
}

// SAFETY: zeroed is empty, with nothing allocated - as a kzalloc()'d struct
// holding one has always started out; the preallocated elements, if any, are
// Ts zeroed
unsafe impl<T: ::pin_init::Zeroable, const N: usize> ::pin_init::Zeroable for DArray<T, N> {}

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

/// Empty, as C's `= {}`: nothing allocated.
impl<T> Default for DArray<T> {
    fn default() -> Self {
        Self { nr: 0, size: 0, data: core::ptr::null_mut(), preallocated: [] }
    }
}

// ---- Rust's own, in C's structs ---------------------------------------------

/// A field of a struct C shares that's Rust's alone - a Lock<c::mutex,
/// KVVec<u32>>, say: C reserves its storage, as opaque bytes of its size and
/// alignment, and never looks inside; Rust reaches it as a @T, by Deref.
/// Declared with rust_only!(T), which sizes it.
///
/// To C, and to improper_ctypes, it's @U64S u64s. The lint looks through the
/// *mut bch_fs that extern blocks pass into every struct bch_fs holds, and a
/// @T - not FFI-safe, and none of C's business - would fail every one of
/// them; @T is only a PhantomData, which the lint doesn't inspect.
///
/// Rust allocates the struct holding it, with the struct's init() - see
/// #[derive(Init)] - and new() is the field's initializer there, its
/// #[init(...)]. So there's always a @T in it, for Deref, and Drop drops it.
/// It isn't Zeroable, whatever @T is: a field of it with no #[init] is a
/// compile error, not a zeroed @T.
#[repr(C)]
pub struct RustOnly<T, const U64S: usize> {
    /// UnsafeCell: a @T - a lock - may be written through a shared reference
    storage: core::cell::UnsafeCell<[core::mem::MaybeUninit<u64>; U64S]>,
    _t:      core::marker::PhantomData<T>,
}

// SAFETY: it's a @T, behind the storage C reserved
unsafe impl<T: Send, const U64S: usize> Send for RustOnly<T, U64S> {}
unsafe impl<T: Sync, const U64S: usize> Sync for RustOnly<T, U64S> {}

// SAFETY: zeroed storage is a zeroed @T - which, @T being Zeroable, is a @T,
// as new() would have made one. A @T with invariants zeroes can't satisfy -
// a lock - isn't Zeroable, so neither is its RustOnly.
unsafe impl<T: ::pin_init::Zeroable, const U64S: usize> ::pin_init::Zeroable for RustOnly<T, U64S> {}

impl<T, const U64S: usize> RustOnly<T, U64S> {
    /// The storage holds a @T: rust_only!() sizes it, and u64 alignment is
    /// all it has.
    const FITS: () = assert!(core::mem::size_of::<T>() <= U64S * 8 &&
                             core::mem::align_of::<T>() <= core::mem::align_of::<u64>(),
                             "RustOnly: storage too small, or too little aligned, for its T");

    /// A RustOnly holding the @T @init makes, in place - it's pinned there.
    pub fn new<E>(init: impl ::pin_init::PinInit<T, E>) -> impl ::pin_init::PinInit<Self, E> {
        let () = Self::FITS;
        // SAFETY: the storage is a @T's, by FITS, so initializing the @T
        // initializes the RustOnly; on Err, init left nothing to drop.
        unsafe {
            ::pin_init::pin_init_from_closure(move |slot: *mut Self| {
                crate::util::alloc::init_at(init, slot.cast::<T>())
            })
        }
    }
}

impl<T, const U64S: usize> Drop for RustOnly<T, U64S> {
    fn drop(&mut self) {
        // SAFETY: a @T, as new() made it - and nothing reaches it after
        unsafe { core::ptr::drop_in_place(self.storage.get().cast::<T>()) }
    }
}

impl<T, const U64S: usize> core::ops::Deref for RustOnly<T, U64S> {
    type Target = T;

    fn deref(&self) -> &T {
        let () = Self::FITS;
        // SAFETY: a @T, as new() made it - the containing struct's init()
        // ran before anyone was handed it
        unsafe { &*self.storage.get().cast::<T>() }
    }
}

/// RustOnly<@T>, its storage sized for a @T.
macro_rules! rust_only {
    ($t:ty) => {
        $crate::types::RustOnly<$t, { core::mem::size_of::<$t>().div_ceil(8) }>
    };
}
pub(crate) use rust_only;

// ---- Rust's own, that C reads ------------------------------------------------

/// A static C reads, defined in Rust - a #[c_export] static, C's `extern const
/// T`. Sync for the pointers a C struct holds, which aren't: C reads it from
/// anywhere, and nothing writes it.
#[repr(transparent)]
pub struct CStatic<T>(T);

// SAFETY: new()'s
unsafe impl<T> Sync for CStatic<T> {}

impl<T> CStatic<T> {
    /// # Safety
    /// Nothing in @v is written through a shared reference - no Cell, no
    /// atomic: its pointers are its only parts that aren't Sync. #[c_export]
    /// calls this for its statics.
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

/// util/darray.h's DEFINE_DARRAY()s: C's typedefs are its own, these the
/// same types for Rust.
#[allow(non_camel_case_types)]
pub mod darrays {
    use super::DArray;
    use crate::util::ffi::c_char;

    pub type darray_char = DArray<c_char>;
    pub type darray_u8   = DArray<u8>;
    pub type darray_u16  = DArray<u16>;
    pub type darray_u32  = DArray<u32>;
    pub type darray_u64  = DArray<u64>;
    pub type darray_s8   = DArray<i8>;
    pub type darray_s16  = DArray<i16>;
    pub type darray_s32  = DArray<i32>;
    pub type darray_s64  = DArray<i64>;
    pub type darray_str  = DArray<*mut c_char>;
    pub type darray_const_str = DArray<*const c_char>;
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
typeinfo_opaque!(DArray<T, N>, T, const N: usize);
typeinfo_opaque!(GenRadix<T>, T);
typeinfo_opaque!(Fifo<T, I>, T, I);
typeinfo_opaque!(MinHeap<T, N>, T, const N: usize);

/// DEFINE_MIN_HEAP(T, name) - linux/min_heap.h: laid out as a darray, but a
/// heap - its own type. Not Clone or Copy: it owns its allocation.
#[repr(C)]
pub struct MinHeap<T, const N: usize = 0> {
    pub nr:           usize,
    pub size:         usize,
    pub data:         *mut T,
    pub preallocated: [T; N],
}

// SAFETY: as DArray's: zeroed is empty, nothing allocated
unsafe impl<T: ::pin_init::Zeroable, const N: usize> ::pin_init::Zeroable for MinHeap<T, N> {}

/// FIFO(T), FIFO_U16_IDX(T)... - util/fifo.h: __FIFO(T, I).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Fifo<T, I = usize> {
    pub front: I,
    pub back:  I,
    pub size:  I,
    pub mask:  I,
    pub data:  *mut T,
}

// SAFETY: zeroed is empty, nothing allocated - as before init_fifo()
unsafe impl<T, I: ::pin_init::Zeroable> ::pin_init::Zeroable for Fifo<T, I> {}

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

pub fn set_c_bit(b: &mut [u8], i: usize, v: bool) {
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

/// sizeof(long), as an alignment: as CacheAligned, C's __aligned(sizeof(long)).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct LongAligned([core::ffi::c_long; 0]);

// SAFETY: no bytes
unsafe impl ::pin_init::Zeroable for CacheAligned {}
unsafe impl ::pin_init::Zeroable for LongAligned {}

/// C's atomic64_t, in a struct Rust defines: an AtomicI64 - its one s64 - and
/// Zeroable, which pin_init has no impl of for atomics. Declared to C with
/// #[c("atomic64_t ...")].
#[repr(transparent)]
#[derive(Default)]
pub struct Atomic64(core::sync::atomic::AtomicI64);

// SAFETY: an integer, and zero is one
unsafe impl ::pin_init::Zeroable for Atomic64 {}

impl core::ops::Deref for Atomic64 {
    type Target = core::sync::atomic::AtomicI64;

    fn deref(&self) -> &core::sync::atomic::AtomicI64 {
        &self.0
    }
}

/// Default for a type C shares: all zeroes, C's `= {}` - as bindgen's was. For
/// types whose every field is C's, to which all zeroes is a valid value - an
/// unsized vstruct's C definition too, by name: c_default!(T__head).
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

/// A type C declares, `struct NAME;`, and defines only in a .c file: Rust
/// sees it only behind pointers. Not Send, Sync or Unpin - C's to say.
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
