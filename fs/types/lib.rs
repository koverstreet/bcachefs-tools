// SPDX-License-Identifier: GPL-2.0

//! Types defined in Rust that C shares: the records their C is written from.
//!
//! A struct C shares is a normal #[repr(C)] Rust struct, #[derive(CStruct)]
//! (fs/cstruct-macros); its fields with a C rendering are C's too, and the
//! others - a Mutex<KVVec<u32>>, say - Rust's alone, which C reserves opaque
//! storage for. Enums, constants, typedefs, x-macro lists, bitmask accessors
//! and C not yet converted are macros there too: c_enum!, c_const!,
//! c_typedef!, c_xmacro!, c_bitmask!, c_verbatim!.
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
//! A source file's records, ordered by line, are one header: foo/types.rs
//! gives foo/types_gen.h, foo/bar_types.rs foo/bar_gen.h - by file, not
//! module, as mod.rs's #[path]s make the two differ.

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

// ---- C's containers ------------------------------------------------------
//
// Laid out as the C macros make them; C is the code that uses them, Rust only
// holds them, so they're opaque - darray's layout is C's, not Vec's. A field
// of one is declared in C with its macro, by #[c].

/// DARRAY(T) - DARRAY_PREALLOCATED(T, N) - util/darray.h. Not Clone or
/// Copy: it owns its allocation, and a copy would be a second owner.
#[repr(C)]
pub struct DArray<T, const N: usize = 0> {
    pub nr:           usize,
    pub size:         usize,
    pub data:         *mut T,
    pub preallocated: [T; N],
}

/// How a tagged_union! says which arm it holds: stabby's IDeterminant
/// (docs.rs/stabby), n-way, over a layout that's given rather than chosen.
///
/// get() is the tag - a function of the whole value, not necessarily a field
/// of it: an extent entry's is the lowest set bit of its first word. A tagged
/// union is written payload first, then set() marks it as holding that arm -
/// stabby's order, so that a tag that's bits of the payload is written after
/// the payload that would otherwise overwrite it.
///
/// tagged_union! makes one for a stored tag; for any other, it's written by
/// hand and named with `by`.
pub trait Determinant<U> {
    type Tag: Copy + PartialEq;
    fn get(u: &U) -> Self::Tag;
    fn set(u: &mut U, tag: Self::Tag);
}

/// An arm of tagged_union! U, by its type: the tag that selects it - for what's
/// generic over the arms, U::from_arm(). tagged_union! implements it for each
/// arm, so two arms of one type are an error.
pub trait ArmOf<U> {
    type Tag;
    const TAG: Self::Tag;
}

/// Empty, as C's `= {}`: nothing allocated.
impl<T> Default for DArray<T> {
    fn default() -> Self {
        Self { nr: 0, size: 0, data: core::ptr::null_mut(), preallocated: [] }
    }
}

/// util/darray.h's DEFINE_DARRAY()s: C's typedefs are its own, these the
/// same types for Rust.
#[allow(non_camel_case_types)]
pub mod darrays {
    use super::DArray;
    use core::ffi::c_char;

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

/// SMP_CACHE_BYTES, as an alignment: a [CacheAligned; 0] field aligns what
/// follows it, and its struct, as C's __aligned(SMP_CACHE_BYTES) and
/// ____cacheline_aligned do - repr(align) takes only a literal. The
/// kernel's L1_CACHE_BYTES for each target; the generated headers assert
/// it, so a wrong or missing target fails its build.
#[cfg_attr(any(target_arch = "x86_64", target_arch = "x86", target_arch = "aarch64",
               target_arch = "arm", target_arch = "riscv64", target_arch = "loongarch64"),
           repr(align(64)))]
#[cfg_attr(target_arch = "powerpc64", repr(align(128)))]
#[cfg_attr(target_arch = "s390x", repr(align(256)))]
#[derive(Clone, Copy, Default)]
pub struct CacheAligned;

/// Default for a type C shares: all zeroes, C's `= {}` - as bindgen's was. For
/// types whose every field is C's, to which all zeroes is a valid value.
macro_rules! c_default {
    ($name:ident) => {
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
