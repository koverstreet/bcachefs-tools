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
use crate::util::vstructs::{Floored, Plain, View, ViewMut};
use core::marker::PhantomData;

use c::bpos as Bpos;

use core::cmp::Ordering;

/// The u64s of a key's header, struct bkey: as C's BKEY_U64s.
pub const BKEY_U64S: usize = core::mem::size_of::<c::bkey>() / core::mem::size_of::<u64>();

pub trait AsBkeyI {
    fn as_bkey_i(&self) -> &c::bkey_i;
    fn as_bkey_i_mut(&mut self) -> &mut c::bkey_i;

    fn as_bkey(&self) -> &c::bkey {
        &self.as_bkey_i().k
    }

    fn as_bkey_mut(&mut self) -> &mut c::bkey {
        &mut self.as_bkey_i_mut().k
    }
}

impl AsBkeyI for c::bkey_i {
    fn as_bkey_i(&self) -> &c::bkey_i {
        self
    }

    fn as_bkey_i_mut(&mut self) -> &mut c::bkey_i {
        self
    }
}

impl c::bkey {
    /// A key with no value at POS_MIN, of type deleted: C's KEY(0, 0, 0), as
    /// bkey_init() sets.
    pub fn new() -> Self {
        Self {
            u64s:        BKEY_U64S as u8,
            format_bits: c::bkey_format_bits::new().with_format(c::KEY_FORMAT_CURRENT as u8),
            ..Default::default()
        }
    }

    /// u64s for a value of @bytes: C's set_bkey_val_bytes().
    pub fn set_val_bytes(&mut self, bytes: usize) {
        let u64s = BKEY_U64S + bytes.div_ceil(size_of::<u64>());
        self.u64s = u8::try_from(u64s)
            .unwrap_or_else(|_| panic!("key of {u64s} u64s: a key is at most {} u64s", u8::MAX));
    }

    pub fn pos(&self) -> c::bpos {
        self.p
    }

    pub fn set_pos(&mut self, pos: c::bpos) {
        self.p = pos;
    }

    pub fn set_snapshot(&mut self, snapshot: u32) {
        self.p.snapshot = snapshot;
    }

    pub fn size(&self) -> u32 {
        self.size
    }

    pub fn set_size(&mut self, size: u32) {
        self.size = size;
    }

    /// Make an extent @new_size long, from where it starts now - its end,
    /// its position, moves: as bch2_key_resize().
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
        bkey_start_pos(self)
    }

    pub fn start_offset(&self) -> u64 {
        bkey_start_offset(self)
    }

    pub fn is_deleted(&self) -> bool {
        bkey_deleted(self)
    }

    pub fn is_btree_ptr(&self) -> bool {
        bkey_is_btree_ptr(self)
    }
}

/// As Display: bch2_bpos_to_text().
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
    /// The next position, snapshot field first: as bpos_successor(). There's
    /// none after SPOS_MAX - panics, as C BUG()s.
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

    /// The previous position, snapshot field first: as bpos_predecessor().
    /// There's none before POS_MIN - panics, as C BUG()s.
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

/// Parse a bpos field that holds a u64 (inode, offset). Accepts the literal
/// tokens "U64_MAX" / "U32_MAX" as their respective sentinel values, matching
/// how positions are printed in dmesg / bcachefs_to_text output.
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

/// A key and its value, borrowed: C's struct bkey_s_c, which it's laid out
/// as - so a C entry point can take one by value where C passes a
/// bkey_s_c. With a value type, the typed one: BkeySC<'a, bch_extent> is
/// struct bkey_s_c_extent (btree/bkey_types.rs).
#[repr(C)]
pub struct BkeySC<'a, V = c::bch_val> {
    pub k:           &'a c::bkey,
    pub v:           &'a V,
    pub(crate) iter: PhantomData<&'a mut BtreeIter<'a>>,
}

// References: Copy whatever V is, which a derive would require it to be.
impl<V> Clone for BkeySC<'_, V> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<V> Copy for BkeySC<'_, V> {}

/// A key with its value inline, of type V: C's struct bkey_i_<type>, whose
/// k is also the bkey_i k_i - an empty value, there.
///
/// Not Clone or Copy: a key is its header and u64s of value, which can run
/// past V - copying one is bkey_copy().
#[derive(Default)]
#[repr(C)]
pub struct BkeyI<V> {
    pub k: c::bkey,
    pub v: V,
}

const _: () = {
    use core::mem::{align_of, offset_of, size_of};
    assert!(size_of::<BkeySC<'static>>() == size_of::<c::bkey_s_c>());
    assert!(align_of::<BkeySC<'static>>() == align_of::<c::bkey_s_c>());
    assert!(offset_of!(BkeySC<'static>, k) == offset_of!(c::bkey_s_c, k));
    assert!(offset_of!(BkeySC<'static>, v) == offset_of!(c::bkey_s_c, v));
};

/// A key type, named by its C typed key (bkey_i_<name>): its value type, and
/// typed access to it - for code generic over key types.
pub trait TypedBkey {
    /// The value: bch_<name>.
    type Val;

    const TYPE: c::bch_bkey_type;

    /// The shortest a valid value is: Val up to BCH_BKEY_TYPES()' field, the
    /// first one a value may end before, or all of it for `_`. One that long
    /// but shorter than Val is an older version; a shorter one is damage,
    /// which validate rejects as bkey_val_size_too_small.
    const MIN_VAL_SIZE: usize;

    /// A key of this type at POS_MIN, its value zeroed: C's
    /// bkey_<name>_init().
    fn new() -> Self where Self: Sized;

    /// @k's value if it's this type, zero padded past the end of a short
    /// one: see val_copy_pad().
    fn val_copy_pad(k: BkeySC<'_>) -> Option<Self::Val>;

    /// @k, if it's this type, as an owned key: the value zero padded, u64s as
    /// on disk - as bch2_bkey_get_i_typed() copies it.
    fn from_key(k: BkeySC<'_>) -> Option<Self> where Self: Sized;
}

/// BCH_BKEY_TYPES()' field column as a length, as C's bch2_bkey_min_val_sizes[]
/// has it: @val up to @field, or all of it for `_`.
macro_rules! min_val_size {
    ($val:ty, _)             => { size_of::<$val>() };
    ($val:ty, $field:ident)  => { core::mem::offset_of!($val, $field) };
}

/// And as what a View of it derefs to, Floored's Min: the value up to the
/// field, #[derive(View)]'s __bch_<name>_min!(), or all of it for `_`.
macro_rules! bkey_floored {
    ($name:ident, _) => { crate::paste! {
        unsafe impl Floored for c::[<bch_ $name>] {
            type Min = Self;
        }
    } };
    ($name:ident, $field:ident) => { crate::paste! {
        c::[<__bch_ $name _min>]!($field);
    } };
}

/// And what the typed dispatch enums hold of a value: all of it, if every
/// valid one is whole - `_` - else a View of it.
macro_rules! bkey_val_ref {
    ($a:lifetime, $val:ty, _)          => { &$a $val };
    ($a:lifetime, $val:ty, $f:ident)   => { View<$a, $val> };
}

macro_rules! bkey_val_mut {
    ($a:lifetime, $val:ty, _)          => { &$a mut $val };
    ($a:lifetime, $val:ty, $f:ident)   => { ViewMut<$a, $val> };
}

/// And that, from @v: a value at least MIN_VAL_SIZE long.
macro_rules! bkey_val_ref_new {
    // SAFETY: MIN_VAL_SIZE is all of it, for `_`; a value is at the u64
    // after its key's header, so it's aligned
    ($v:expr, _)        => { unsafe { &*$v.as_ptr().cast() } };
    ($v:expr, $f:ident) => { View::new($v) };
}

macro_rules! bkey_val_mut_new {
    // SAFETY: as bkey_val_ref_new!()'s
    ($v:expr, _)        => { unsafe { &mut *$v.as_mut_ptr().cast() } };
    ($v:expr, $f:ident) => { ViewMut::new($v) };
}

/// Everything defined per key type, from BCH_BKEY_TYPES(): (name, KEY_TYPE
/// number, ...).
macro_rules! bkey_types {
    ($(($name:ident, $nr:literal, $flags:expr, $field:tt $(, $($rest:tt)*)?)),* $(,)?) => { crate::paste! {
        $(
        pub type [<Bkey $name:camel>] = c::[<bkey_i_ $name>];

        impl c::[<bkey_i_ $name>] {
            pub fn k(&self) -> &c::bkey { &self.k }
            pub fn k_mut(&mut self) -> &mut c::bkey { &mut self.k }
            pub fn k_i(&self) -> &c::bkey_i { unsafe { &*(self as *const Self as *const c::bkey_i) } }
            pub fn k_i_mut(&mut self) -> &mut c::bkey_i { unsafe { &mut *(self as *mut Self as *mut c::bkey_i) } }
        }

        impl AsBkeyI for c::[<bkey_i_ $name>] {
            fn as_bkey_i(&self) -> &c::bkey_i { self.k_i() }
            fn as_bkey_i_mut(&mut self) -> &mut c::bkey_i { self.k_i_mut() }
        }

        impl TypedBkey for c::[<bkey_i_ $name>] {
            type Val = c::[<bch_ $name>];

            const TYPE: c::bch_bkey_type = c::bch_bkey_type::[<KEY_TYPE_ $name>];
            const MIN_VAL_SIZE: usize = min_val_size!(c::[<bch_ $name>], $field);

            fn new() -> Self {
                let mut k = c::bkey::new();
                k.type_ = $nr;
                k.set_val_bytes(size_of::<Self::Val>());
                Self { k, v: Default::default() }
            }

            fn val_copy_pad(k: BkeySC<'_>) -> Option<Self::Val> {
                (k.k.type_ == $nr).then(|| unsafe { k.val_copy_pad() })
            }

            fn from_key(k: BkeySC<'_>) -> Option<Self> {
                let v = Self::val_copy_pad(k)?;
                let mut r = Self::default();
                *r.k_mut() = *k.k;
                r.v = v;
                Some(r)
            }
        }

        bkey_floored!($name, $field);
        )*

        /// Typed dispatch for split-const bkey references: BkeySC::v().
        pub enum BkeyValSC<'a> {
            $($name(&'a c::bkey, bkey_val_ref!('a, c::[<bch_ $name>], $field)),)*
            /// A type this code doesn't know, or a value shorter than its
            /// type's ever been - damage, which validate rejects.
            unknown(&'a c::bkey, u8),
        }

        impl<'a> BkeySC<'a> {
            /// The value, by the key's type: whole, for a type every valid
            /// value of which is - else a View of it, as view().
            pub fn v(&self) -> BkeyValSC<'a> {
                let v = self.val_bytes();
                match self.k.type_ as u32 {
                    $($nr if v.len() >= <[<Bkey $name:camel>] as TypedBkey>::MIN_VAL_SIZE =>
                        BkeyValSC::$name(self.k, bkey_val_ref_new!(v, $field)),)*
                    _ => BkeyValSC::unknown(self.k, self.k.type_),
                }
            }

            $(
            /// The key as C's typed key, if it's that type, for C functions
            /// that take one.
            pub fn [<to_c_ $name>](&self) -> Option<c::[<bkey_s_c_ $name>]> {
                // the same key, the value as its type: C's typed key is this
                // with a 'static, as its pointers have no lifetime
                (self.k.type_ == $nr).then(|| unsafe {
                    core::mem::transmute::<BkeySC<'a>, c::[<bkey_s_c_ $name>]>(*self)
                })
            }
            )*
        }

        // C's typed keys - from a C lookup, say - as a plain bkey_s_c, for
        // bkey_s_c_to_result(): they're a union over the same key and value.
        $(
        impl From<c::[<bkey_s_c_ $name>]> for c::bkey_s_c {
            fn from(k: c::[<bkey_s_c_ $name>]) -> Self {
                c::bkey_s_c {
                    k: k.k as *const c::bkey as *mut c::bkey,
                    v: k.v as *const c::[<bch_ $name>] as *mut c::bch_val,
                }
            }
        }
        )*

        /// Typed dispatch for split-mutable bkey references: BkeyS::v_mut().
        pub enum BkeyValS<'a> {
            $($name(&'a mut c::bkey, bkey_val_mut!('a, c::[<bch_ $name>], $field)),)*
            /// As BkeyValSC's.
            unknown(&'a mut c::bkey, u8),
        }

        impl BkeyS<'_> {
            /// The value, by the key's type, mutably: as BkeySC::v(). A field
            /// a grown type's older value lacks is a set() error, as
            /// bkey_i::view_mut()'s.
            pub fn v_mut(&mut self) -> BkeyValS<'_> {
                let type_ = self.k.type_;
                let (k, v) = self.split_mut();
                match type_ as u32 {
                    $($nr if v.len() >= <[<Bkey $name:camel>] as TypedBkey>::MIN_VAL_SIZE =>
                        BkeyValS::$name(k, bkey_val_mut_new!(v, $field)),)*
                    _ => BkeyValS::unknown(k, type_),
                }
            }
        }
    }};
}

c::BCH_BKEY_TYPES!(bkey_types);

impl<'a> BkeySC<'a> {
    /// The key as C's bkey_s_c, for passing to C.
    pub(crate) fn to_raw(self) -> c::bkey_s_c {
        c::bkey_s_c {
            k: self.k as *const c::bkey as *mut c::bkey,
            v: self.v as *const c::bch_val as *mut c::bch_val,
        }
    }

    /// The value, copied into a @T and zero padded past what the key has: for
    /// reading a value that may predate fields @T has, which a reference to
    /// it would read past the end of - as C's bkey_val_copy_pad(). Typed
    /// wrappers check the key type, then call this. To read one in place,
    /// view().
    ///
    /// # Safety
    /// @T is the value type of this key's type: plain data, all-zeroes valid.
    pub(crate) unsafe fn val_copy_pad<T: Default>(&self) -> T {
        let mut v = T::default();
        let bytes = ((self.k.u64s as usize).saturating_sub(BKEY_U64S) * 8)
            .min(core::mem::size_of::<T>());
        unsafe {
            core::ptr::write_bytes(&mut v as *mut T as *mut u8, 0, core::mem::size_of::<T>());
            core::ptr::copy_nonoverlapping(self.v as *const c::bch_val as *const u8,
                                           &mut v as *mut T as *mut u8, bytes);
        }
        v
    }

    pub fn to_text<'f>(&self, fs: &'f Fs) -> BkeySCToText<'a, 'f> {
        BkeySCToText {
            k: BkeySC { k: self.k, v: self.v, iter: PhantomData },
            fs,
        }
    }

    /// Key only - type, pos, size - without rendering the value.
    pub fn to_text_key(&self) -> BkeySCKeyToText<'a> {
        BkeySCKeyToText {
            k: BkeySC { k: self.k, v: self.v, iter: PhantomData },
        }
    }

    /// The value as raw bytes: its full u64s extent past the key header.
    pub fn val_bytes(&self) -> &'a [u8] {
        let u64s = self.k.u64s as usize - core::mem::size_of::<c::bkey>() / 8;
        unsafe {
            core::slice::from_raw_parts(self.v as *const c::bch_val as *const u8, u64s * 8)
        }
    }

    /// The value as @K's, if the key is that type: a View of as many of its
    /// bytes as there are - one written before its type grew reads as zero
    /// past them. None if it's another type, or shorter than its type has
    /// ever been (MIN_VAL_SIZE): that's damage, not an older version.
    pub fn view<K: TypedBkey>(&self) -> Option<View<'a, K::Val>>
    where
        K::Val: Plain,
    {
        let v = self.val_bytes();
        (self.k.type_ as u32 == K::TYPE.0 && v.len() >= K::MIN_VAL_SIZE)
            .then(|| View::new(v))
    }

    /// The value as @K's, whole, if the key is that type: for a type every
    /// valid value of which is - BCH_BKEY_TYPES()' `_`, Floored by itself. A
    /// type that grew is view()'s: an older value of it is shorter. None for
    /// a shorter one here, which is damage.
    pub fn whole<K: TypedBkey>(&self) -> Option<&'a K::Val>
    where
        K::Val: Floored<Min = K::Val>,
    {
        self.whole_val::<K>()
    }

    /// whole(), for any type: for a key known to hold all of it.
    fn whole_val<K: TypedBkey>(&self) -> Option<&'a K::Val> {
        let v = self.val_bytes();
        (self.k.type_ as u32 == K::TYPE.0 && v.len() >= size_of::<K::Val>())
            // SAFETY: all of it is there, at the u64 after the header - aligned
            .then(|| unsafe { &*v.as_ptr().cast::<K::Val>() })
    }

    /// Where in the value its extent entries - its pointers - are: as
    /// bch2_bkey_ptrs_c(). Empty for a key type without them.
    fn extent_entry_range(&self) -> core::ops::Range<usize> {
        use core::mem::{offset_of, size_of};

        let len = self.val_bytes().len();
        match self.v() {
            BkeyValSC::btree_ptr(..)    => offset_of!(c::bch_btree_ptr, start)..len,
            BkeyValSC::extent(..)       => offset_of!(c::bch_extent, start)..len,
            BkeyValSC::reflink_v(..)    => offset_of!(c::bch_reflink_v, start)..len,
            BkeyValSC::btree_ptr_v2(..) => offset_of!(c::bch_btree_ptr_v2, start)..len,
            BkeyValSC::stripe(_, v) => {
                let start = offset_of!(c::bch_stripe, ptrs);
                start..(start + v.nr_blocks as usize * size_of::<c::bch_extent_ptr>()).min(len)
            }
            _ => 0..0,
        }
    }

    /// The value's extent entries - its pointers - as bytes: as
    /// bch2_bkey_ptrs_c(). Empty for a key type without them, or a value too
    /// short to hold them.
    pub fn extent_entry_bytes(&self) -> &'a [u8] {
        self.val_bytes().get(self.extent_entry_range()).unwrap_or_default()
    }

    pub fn pos(&self) -> c::bpos {
        self.k.p
    }

    pub fn size(&self) -> u32 {
        self.k.size
    }

    pub fn key_type(&self) -> c::bch_bkey_type {
        c::bch_bkey_type(self.k.type_ as u32)
    }

    pub fn is_deleted(&self) -> bool {
        self.key_type() == c::bch_bkey_type::KEY_TYPE_deleted
    }

    pub fn is_btree_ptr(&self) -> bool {
        matches!(
            self.key_type(),
            c::bch_bkey_type::KEY_TYPE_btree_ptr | c::bch_bkey_type::KEY_TYPE_btree_ptr_v2
        )
    }

    pub fn start_pos(&self) -> c::bpos {
        bkey_start_pos(self.k)
    }

    pub fn start_offset(&self) -> u64 {
        bkey_start_offset(self.k)
    }
}

/// Editing a key's value: it's only as long as its u64s say - a key copied
/// from one written before its type grew is shorter than its type, and a
/// field past its end is one the key won't carry when it's written. Grow it
/// first, as C's bch2_bkey_make_mut_typed() does: bkey_make_mut() or
/// bkey_get_mut() with the typed key's size.
impl c::bkey_i {
    /// The value's bytes, as many as its u64s say: an owned key's buffer
    /// holds them.
    fn val_bytes_mut(&mut self) -> &mut [u8] {
        let len = (self.k.u64s as usize).saturating_sub(BKEY_U64S) * size_of::<u64>();
        // SAFETY: its value follows its header, as long as u64s says
        unsafe { core::slice::from_raw_parts_mut((self as *mut Self).cast::<u8>().add(size_of::<c::bkey>()), len) }
    }

    /// The value as @K's, whole: as whole_mut(), to read.
    pub fn whole<K: TypedBkey>(&self) -> Option<&K::Val> {
        BkeySC::from(self).whole_val::<K>()
    }

    /// The value as @K's, whole, to edit: None unless the key is that type and
    /// holds all of it - one grown to its size, or made new.
    pub fn whole_mut<K: TypedBkey>(&mut self) -> Option<&mut K::Val> {
        let is_k = self.k.type_ as u32 == K::TYPE.0;
        let v = self.val_bytes_mut();
        (is_k && v.len() >= size_of::<K::Val>())
            // SAFETY: all of it is there, at the u64 after the header - aligned
            .then(|| unsafe { &mut *v.as_mut_ptr().cast::<K::Val>() })
    }

    /// The value as @K's, to edit field by field, if the key is that type and
    /// at least K::MIN_VAL_SIZE: a field past the value's end is a set()
    /// error, not a write the key wouldn't carry.
    pub fn view_mut<K: TypedBkey>(&mut self) -> Option<ViewMut<'_, K::Val>>
    where
        K::Val: Plain,
    {
        let is_k = self.k.type_ as u32 == K::TYPE.0;
        let v = self.val_bytes_mut();
        (is_k && v.len() >= K::MIN_VAL_SIZE).then(|| ViewMut::new(v))
    }
}

impl<'a> From<&'a c::bkey_i> for BkeySC<'a> {
    fn from(k: &'a c::bkey_i) -> Self {
        BkeySC {
            k:    &k.k,
            v:    &k.v,
            iter: PhantomData,
        }
    }
}

impl<'a> From<&'a c::bkey_s_c> for BkeySC<'a> {
    fn from(k: &'a c::bkey_s_c) -> Self {
        BkeySC {
            k:    unsafe { &*k.k },
            v:    unsafe { &*k.v },
            iter: PhantomData,
        }
    }
}

/// Mutable counterpart of [`BkeySC`]: a borrowed, unpacked key + value with the
/// lifetime tracked, so mutations land on the underlying buffer. It is a mutable
/// handle, so it is deliberately not `Copy`; the extent iterators borrow it
/// (`&mut BkeyS`), which lets a single handle be iterated more than once (e.g. a
/// read scan then a rewrite pass) without ever aliasing `&mut`.
///
/// Laid out as C's struct bkey_s - and with a value type, the typed one:
/// BkeyS<'a, bch_extent> is struct bkey_s_extent (btree/bkey_types.rs).
#[repr(C)]
pub struct BkeyS<'a, V = c::bch_val> {
    pub k: &'a mut c::bkey,
    pub v: &'a mut V,
}

impl<'a> BkeyS<'a> {
    pub fn key_type(&self) -> c::bch_bkey_type {
        c::bch_bkey_type(self.k.type_ as u32)
    }

    /// The key, to read: as bkey_s_to_s_c().
    pub fn as_sc(&self) -> BkeySC<'_> {
        BkeySC { k: &*self.k, v: &*self.v, iter: PhantomData }
    }

    /// The value bytes, mutably. Length is derived from the key's `u64s`; the
    /// value is never packed, so this is the on-disk value region in place.
    pub fn val_bytes_mut(&mut self) -> &mut [u8] {
        self.split_mut().1
    }

    /// The key and its value's bytes, mutably, apart: for v_mut()'s arms.
    fn split_mut(&mut self) -> (&mut c::bkey, &mut [u8]) {
        let len = self.k.u64s as usize * 8 - core::mem::size_of::<c::bkey>();
        // SAFETY: its value follows its header, as long as u64s says
        (&mut *self.k, unsafe { core::slice::from_raw_parts_mut(self.v as *mut c::bch_val as *mut u8, len) })
    }

    /// The value's extent entries as bytes, mutably: as
    /// BkeySC::extent_entry_bytes().
    pub fn extent_entry_bytes_mut(&mut self) -> &mut [u8] {
        let range = self.as_sc().extent_entry_range();
        self.val_bytes_mut().get_mut(range).unwrap_or_default()
    }
}

impl<'a> From<&'a mut c::bkey_i> for BkeyS<'a> {
    fn from(k: &'a mut c::bkey_i) -> Self {
        BkeyS {
            k: &mut k.k,
            v: &mut k.v,
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
                c::bch2_bkey_val_to_text(buf, self.fs.raw, self.k.to_raw())
            })
        }
    }
}

pub struct BkeySCKeyToText<'a> {
    k: BkeySC<'a>,
}

impl fmt::Display for BkeySCKeyToText<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        unsafe { printbuf_to_formatter(f, |buf| c::bch2_bkey_to_text(buf, self.k.k)) }
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

/// Start position of a bkey (p.offset - size).
pub fn bkey_start_pos(k: &c::bkey) -> c::bpos {
    c::bpos {
        inode: k.p.inode,
        offset: k.p.offset.wrapping_sub(k.size as u64),
        snapshot: k.p.snapshot,
    }
}

pub fn bkey_start_offset(k: &c::bkey) -> u64 {
    k.p.offset.wrapping_sub(k.size as u64)
}

pub fn bkey_deleted(k: &c::bkey) -> bool {
    c::bch_bkey_type(k.type_ as u32) == c::bch_bkey_type::KEY_TYPE_deleted
}

/// Whether @k is a tombstone in an extents btree - deleted, a whiteout or an
/// extent whiteout: as bkey_extent_whiteout().
pub fn bkey_extent_whiteout(k: &c::bkey) -> bool {
    matches!(c::bch_bkey_type(k.type_ as u32),
             c::bch_bkey_type::KEY_TYPE_deleted |
             c::bch_bkey_type::KEY_TYPE_whiteout |
             c::bch_bkey_type::KEY_TYPE_extent_whiteout)
}

/// The key, without its value: as bch2_bkey_to_text().
impl fmt::Display for c::bkey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        printbuf_to_formatter(f, |buf| unsafe { c::bch2_bkey_to_text(buf, self) })
    }
}

pub fn bkey_is_btree_ptr(k: &c::bkey) -> bool {
    matches!(
        c::bch_bkey_type(k.type_ as u32),
        c::bch_bkey_type::KEY_TYPE_btree_ptr | c::bch_bkey_type::KEY_TYPE_btree_ptr_v2
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Le32;

    /// A snapshot key from before depth, skip and btime - 24 bytes of its 64 -
    /// views as a snapshot: the fields it has as they are, the rest zero, not
    /// what's in the buffer past it. Shorter than snapshots have ever been,
    /// it's not one.
    #[test]
    fn bkey_view_short_value() {
        assert_eq!(BkeySnapshot::MIN_VAL_SIZE, 24);

        let mut k = BkeySnapshot::new();
        k.v.subvol = Le32::new(5);
        k.v.children = [Le32::new(3), Le32::new(4)];
        k.v.depth = Le32::new(7);
        k.k.set_val_bytes(24);

        let s = BkeySC::from(k.k_i()).view::<BkeySnapshot>().expect("a snapshot");
        assert_eq!((s.subvol.get(), s.children[0].get(), s.children[1].get()), (5, 3, 4));
        assert_eq!(s.depth().get(), 0);
        assert!(s.whole().is_none());
        assert!(BkeySC::from(k.k_i()).view::<BkeySubvolume>().is_none());

        k.k.set_val_bytes(16);
        assert!(BkeySC::from(k.k_i()).view::<BkeySnapshot>().is_none());
    }

    /// A key's value by its type: a View for a type that grew, an older value
    /// of it read as one; whole for a type that hasn't, a shorter value of
    /// which is damage - unknown, not a reference past its end.
    #[test]
    fn bkey_v_short_values() {
        let mut k = BkeySubvolume::new();
        k.v.snapshot = Le32::new(5);
        k.k.set_val_bytes(16);
        match BkeySC::from(k.k_i()).v() {
            BkeyValSC::subvolume(_, v) => assert_eq!((v.snapshot.get(), v.fs_path_parent().get()), (5, 0)),
            _ => panic!("a subvolume"),
        }

        let mut g = BkeyInodeGeneration::new();
        g.v.bi_generation = Le32::new(3);
        assert_eq!(BkeySC::from(g.k_i()).whole::<BkeyInodeGeneration>().map(|v| v.bi_generation.get()),
                   Some(3));
        g.k.set_val_bytes(0);
        assert!(matches!(BkeySC::from(g.k_i()).v(), BkeyValSC::unknown(..)));
        assert!(BkeySC::from(g.k_i()).whole::<BkeyInodeGeneration>().is_none());
    }

    /// A View of a snapshot over fewer bytes than every snapshot has - not
    /// one from view(), which won't make it - panics at a field every one
    /// has, rather than reading past them.
    #[test]
    #[should_panic(expected = "fewer than every valid one has")]
    fn bkey_view_unfloored() {
        let _ = View::<c::bch_snapshot>::new(&[0u8; 16]).subvol;
    }

    /// A subvolume key from before fs_path_parent - 16 bytes of its 48 - can't
    /// be had whole to edit, and setting a field it lacks is an error, not a
    /// write the key wouldn't carry; one it has sets.
    #[test]
    fn bkey_edit_short_value() {
        let mut k = BkeySubvolume::new();
        k.k.set_val_bytes(16);
        let k = k.k_i_mut();

        assert!(k.whole_mut::<BkeySubvolume>().is_none());
        assert!(k.whole_mut::<BkeySnapshot>().is_none());

        let mut v = k.view_mut::<BkeySubvolume>().expect("a subvolume");
        v.snapshot().set(5).unwrap();
        let e = v.fs_path_parent().set(9).unwrap_err();
        assert_eq!((e.field, e.at, e.len), ("fs_path_parent", 20, 16));

        let mut full = BkeySubvolume::new();
        full.k_i_mut().whole_mut::<BkeySubvolume>().expect("whole").fs_path_parent = Le32::new(9);
        assert_eq!(full.v.fs_path_parent, 9);
        assert_eq!(BkeySC::from(k as &c::bkey_i).view::<BkeySubvolume>().expect("a subvolume")
                   .fs_path_parent().get(), 0);
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
