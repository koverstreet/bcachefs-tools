#![allow(non_camel_case_types)]

use super::iter::BtreeIter;
use crate::c;
use crate::fs::Fs;
use crate::printbuf_to_formatter;
use core::fmt;
#[cfg(feature = "std")]
use core::str::FromStr;
#[cfg(feature = "std")]
use crate::errcode::{bch_errcode, BchError};
use crate::util::vstructs::{ArmRef, Floored, Unsized};
use core::marker::PhantomData;

use c::bpos as Bpos;

use core::cmp::Ordering;

/// struct bkey, in u64s.
pub const BKEY_U64S: usize = core::mem::size_of::<c::bkey>() / core::mem::size_of::<u64>();

/// A BkeyI, generic or typed, for what takes either: btree_insert().
pub trait AsBkeyI {
    fn as_bkey_i_mut(&mut self) -> &mut BkeyI;
}

impl<V: ?Sized> AsBkeyI for BkeyI<V> {
    fn as_bkey_i_mut(&mut self) -> &mut BkeyI {
        self.k_i_mut()
    }
}

impl c::bkey {
    /// Of type deleted, at POS_MIN, with no value: as bkey_init() sets.
    pub fn new() -> Self {
        Self {
            u64s:        BKEY_U64S as u8,
            format_bits: c::bkey_format_bits::new().with_format(c::KEY_FORMAT_CURRENT as u8),
            ..Default::default()
        }
    }

    /// As its u64s say.
    pub fn val_bytes(&self) -> usize {
        (self.u64s as usize).saturating_sub(BKEY_U64S) * size_of::<u64>()
    }

    /// Whether it's @K's type: the tag. Not whether its value is long enough
    /// - that's validate's, as in C.
    pub fn is<K: TypedBkey + ?Sized>(&self) -> bool {
        self.type_ as u32 == K::TYPE.0
    }

    pub fn set_val_bytes(&mut self, bytes: usize) {
        let u64s = BKEY_U64S + bytes.div_ceil(size_of::<u64>());
        self.u64s = u8::try_from(u64s)
            .unwrap_or_else(|_| panic!("key of {u64s} u64s: a key is at most {} u64s", u8::MAX));
    }

    /// Make an extent @new_size long from where it starts: its end, its
    /// position, moves.
    pub fn resize(&mut self, new_size: u32) {
        self.p.offset = self.p.offset - self.size as u64 + new_size as u64;
        self.size = new_size;
    }

    pub fn set_range(&mut self, inode: u64, start: u64, end: u64, snapshot: u32) {
        self.p = spos(inode, end, snapshot);
        self.size = (end - start) as u32;
    }

    pub fn key_type(&self) -> c::bch_bkey_type {
        c::bch_bkey_type(self.type_ as u32)
    }

    pub fn set_version_lo(&mut self, version: u64) {
        self.bversion.lo = version;
    }

    pub fn start_pos(&self) -> c::bpos {
        c::bpos { offset: self.start_offset(), ..self.p }
    }

    pub fn start_offset(&self) -> u64 {
        self.p.offset.wrapping_sub(self.size as u64)
    }

    pub fn is_deleted(&self) -> bool {
        self.key_type() == c::bch_bkey_type::KEY_TYPE_deleted
    }

    /// A tombstone in an extents btree.
    pub fn is_extent_whiteout(&self) -> bool {
        matches!(self.key_type(),
                 c::bch_bkey_type::KEY_TYPE_deleted |
                 c::bch_bkey_type::KEY_TYPE_whiteout |
                 c::bch_bkey_type::KEY_TYPE_extent_whiteout)
    }

    pub fn is_btree_ptr(&self) -> bool {
        matches!(self.key_type(),
                 c::bch_bkey_type::KEY_TYPE_btree_ptr | c::bch_bkey_type::KEY_TYPE_btree_ptr_v2)
    }
}

/// As Display.
impl fmt::Debug for Bpos {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl PartialEq for Bpos {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Bpos {}

impl PartialOrd for Bpos {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Bpos {
    fn cmp(&self, other: &Self) -> Ordering {
        let l_inode = self.inode;
        let r_inode = other.inode;
        let l_offset = self.offset;
        let r_offset = other.offset;
        let l_snapshot = self.snapshot;
        let r_snapshot = other.snapshot;

        l_inode
            .cmp(&r_inode)
            .then(l_offset.cmp(&r_offset))
            .then(l_snapshot.cmp(&r_snapshot))
    }
}

pub const fn spos(inode: u64, offset: u64, snapshot: u32) -> Bpos {
    Bpos {
        inode,
        offset,
        snapshot,
    }
}

pub const fn pos(inode: u64, offset: u64) -> Bpos {
    spos(inode, offset, 0)
}

pub const POS_MIN: Bpos = spos(0, 0, 0);
pub const POS_MAX: Bpos = spos(u64::MAX, u64::MAX, 0);
pub const SPOS_MAX: Bpos = spos(u64::MAX, u64::MAX, u32::MAX);

impl c::bpos {
    /// The next position, snapshot field first. None after SPOS_MAX: panics.
    pub fn successor(self) -> Self {
        let (inode, offset, snapshot) = (self.inode, self.offset, self.snapshot);

        if let Some(snapshot) = snapshot.checked_add(1) {
            spos(inode, offset, snapshot)
        } else if let Some(offset) = offset.checked_add(1) {
            spos(inode, offset, 0)
        } else {
            spos(inode.checked_add(1).expect("no position after SPOS_MAX"), 0, 0)
        }
    }

    /// The next position ignoring the snapshot field: past every snapshot
    /// version of this one. Panics after POS_MAX.
    pub fn nosnap_successor(self) -> Self {
        let (inode, offset) = (self.inode, self.offset);

        if let Some(offset) = offset.checked_add(1) {
            spos(inode, offset, 0)
        } else {
            spos(inode.checked_add(1).expect("no position after POS_MAX"), 0, 0)
        }
    }

    /// The previous position, snapshot field first. None before POS_MIN:
    /// panics.
    pub fn predecessor(self) -> Self {
        let (inode, offset, snapshot) = (self.inode, self.offset, self.snapshot);

        if let Some(snapshot) = snapshot.checked_sub(1) {
            spos(inode, offset, snapshot)
        } else if let Some(offset) = offset.checked_sub(1) {
            spos(inode, offset, u32::MAX)
        } else {
            spos(inode.checked_sub(1).expect("no position before POS_MIN"), u64::MAX, u32::MAX)
        }
    }
}

/// A bpos field that holds a u64 (inode, offset). "U64_MAX" and "U32_MAX" are
/// accepted, as positions are printed.
#[cfg(feature = "std")]
fn parse_bpos_u64(s: &str) -> Result<u64, BchError> {
    match s {
        "U64_MAX" => Ok(u64::MAX),
        "U32_MAX" => Ok(u32::MAX as u64),
        _        => s.parse().map_err(|_| BchError::from(bch_errcode::BCH_ERR_EINVAL_parse_bpos)),
    }
}

/// Same for the snapshot field (u32).
#[cfg(feature = "std")]
fn parse_bpos_u32(s: &str) -> Result<u32, BchError> {
    match s {
        "U32_MAX" => Ok(u32::MAX),
        _        => s.parse().map_err(|_| BchError::from(bch_errcode::BCH_ERR_EINVAL_parse_bpos)),
    }
}

#[cfg(feature = "std")]
impl FromStr for c::bpos {
    type Err = BchError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "POS_MIN" {
            return Ok(POS_MIN);
        }

        if s == "POS_MAX" {
            return Ok(POS_MAX);
        }

        if s == "SPOS_MAX" {
            return Ok(SPOS_MAX);
        }

        let err = || BchError::from(bch_errcode::BCH_ERR_EINVAL_parse_bpos);

        let mut fields = s.split(':');
        let ino_str = fields.next().ok_or_else(err)?;
        let off_str = fields.next().ok_or_else(err)?;
        let snp_str = fields.next();

        let ino: u64 = parse_bpos_u64(ino_str)?;
        let off: u64 = parse_bpos_u64(off_str)?;
        let snp: u32 = snp_str
            .map(parse_bpos_u32)
            .transpose()?
            .unwrap_or(0);

        Ok(c::bpos {
            inode:    ino,
            offset:   off,
            snapshot: snp,
        })
    }
}

pub type bkey_type = c::bch_bkey_type;

/// A key and its value, borrowed: laid out as C's struct bkey_s_c, so a C entry
/// point can take one by value. typed() switches on k.type_, each arm the key
/// as its type's, in place - BkeySC<'a, Extent<Validated>>,
/// BkeySCExtent<'a, Validated> - whose value is v().
/// @v is C's thin pointer to the value; @V is only its type, and v() makes the
/// reference, sized or not.
#[repr(C)]
pub struct BkeySC<'a, V: ?Sized = c::bch_val> {
    pub k:           &'a c::bkey,
    pub(crate) v:    *const c::bch_val,
    pub(crate) iter: PhantomData<&'a mut BtreeIter<'a>>,
    pub(crate) val:  PhantomData<*const V>,
}

// Copy whatever V is: a derive would require V: Copy.
impl<V: ?Sized> Clone for BkeySC<'_, V> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<V: ?Sized> Copy for BkeySC<'_, V> {}

/// A key with its value inline: C's struct bkey_i_<type> for value type V,
/// sized or a DST. typed() and arm() hand one out by its type; its length is
/// validate's.
///
/// BkeyI, V = [u64] as long as k.u64s says, is a key of any type, all of it -
/// where C's bkey_i is only the header. It's made from a pointer that reaches
/// the value (from_raw()), never widened from a reference to the header.
///
/// Not Clone or Copy: the value can run past V - copying a key is bkey_copy().
#[derive(Default)]
#[repr(C)]
pub struct BkeyI<V: ?Sized = [u64]> {
    pub k: c::bkey,
    pub v: V,
}

impl BkeyI {
    /// Where a thin key - C's, or an owner's buffer - becomes a reference to
    /// all of one, as long as its u64s say.
    ///
    /// # Safety
    /// @p is a key, its u64s right and all of them there, and it may reach
    /// them - not a pointer widened from a reference to the header.
    pub unsafe fn from_raw(p: *const c::bkey_i) -> *const Self {
        // SAFETY: the caller's - its header is there
        let n = unsafe { (*p).k.u64s as usize }.saturating_sub(BKEY_U64S);
        core::ptr::slice_from_raw_parts(p.cast::<u64>(), n) as *const Self
    }

    /// from_raw(), to edit.
    ///
    /// # Safety
    /// As from_raw()'s.
    pub unsafe fn from_raw_mut(p: *mut c::bkey_i) -> *mut Self {
        // SAFETY: as from_raw()'s
        let n = unsafe { (*p).k.u64s as usize }.saturating_sub(BKEY_U64S);
        core::ptr::slice_from_raw_parts_mut(p.cast::<u64>(), n) as *mut Self
    }

    /// Thin, for C.
    pub fn as_ffi(&self) -> *const c::bkey_i {
        (self as *const Self).cast()
    }

    /// For C, to edit.
    pub fn as_ffi_mut(&mut self) -> *mut c::bkey_i {
        (self as *mut Self).cast()
    }
}

/// From a walk over keys in bytes, which checked each one's u64s against its
/// bytes.
impl<'a> crate::util::vstructs::UnvalidatedRef<'a, c::bkey_i> {
    pub fn key(&self) -> &'a BkeyI {
        // SAFETY: the walk checked its u64s against its bytes, all of which
        // as_bytes() is
        unsafe { &*BkeyI::from_raw(self.as_bytes().as_ptr().cast()) }
    }
}

/// key(), mutably: a journal entry's walk.
impl crate::util::vstructs::UnvalidatedMut<'_, c::bkey_i> {
    pub fn key_mut(&mut self) -> &mut BkeyI {
        // SAFETY: the walk checked its u64s against its bytes, which
        // as_mut_ptr() reaches all of
        unsafe { &mut *BkeyI::from_raw_mut(self.as_mut_ptr()) }
    }
}

impl<V: ?Sized> BkeyI<V> {
    /// As a key of any type, all of it.
    pub fn k_i(&self) -> &BkeyI {
        // SAFETY: a key, as long as its u64s say
        unsafe { &*BkeyI::from_raw((self as *const Self).cast()) }
    }

    /// k_i(), to edit.
    pub fn k_i_mut(&mut self) -> &mut BkeyI {
        // SAFETY: as k_i()'s
        unsafe { &mut *BkeyI::from_raw_mut((self as *mut Self).cast()) }
    }

    pub fn v(&self) -> &V {
        &self.v
    }

    pub fn v_mut(&mut self) -> &mut V {
        &mut self.v
    }
}

const _: () = {
    use core::mem::{align_of, offset_of, size_of};
    assert!(size_of::<BkeySC<'static>>() == size_of::<c::bkey_s_c>());
    assert!(align_of::<BkeySC<'static>>() == align_of::<c::bkey_s_c>());
    assert!(offset_of!(BkeySC<'static>, k) == offset_of!(c::bkey_s_c, k));
    assert!(offset_of!(BkeySC<'static>, v) == offset_of!(c::bkey_s_c, v));
};

/// A key type, named by its value type, bch_<name> - a family's struct, or a
/// plain type: what code generic over key types goes by. A key of the type,
/// read from the btree, has the value in place: InPlace.
pub trait TypedBkey {
    /// The value as a key read in place has it, as long as k.u64s says: a
    /// family's X<Validated> - a grown type's floor and the bytes there are,
    /// or a header and its tail - or a plain type itself.
    type InPlace: ?Sized + ArmRef;

    const TYPE: c::bch_bkey_type;

    /// The shortest a valid value is: the struct up to BCH_BKEY_TYPES()'
    /// field, the first one a value may end before, or all of it for `_` - a
    /// DST's header. One that long but shorter than the struct is an older
    /// version; a shorter one is damage, which validate rejects as
    /// bkey_val_size_too_small.
    const MIN_VAL_SIZE: usize;

    /// How long a value is with all of it there: the struct's size - a DST's
    /// header.
    const WHOLE_VAL_SIZE: usize;

    /// The key at @k, in place - a thin pointer for a sized value, fat for a
    /// DST, as long as k.u64s says.
    ///
    /// # Safety
    /// @k is a key of this type, its length validate's.
    unsafe fn cast(k: *const c::bkey_i) -> *const BkeyI<Self::InPlace>;

    /// cast(), to edit.
    ///
    /// # Safety
    /// As cast()'s.
    unsafe fn cast_mut(k: *mut c::bkey_i) -> *mut BkeyI<Self::InPlace>;
}

/// A key type whose value can be had whole, by value - made, or copied: a
/// plain type, or a grown type's struct, the fields an older value lacks
/// zeroed. Not a type with a tail. A key of it by value is BkeyI<Self>, C's
/// bkey_i_<name>: BkeyI::new(), BkeyI::from_key().
///
/// # Safety
/// Any bytes, all zeroes among them, are a valid Self in each of its fields -
/// padding aside, which may hold anything: val_copy_pad() copies the key's
/// bytes over a zeroed one.
pub unsafe trait SizedBkey: Sized + Default + TypedBkey {
    /// @k's value if it's this type, zero padded past the end of a short
    /// one.
    fn val_copy_pad(k: BkeySC<'_>) -> Option<Self> {
        // SAFETY: the value type of this key's type, by the type check -
        // whose fields any bytes are, as SizedBkey's impl says
        k.k.is::<Self>().then(|| unsafe { k.val_copy_pad() })
    }
}

impl<V: SizedBkey> BkeyI<V> {
    /// At POS_MIN, its value zeroed.
    pub fn new() -> Self {
        let mut k = c::bkey::new();
        k.type_ = V::TYPE.0 as u8;
        k.set_val_bytes(size_of::<V>());
        BkeyI { k, v: V::default() }
    }

    /// @k, if it's of type @V, as an owned key: the value zero padded, u64s
    /// as on disk.
    pub fn from_key(k: BkeySC<'_>) -> Option<Self> {
        Some(BkeyI { k: *k.k, v: V::val_copy_pad(k)? })
    }
}

/// A value as a key is read with it, BkeySC<'_, Self>, and what a mutable copy
/// of the key is: TransAttempt::bkey_make_mut_noupdate() makes a
/// TransBkey<Self::Mut> of one, grown to MIN_BYTES. Any key's copy is any
/// key's, as long as it is; a plain type's is itself, which a valid key has
/// all of; a grown type's, read as X<Validated>, is X, the fields an older
/// value lacks zeroed.
///
/// # Safety
/// Mut is bch_val, or a SizedBkey whose key is MIN_BYTES: TransBkey<Mut>'s
/// value is over that many.
pub unsafe trait MutBkey {
    type Mut: ?Sized;
    const MIN_BYTES: usize;
}

// SAFETY: any key, no longer than it is
unsafe impl MutBkey for c::bch_val {
    type Mut = c::bch_val;
    const MIN_BYTES: usize = 0;
}

/// Per value type, by its BCH_BKEY_TYPES() field column: `_` if every valid
/// value is whole, else the first field a valid one may end before. A sized
/// type's ArmRef is &T, whole. A DST is a family's (util/vstructs.rs), listed
/// here: #[whole] - with a tail, or whole - or #[grown], which SizedBkey can
/// have by value too. A type with a field in the column that isn't listed
/// doesn't build: it grew, so it's a #[grown] family.
macro_rules! bkey_val {
    (inline_data, $f:tt)          => { bkey_val!(@family inline_data, $f); };
    (indirect_inline_data, $f:tt) => { bkey_val!(@family indirect_inline_data, $f); };
    (xattr, $f:tt)                => { bkey_val!(@family xattr, $f); };
    (alloc, $f:tt)                => { bkey_val!(@family alloc, $f); };
    (alloc_v2, $f:tt)             => { bkey_val!(@family alloc_v2, $f); };
    (alloc_v3, $f:tt)             => { bkey_val!(@family alloc_v3, $f); };
    (inode, $f:tt)                => { bkey_val!(@family inode, $f); };
    (inode_v2, $f:tt)             => { bkey_val!(@family inode_v2, $f); };
    (inode_v3, $f:tt)             => { bkey_val!(@family inode_v3, $f); };
    (accounting, $f:tt)           => { bkey_val!(@family accounting, $f); };
    (damage, $f:tt)               => { bkey_val!(@family damage, $f); };
    (btree_ptr, $f:tt)            => { bkey_val!(@family btree_ptr, $f); };
    (btree_ptr_v2, $f:tt)         => { bkey_val!(@family btree_ptr_v2, $f); };
    (extent, $f:tt)               => { bkey_val!(@family extent, $f); };
    (reflink_v, $f:tt)            => { bkey_val!(@family reflink_v, $f); };
    (stripe, $f:tt)               => { bkey_val!(@family stripe, $f); };

    (error, $f:tt)                => { bkey_val!(@grown error, $f); };
    (subvolume, $f:tt)            => { bkey_val!(@grown subvolume, $f); };
    (snapshot, $f:tt)             => { bkey_val!(@grown snapshot, $f); };
    (alloc_v4, $f:tt)             => { bkey_val!(@grown alloc_v4, $f); };
    (logged_op_finsert, $f:tt)    => { bkey_val!(@grown logged_op_finsert, $f); };

    // Its field column says gens, but every valid one is whole:
    // bch2_bucket_gens_validate() takes no other length
    (bucket_gens, $f:tt)          => { bkey_val!(@sized bucket_gens); };

    ($name:ident, _)              => { bkey_val!(@sized $name); };

    // A family's: the key types in every state, named - BkeyISubvolume<Whole>
    (@family $name:ident, $field:tt) => { crate::paste! {
        pub type [<BkeyI $name:camel>]<S> = BkeyI<c::[<$name:camel>]<S>>;
        pub type [<BkeySC $name:camel>]<'a, S> = BkeySC<'a, c::[<$name:camel>]<S>>;
        pub type [<BkeyS $name:camel>]<'a, S> = BkeyS<'a, c::[<$name:camel>]<S>>;

        impl TypedBkey for c::[<$name:camel>] {
            type InPlace = c::[<$name:camel>]<crate::util::vstructs::Validated>;
            const TYPE: c::bch_bkey_type = c::bch_bkey_type::[<KEY_TYPE_ $name>];
            // its family's head: C's struct, or a grown type's floor
            const MIN_VAL_SIZE: usize = size_of::<<Self::InPlace as Unsized>::Head>();
            const WHOLE_VAL_SIZE: usize = size_of::<Self>();

            unsafe fn cast(k: *const c::bkey_i) -> *const BkeyI<Self::InPlace> {
                // SAFETY: the caller's: its value, after its header, as long
                // as k.u64s says
                unsafe {
                    let len = (*k).k.val_bytes();
                    let v = <Self::InPlace as Unsized>::from_raw(k.cast::<u8>().add(size_of::<c::bkey>()), len);
                    v.byte_sub(size_of::<c::bkey>()) as *const BkeyI<Self::InPlace>
                }
            }

            unsafe fn cast_mut(k: *mut c::bkey_i) -> *mut BkeyI<Self::InPlace> {
                // SAFETY: as cast()'s
                unsafe {
                    let len = (*k).k.val_bytes();
                    let v = <Self::InPlace as Unsized>::from_raw_mut(k.cast::<u8>().add(size_of::<c::bkey>()), len);
                    v.byte_sub(size_of::<c::bkey>()) as *mut BkeyI<Self::InPlace>
                }
            }
        }

        const _: () = assert!(<c::[<$name:camel>] as TypedBkey>::MIN_VAL_SIZE ==
                              bkey_val!(@min_repr c::[<$name:camel>], $field),
                              concat!("bch_", stringify!($name), ": its family's head isn't the \
                                       minimum BCH_BKEY_TYPES() names"));
    } };

    // A grown type's struct can be had by value: an older value zero padded
    (@grown $name:ident, $field:tt) => { crate::paste! {
        bkey_val!(@family $name, $field);
        // SAFETY: Plain, as #[grown] requires: no padding, any bytes a value
        unsafe impl SizedBkey for c::[<$name:camel>] {}

        // SAFETY: a SizedBkey, its key whole
        unsafe impl MutBkey for c::[<$name:camel>]<crate::util::vstructs::Validated> {
            type Mut = c::[<$name:camel>];
            const MIN_BYTES: usize = size_of::<BkeyI<c::[<$name:camel>]>>();
        }
    } };

    (@sized $name:ident) => { crate::paste! {
        pub type [<BkeyI $name:camel>] = BkeyI<c::[<$name:camel>]>;
        pub type [<BkeySC $name:camel>]<'a> = BkeySC<'a, c::[<$name:camel>]>;
        pub type [<BkeyS $name:camel>]<'a> = BkeyS<'a, c::[<$name:camel>]>;

        impl TypedBkey for c::[<$name:camel>] {
            type InPlace = Self;
            const TYPE: c::bch_bkey_type = c::bch_bkey_type::[<KEY_TYPE_ $name>];
            const MIN_VAL_SIZE: usize = size_of::<Self>();
            const WHOLE_VAL_SIZE: usize = size_of::<Self>();

            unsafe fn cast(k: *const c::bkey_i) -> *const BkeyI<Self> {
                k.cast()
            }

            unsafe fn cast_mut(k: *mut c::bkey_i) -> *mut BkeyI<Self> {
                k.cast()
            }
        }

        // SAFETY: an on-disk value - integers, and structs, arrays and unions
        // of them - any bytes of which are one
        unsafe impl SizedBkey for c::[<$name:camel>] {}

        // SAFETY: a SizedBkey, its key whole
        unsafe impl MutBkey for c::[<$name:camel>] {
            type Mut = Self;
            const MIN_BYTES: usize = size_of::<BkeyI<Self>>();
        }

        // Whole, &T - and the Floored Min a View derefs to. A value is at the
        // u64 after its key's header, so it's aligned.
        impl ArmRef for c::[<$name:camel>] {
            fn arm_ref(b: &[u8]) -> Option<&Self> {
                // SAFETY: all of it, aligned
                (b.len() >= size_of::<Self>() && b.as_ptr() as usize % align_of::<Self>() == 0)
                    .then(|| unsafe { &*b.as_ptr().cast() })
            }

            fn arm_mut(b: &mut [u8]) -> Option<&mut Self> {
                // SAFETY: as arm_ref()'s
                (b.len() >= size_of::<Self>() && b.as_ptr() as usize % align_of::<Self>() == 0)
                    .then(|| unsafe { &mut *b.as_mut_ptr().cast() })
            }
        }

        unsafe impl Floored for c::[<$name:camel>] {
            type Min = Self;
        }
    } };


    // BCH_BKEY_TYPES()' field column as a length, as C's
    // bch2_bkey_min_val_sizes[] has it: @val up to @field, or all of it
    (@min $val:ty, _)            => { size_of::<$val>() };
    (@min $val:ty, $field:ident) => { core::mem::offset_of!($val, $field) };
    // a family's: its fields are its repr's
    (@min_repr $val:ty, _)            => { size_of::<$val>() };
    (@min_repr $val:ty, $field:ident) => { core::mem::offset_of!($val, repr.$field) };
}

/// Everything defined per key type, from BCH_BKEY_TYPES(): (name, KEY_TYPE
/// number, ...).
macro_rules! bkey_types {
    ($(($name:ident, $nr:literal, $flags:expr, $field:tt $(, $($rest:tt)*)?)),* $(,)?) => { crate::paste! {
        $(
        bkey_val!($name, $field);
        )*

        /// A key, by its type: BkeySC::typed().
        #[allow(non_camel_case_types)]
        #[derive(Clone, Copy)]
        pub enum BkeySCTyped<'a> {
            $($name(BkeySC<'a, <c::[<$name:camel>] as TypedBkey>::InPlace>),)*
            /// A type this code doesn't know.
            unknown(BkeySC<'a>),
        }

        impl<'a> BkeySC<'a> {
            pub fn typed(self) -> BkeySCTyped<'a> {
                match self.k.type_ as u32 {
                    $($nr => BkeySCTyped::$name(self.cast()),)*
                    _ => BkeySCTyped::unknown(self),
                }
            }

        }

        /// BkeySCTyped, to edit: BkeyS::typed().
        #[allow(non_camel_case_types)]
        pub enum BkeySTyped<'a> {
            $($name(BkeyS<'a, <c::[<$name:camel>] as TypedBkey>::InPlace>),)*
            /// A type this code doesn't know.
            unknown(BkeyS<'a>),
        }

        impl BkeyS<'_> {
            pub fn typed(&mut self) -> BkeySTyped<'_> {
                let k = self.reborrow();
                match k.k.type_ as u32 {
                    $($nr => BkeySTyped::$name(k.cast()),)*
                    _ => BkeySTyped::unknown(k),
                }
            }
        }

        /// A key with its value inline, by its type, each arm as long as its
        /// u64s say: BkeyI::typed().
        #[allow(non_camel_case_types)]
        pub enum BkeyITyped<'a> {
            $($name(&'a BkeyI<<c::[<$name:camel>] as TypedBkey>::InPlace>),)*
            /// A type this code doesn't know.
            unknown(&'a BkeyI),
        }

        /// BkeyITyped, to edit.
        #[allow(non_camel_case_types)]
        pub enum BkeyITypedMut<'a> {
            $($name(&'a mut BkeyI<<c::[<$name:camel>] as TypedBkey>::InPlace>),)*
            /// A type this code doesn't know.
            unknown(&'a mut BkeyI),
        }

        impl BkeyI {
            pub fn typed(&self) -> BkeyITyped<'_> {
                match self.k.type_ as u32 {
                    $($nr =>
                        // SAFETY: as arm()'s
                        BkeyITyped::$name(unsafe { &*<c::[<$name:camel>] as TypedBkey>::cast(self.as_ffi()) }),)*
                    _ => BkeyITyped::unknown(self),
                }
            }

            /// typed(), to edit.
            pub fn typed_mut(&mut self) -> BkeyITypedMut<'_> {
                match self.k.type_ as u32 {
                    $($nr =>
                        // SAFETY: as arm()'s
                        BkeyITypedMut::$name(unsafe { &mut *<c::[<$name:camel>] as TypedBkey>::cast_mut(self.as_ffi_mut()) }),)*
                    _ => BkeyITypedMut::unknown(self),
                }
            }
        }
    }};
}

c::BCH_BKEY_TYPES!(bkey_types);

impl<'a, V: ?Sized> BkeySC<'a, V> {
    /// The value as bytes, as long as k.u64s says.
    pub fn val_bytes(&self) -> &'a [u8] {
        // SAFETY: its value follows its header, as long as u64s says, for 'a
        unsafe { core::slice::from_raw_parts(self.v.cast::<u8>(), self.k.val_bytes()) }
    }

    /// The key as one with a @W value: typed() and arm(), which check it is.
    fn cast<W: ?Sized>(self) -> BkeySC<'a, W> {
        BkeySC { k: self.k, v: self.v, iter: PhantomData, val: PhantomData }
    }

    /// Of any type: C's .s_c.
    pub fn s_c(self) -> BkeySC<'a> {
        self.cast()
    }

    pub fn to_text<'f>(&self, fs: &'f Fs) -> BkeySCToText<'a, 'f> {
        BkeySCToText { k: self.s_c(), fs }
    }

    /// The key as C's typed key, bkey_s_c_<type>, for C functions that take
    /// one: the same key, its lifetime C's - none. Crate-private: it's to be
    /// handed to C within 'a, not kept.
    pub(crate) fn to_c(self) -> BkeySC<'static, V> {
        // SAFETY: a C typed key's pointers have no lifetime, and this one is
        // for C, within 'a
        BkeySC { k: unsafe { &*(self.k as *const c::bkey) }, v: self.v, iter: PhantomData, val: PhantomData }
    }
}

// C's typed keys - from a C lookup, say - as a plain bkey_s_c, for
// bkey_s_c_to_result(): the same key and value.
impl<V: ?Sized> From<BkeySC<'static, V>> for c::bkey_s_c {
    fn from(k: BkeySC<'static, V>) -> Self {
        c::bkey_s_c { k: k.k as *const c::bkey as *mut c::bkey, v: k.v.cast_mut() }
    }
}

impl<'a, V: ?Sized + ArmRef> BkeySC<'a, V> {
    /// As its type's ArmRef has it: &T for a sized type, whose valid values are
    /// all whole; else its family's DST, as long as k.u64s says. A value
    /// shorter than validate passes is a bug.
    pub fn v(&self) -> &'a V {
        V::arm_ref(self.val_bytes())
            .unwrap_or_else(|| panic!("{}: {} bytes, shorter than validate passes", self.k,
                                      self.val_bytes().len()))
    }
}

impl<'a> BkeySC<'a> {
    /// The key as @K's, if it's that type.
    pub fn arm<K: TypedBkey + ?Sized>(self) -> Option<BkeySC<'a, K::InPlace>> {
        self.k.is::<K>().then(|| self.cast())
    }

    /// The key as C's bkey_s_c, for passing to C.
    pub(crate) fn to_raw(self) -> c::bkey_s_c {
        c::bkey_s_c {
            k: self.k as *const c::bkey as *mut c::bkey,
            v: self.v.cast_mut(),
        }
    }

    /// Copied into a @T, zero padded past what the key has: for a value that
    /// may predate fields @T has, which a reference would read past the end of.
    /// To read one in place, arm() then v().
    ///
    /// # Safety
    /// @T is the value type of this key's type: plain data, all-zeroes valid.
    pub(crate) unsafe fn val_copy_pad<T>(&self) -> T {
        let mut v = core::mem::MaybeUninit::<T>::zeroed();
        let n = self.k.val_bytes().min(size_of::<T>());
        // SAFETY: @n of the value's bytes, which it has, into a zeroed T -
        // which the caller says any bytes are
        unsafe {
            core::ptr::copy_nonoverlapping(self.v.cast::<u8>(), v.as_mut_ptr().cast::<u8>(), n);
            v.assume_init()
        }
    }

    /// Where in the value its extent entries - its pointers - are. Empty for a
    /// key type without them.
    fn extent_entry_range(&self) -> core::ops::Range<usize> {
        use core::mem::{offset_of, size_of};

        let len = self.val_bytes().len();
        match self.typed() {
            BkeySCTyped::btree_ptr(_)    => offset_of!(c::BtreePtr, repr.start)..len,
            BkeySCTyped::extent(_)       => offset_of!(c::Extent, repr.start)..len,
            BkeySCTyped::reflink_v(_)    => offset_of!(c::ReflinkV, repr.start)..len,
            BkeySCTyped::btree_ptr_v2(_) => offset_of!(c::BtreePtrV2, repr.start)..len,
            BkeySCTyped::stripe(s) => {
                let start = offset_of!(c::Stripe, repr.ptrs);
                start..(start + s.v().nr_blocks as usize * size_of::<c::bch_extent_ptr>()).min(len)
            }
            _ => 0..0,
        }
    }

    /// Empty for a key type without extent entries, or a value too short to
    /// hold them.
    pub fn extent_entry_bytes(&self) -> &'a [u8] {
        self.val_bytes().get(self.extent_entry_range()).unwrap_or_default()
    }

    pub fn key_type(&self) -> c::bch_bkey_type {
        c::bch_bkey_type(self.k.type_ as u32)
    }

    pub fn is_deleted(&self) -> bool {
        self.k.is_deleted()
    }

    pub fn is_btree_ptr(&self) -> bool {
        self.k.is_btree_ptr()
    }

    pub fn start_pos(&self) -> c::bpos {
        self.k.start_pos()
    }

    pub fn start_offset(&self) -> u64 {
        self.k.start_offset()
    }
}

/// Editing a key's value: it's only as long as its u64s say - a key copied from
/// one written before its type grew is shorter than its type, and a field past
/// its end is one the key won't carry when it's written. Grow it first:
/// bkey_make_mut() or bkey_get_mut() with the typed key's size.
impl BkeyI {
    /// The key as @K's, if it's that type.
    pub fn arm<K: TypedBkey + ?Sized>(&self) -> Option<&BkeyI<K::InPlace>> {
        // SAFETY: a K, its length validate's, as in C; a key's value is at
        // the u64 after its header, aligned
        self.k.is::<K>().then(|| unsafe { &*K::cast(self.as_ffi()) })
    }

    /// arm(), to edit.
    pub fn arm_mut<K: TypedBkey + ?Sized>(&mut self) -> Option<&mut BkeyI<K::InPlace>> {
        // SAFETY: as arm()'s
        self.k.is::<K>().then(|| unsafe { &mut *K::cast_mut(self.as_ffi_mut()) })
    }

    /// The key as @K's C struct, whole, if it's that type: for a grown type,
    /// whose arm() is the value only as long as it is. Growing the key to its
    /// struct first is the caller's, as above.
    pub fn whole<K: SizedBkey>(&self) -> Option<&BkeyI<K>> {
        // SAFETY: a K, grown to its struct by the caller, as C's; a key's
        // value is at the u64 after its header, aligned
        self.k.is::<K>().then(|| unsafe { &*self.as_ffi().cast() })
    }

    /// whole(), to edit.
    pub fn whole_mut<K: SizedBkey>(&mut self) -> Option<&mut BkeyI<K>> {
        // SAFETY: as whole()'s
        self.k.is::<K>().then(|| unsafe { &mut *self.as_ffi_mut().cast() })
    }
}

impl<'a> From<&'a BkeyI> for BkeySC<'a> {
    fn from(k: &'a BkeyI) -> Self {
        BkeySC {
            k:    &k.k,
            v:    k.v.as_ptr().cast(),
            iter: PhantomData,
            val:  PhantomData,
        }
    }
}

impl<'a> From<&'a c::bkey_s_c> for BkeySC<'a> {
    fn from(k: &'a c::bkey_s_c) -> Self {
        BkeySC {
            k:    unsafe { &*k.k },
            v:    k.v,
            iter: PhantomData,
            val:  PhantomData,
        }
    }
}

/// A key and its value, borrowed to edit: laid out as C's struct bkey_s. A
/// tagged union, as BkeySC is. Not Copy: editing borrows it, &mut BkeyS, so one
/// can be walked twice - a read, then a rewrite - without aliasing.
#[repr(C)]
pub struct BkeyS<'a, V: ?Sized = c::bch_val> {
    pub k:          &'a mut c::bkey,
    pub(crate) v:   *mut c::bch_val,
    pub(crate) val: PhantomData<*mut V>,
}

impl<'a, V: ?Sized> BkeyS<'a, V> {
    pub fn key_type(&self) -> c::bch_bkey_type {
        c::bch_bkey_type(self.k.type_ as u32)
    }

    /// The key as C's bkey_s, lent to C for as long as it's borrowed.
    pub(crate) fn as_raw(&mut self) -> c::bkey_s {
        c::bkey_s { split: c::bkey_s_split { kv: c::bkey_s_kv { k: &mut *self.k, v: self.v } } }
    }

    /// To read: a typed key's C .c.
    pub fn as_sc(&self) -> BkeySC<'_, V> {
        BkeySC { k: &*self.k, v: self.v.cast_const(), iter: PhantomData, val: PhantomData }
    }

    /// Of any type: C's .s.
    pub fn s(self) -> BkeyS<'a> {
        self.cast()
    }

    /// Of any type, to read: C's .s_c.
    pub fn s_c(&self) -> BkeySC<'_> {
        self.as_sc().s_c()
    }

    /// The value as bytes, as long as k.u64s says: it's never packed, so
    /// this is the value in place.
    pub fn val_bytes(&self) -> &[u8] {
        // SAFETY: its value follows its header, as long as u64s says
        unsafe { core::slice::from_raw_parts(self.v.cast::<u8>(), self.k.val_bytes()) }
    }

    /// val_bytes(), mutably.
    pub fn val_bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: as val_bytes()
        unsafe { core::slice::from_raw_parts_mut(self.v.cast::<u8>(), self.k.val_bytes()) }
    }

    /// The key, borrowed again: for typed() and arm(), which don't consume
    /// it.
    fn reborrow(&mut self) -> BkeyS<'_, V> {
        BkeyS { k: &mut *self.k, v: self.v, val: PhantomData }
    }

    /// The key as one with a @W value: typed() and arm(), which check it is.
    fn cast<W: ?Sized>(self) -> BkeyS<'a, W> {
        BkeyS { k: self.k, v: self.v, val: PhantomData }
    }
}

impl<V: ?Sized + ArmRef> BkeyS<'_, V> {
    pub fn v(&self) -> &V {
        let k: &c::bkey = self.k;
        V::arm_ref(self.val_bytes())
            .unwrap_or_else(|| panic!("{k}: shorter than validate passes"))
    }

    /// Its value, to edit: a field a grown type's older value lacks is a
    /// set() error.
    pub fn v_mut(&mut self) -> &mut V {
        let k = *self.k;
        V::arm_mut(self.val_bytes_mut())
            .unwrap_or_else(|| panic!("{k}: shorter than validate passes"))
    }
}

impl<'a> BkeyS<'a> {
    /// From a key and its value, apart.
    ///
    /// # Safety
    /// @v is @k's value, as long as k.u64s says, nothing else touching it for
    /// 'a.
    pub unsafe fn from_parts(k: &'a mut c::bkey, v: *mut c::bch_val) -> Self {
        BkeyS { k, v, val: PhantomData }
    }

    /// BkeySC::arm(), to edit.
    pub fn arm<K: TypedBkey + ?Sized>(&mut self) -> Option<BkeyS<'_, K::InPlace>> {
        self.k.is::<K>().then(|| self.reborrow().cast())
    }

    /// BkeySC::extent_entry_bytes(), to edit.
    pub fn extent_entry_bytes_mut(&mut self) -> &mut [u8] {
        let range = self.as_sc().extent_entry_range();
        self.val_bytes_mut().get_mut(range).unwrap_or_default()
    }
}

/// A key of any type, as one of any type.
impl<'a, V: ?Sized> From<&'a mut BkeyI<V>> for BkeyS<'a> {
    fn from(k: &'a mut BkeyI<V>) -> Self {
        BkeyS {
            k:   &mut k.k,
            v:   (&raw mut k.v).cast(),
            val: PhantomData,
        }
    }
}

pub struct BkeySCToText<'a, 'f> {
    k:  BkeySC<'a>,
    fs: &'f Fs,
}

impl fmt::Display for BkeySCToText<'_, '_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        unsafe {
            printbuf_to_formatter(f, |buf| {
                c::bch2_bkey_val_to_text(buf, self.fs.as_ptr(), self.k.to_raw())
            })
        }
    }
}

#[inline(always)]
pub fn bpos_lt(l: Bpos, r: Bpos) -> bool {
    if l.inode != r.inode {
        l.inode < r.inode
    } else if l.offset != r.offset {
        l.offset < r.offset
    } else {
        l.snapshot < r.snapshot
    }
}

#[inline(always)]
pub fn bpos_le(l: Bpos, r: Bpos) -> bool {
    if l.inode != r.inode {
        l.inode < r.inode
    } else if l.offset != r.offset {
        l.offset < r.offset
    } else {
        l.snapshot <= r.snapshot
    }
}

#[inline(always)]
pub fn bpos_gt(l: Bpos, r: Bpos) -> bool {
    bpos_lt(r, l)
}

#[inline(always)]
pub fn bpos_ge(l: Bpos, r: Bpos) -> bool {
    bpos_le(r, l)
}

#[inline(always)]
pub fn bpos_cmp(l: Bpos, r: Bpos) -> i32 {
    if l.inode != r.inode {
        if l.inode < r.inode { -1 } else { 1 }
    } else if l.offset != r.offset {
        if l.offset < r.offset { -1 } else { 1 }
    } else if l.snapshot != r.snapshot {
        if l.snapshot < r.snapshot { -1 } else { 1 }
    } else {
        0
    }
}

#[inline]
pub fn bpos_min(l: Bpos, r: Bpos) -> Bpos {
    if bpos_lt(l, r) { l } else { r }
}

#[inline]
pub fn bpos_max(l: Bpos, r: Bpos) -> Bpos {
    if bpos_gt(l, r) { l } else { r }
}

#[inline(always)]
pub fn bkey_eq(l: Bpos, r: Bpos) -> bool {
    l.inode == r.inode && l.offset == r.offset
}

#[inline(always)]
pub fn bkey_lt(l: Bpos, r: Bpos) -> bool {
    if l.inode != r.inode {
        l.inode < r.inode
    } else {
        l.offset < r.offset
    }
}

#[inline(always)]
pub fn bkey_le(l: Bpos, r: Bpos) -> bool {
    if l.inode != r.inode {
        l.inode < r.inode
    } else {
        l.offset <= r.offset
    }
}

#[inline(always)]
pub fn bkey_gt(l: Bpos, r: Bpos) -> bool {
    bkey_lt(r, l)
}

#[inline(always)]
pub fn bkey_ge(l: Bpos, r: Bpos) -> bool {
    bkey_le(r, l)
}

#[inline(always)]
pub fn bkey_cmp(l: Bpos, r: Bpos) -> i32 {
    if l.inode != r.inode {
        if l.inode < r.inode { -1 } else { 1 }
    } else if l.offset != r.offset {
        if l.offset < r.offset { -1 } else { 1 }
    } else {
        0
    }
}

/// Without its value.
impl fmt::Display for c::bkey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        printbuf_to_formatter(f, |buf| unsafe { c::bch2_bkey_to_text(buf, self) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Le32;

    /// A snapshot key from before depth, skip and btime - 24 bytes of its 64 -
    /// reads as a snapshot: the fields it has as they are, the rest zero, not
    /// what's in the buffer past it.
    #[test]
    fn bkey_short_value() {
        assert_eq!(c::Snapshot::MIN_VAL_SIZE, 24);

        let mut k = BkeyI::<c::Snapshot>::new();
        k.v.subvol = Le32::new(5);
        k.v.children = [Le32::new(3), Le32::new(4)];
        k.v.depth = Le32::new(7);
        k.k.set_val_bytes(24);

        let s = BkeySC::from(k.k_i()).arm::<c::Snapshot>().expect("a snapshot").v();
        assert_eq!((s.subvol.get(), s.children[0].get(), s.children[1].get()), (5, 3, 4));
        assert_eq!(s.depth().get(), 0);
        assert!(s.whole().is_none());
        assert!(BkeySC::from(k.k_i()).arm::<c::Subvolume>().is_none());
    }

    /// A key by its type: a grown type's value its family's, an older value
    /// of it read as one - inline too: dispatch is by type, as C's.
    #[test]
    fn bkey_typed_short_values() {
        let mut k = BkeyI::<c::Subvolume>::new();
        k.v.snapshot = Le32::new(5);
        k.k.set_val_bytes(16);
        match BkeySC::from(k.k_i()).typed() {
            BkeySCTyped::subvolume(s) =>
                assert_eq!((s.v().snapshot.get(), s.v().fs_path_parent().get()), (5, 0)),
            _ => panic!("a subvolume"),
        }
        match k.k_i().typed() {
            BkeyITyped::subvolume(s) =>
                assert_eq!((s.v.snapshot.get(), s.v.fs_path_parent().get()), (5, 0)),
            _ => panic!("a subvolume"),
        }

        let mut g = BkeyI::<c::InodeGeneration>::new();
        g.v.bi_generation = Le32::new(3);
        assert_eq!(BkeySC::from(g.k_i()).arm::<c::InodeGeneration>().map(|g| g.v().bi_generation.get()),
                   Some(3));
        assert!(matches!(g.k_i().typed(), BkeyITyped::inode_generation(_)));
    }

    /// A snapshot can't be had over fewer bytes than every snapshot has -
    /// its floor, 24 - rather than reading past them; over 24, it can.
    #[test]
    fn bkey_unfloored() {
        let b = [0u64; 3];
        // SAFETY: b's bytes, aligned for any value
        let b = unsafe { core::slice::from_raw_parts(b.as_ptr().cast::<u8>(), size_of_val(&b)) };
        use crate::util::vstructs::Validated;
        assert!(<c::Snapshot<Validated> as ArmRef>::arm_ref(&b[..16]).is_none());
        assert!(<c::Snapshot<Validated> as ArmRef>::arm_ref(&b[..24]).is_some());
    }

    /// A subvolume key from before fs_path_parent - 16 bytes of its 48 - edited
    /// in place has only the fields it has: one it lacks has no setter, a
    /// write the key wouldn't carry. One grown whole does.
    #[test]
    fn bkey_edit_short_value() {
        let mut k = BkeyI::<c::Subvolume>::new();
        k.k.set_val_bytes(16);
        let k = k.k_i_mut();

        assert!(k.arm_mut::<c::Snapshot>().is_none());

        let mut s = BkeyS::from(&mut *k);
        let BkeySTyped::subvolume(mut s) = s.typed() else { panic!("a subvolume") };
        s.v_mut().snapshot = Le32::new(5);

        let mut full = BkeyI::<c::Subvolume>::new();
        full.k_i_mut().whole_mut::<c::Subvolume>().expect("whole").v.fs_path_parent = Le32::new(9);
        assert_eq!(full.v.fs_path_parent, 9);
        let s = BkeySC::from(&*k).arm::<c::Subvolume>().expect("a subvolume").v();
        assert_eq!((s.snapshot.get(), s.fs_path_parent().get()), (5, 0));
    }

    #[test]
    fn bpos_successor_predecessor_carry() {
        let cases = [
            (spos(1, 2, 3),               spos(1, 2, 4)),
            (spos(1, 2, u32::MAX),        spos(1, 3, 0)),
            (spos(1, u64::MAX, u32::MAX), spos(2, 0, 0)),
            (POS_MIN,                     spos(0, 0, 1)),
        ];

        for (p, next) in cases {
            assert_eq!(p.successor(), next, "successor of {p}");
            assert_eq!(next.predecessor(), p, "predecessor of {next}");
        }
    }

    #[test]
    #[should_panic(expected = "no position after SPOS_MAX")]
    fn bpos_successor_of_max() {
        SPOS_MAX.successor();
    }

    #[test]
    #[should_panic(expected = "no position before POS_MIN")]
    fn bpos_predecessor_of_min() {
        POS_MIN.predecessor();
    }
}
