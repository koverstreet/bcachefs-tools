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
use crate::util::vstructs::{ArmRef, Ffi, Floored, Unsized, View, ViewMut};
use core::marker::PhantomData;

use c::bpos as Bpos;

use core::cmp::Ordering;

/// The u64s of a key's header, struct bkey: as C's BKEY_U64s.
pub const BKEY_U64S: usize = core::mem::size_of::<c::bkey>() / core::mem::size_of::<u64>();

/// A key with its value inline, of any type or its own: a bkey_i, or a typed
/// one's .k_i - for what takes either, as btree_insert() does.
pub trait AsBkeyI {
    fn as_bkey_i_mut(&mut self) -> &mut c::bkey_i;
}

impl AsBkeyI for c::bkey_i {
    fn as_bkey_i_mut(&mut self) -> &mut c::bkey_i {
        self
    }
}

impl<V: ?Sized> AsBkeyI for BkeyI<V> {
    fn as_bkey_i_mut(&mut self) -> &mut c::bkey_i {
        self.k_i_mut()
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

    /// Its value's length, as its u64s say: C's bkey_val_bytes().
    pub fn val_bytes(&self) -> usize {
        (self.u64s as usize).saturating_sub(BKEY_U64S) * size_of::<u64>()
    }

    /// Whether it's @K's type: the tag. Not whether its value is long enough
    /// - that's validate's, as in C.
    pub fn is<K: TypedBkey + ?Sized>(&self) -> bool {
        self.type_ as u32 == K::TYPE.0
    }

    /// is::<K>(), with all of K's value there: for a reference to all of it,
    /// &BkeyI<K::Val> - a grown type's older value is shorter, and valid.
    pub fn holds_whole<K: TypedBkey + ?Sized>(&self) -> bool {
        self.is::<K>() && self.val_bytes() >= K::WHOLE_VAL_SIZE
    }

    /// u64s for a value of @bytes: C's set_bkey_val_bytes().
    pub fn set_val_bytes(&mut self, bytes: usize) {
        let u64s = BKEY_U64S + bytes.div_ceil(size_of::<u64>());
        self.u64s = u8::try_from(u64s)
            .unwrap_or_else(|_| panic!("key of {u64s} u64s: a key is at most {} u64s", u8::MAX));
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
/// bkey_s_c. It's a tagged union, its tag k.type_: typed() is C's switch on
/// it, each arm the key as its type's, BkeySC<'a, bch_extent> - C's struct
/// bkey_s_c_extent, BkeySCExtent (btree/bkey_types.rs) - whose value is
/// v(). @v is C's - a thin pointer to the value, as long as k.u64s says -
/// and @V only what the value is: v() makes the reference, sized or not.
#[repr(C)]
pub struct BkeySC<'a, V: ?Sized = c::bch_val> {
    pub k:           &'a c::bkey,
    pub(crate) v:    *const c::bch_val,
    pub(crate) iter: PhantomData<&'a mut BtreeIter<'a>>,
    pub(crate) val:  PhantomData<*const V>,
}

// References: Copy whatever V is, which a derive would require it to be.
impl<V: ?Sized> Clone for BkeySC<'_, V> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<V: ?Sized> Copy for BkeySC<'_, V> {}

/// A key with its value inline, of type V: C's struct bkey_i_<type>, whose
/// k is also the bkey_i k_i - an empty value, there. bkey_i's typed() and
/// arm() hand one out only for a key whose value is all there: a reference
/// to one reaches all of V.
///
/// Not Clone or Copy: a key is its header and u64s of value, which can run
/// past V - copying one is bkey_copy().
#[derive(Default)]
#[repr(C)]
pub struct BkeyI<V: ?Sized> {
    pub k: c::bkey,
    pub v: V,
}

impl<V: ?Sized> BkeyI<V> {
    /// The key, of any type: C's .k_i.
    pub fn k_i(&self) -> &c::bkey_i {
        // SAFETY: a bkey_i is a key's header, then its value - empty, there
        unsafe { &*(self as *const Self).cast::<c::bkey_i>() }
    }

    /// k_i(), to edit.
    pub fn k_i_mut(&mut self) -> &mut c::bkey_i {
        // SAFETY: as k_i()'s
        unsafe { &mut *(self as *mut Self).cast::<c::bkey_i>() }
    }

    /// Its value.
    pub fn v(&self) -> &V {
        &self.v
    }

    /// Its value, to edit.
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

/// A key type, named by its C typed key (bkey_i_<name>): its value type, and
/// typed access to it - for code generic over key types.
pub trait TypedBkey {
    /// The value: bch_<name> - sized, or a DST.
    type Val: ?Sized + ArmRef;

    const TYPE: c::bch_bkey_type;

    /// The shortest a valid value is: Val up to BCH_BKEY_TYPES()' field, the
    /// first one a value may end before, or all of it for `_` - a DST's
    /// header. One that long but shorter than Val is an older version; a
    /// shorter one is damage, which validate rejects as
    /// bkey_val_size_too_small.
    const MIN_VAL_SIZE: usize;

    /// How long a value is with all of it there: size_of a sized one, a
    /// DST's header.
    const WHOLE_VAL_SIZE: usize;

    /// The key at @k as this type's - a thin pointer for a sized value, fat
    /// for a DST, as long as k.u64s says.
    ///
    /// # Safety
    /// @k is a key of this type, its value WHOLE_VAL_SIZE at least.
    unsafe fn cast(k: *const c::bkey_i) -> *const BkeyI<Self::Val>;

    /// cast(), to edit.
    ///
    /// # Safety
    /// As cast()'s.
    unsafe fn cast_mut(k: *mut c::bkey_i) -> *mut BkeyI<Self::Val>;
}

/// A key type whose value can be had by value - made, or copied: all of it,
/// as C's struct, ValC - the value itself if it's sized, or a grown type's
/// (#[grown]), the fields an older value lacks zero. Not a type whose value
/// has a tail.
pub trait SizedBkey: TypedBkey<Val: BkeyValC<C: Default>> {
    /// A key of this type at POS_MIN, its value zeroed: C's
    /// bkey_<name>_init().
    fn new() -> Whole<Self>;

    /// @k's value if it's this type, zero padded past the end of a short
    /// one: see val_copy_pad().
    fn val_copy_pad(k: BkeySC<'_>) -> Option<ValC<Self::Val>>;

    /// @k, if it's this type, as an owned key: the value zero padded, u64s as
    /// on disk - as bch2_bkey_get_i_typed() copies it.
    fn from_key(k: BkeySC<'_>) -> Option<Whole<Self>>;
}

/// A key of type @K, whole, by value: C's typed key, bkey_i_<name>, its
/// value C's struct - K itself, for a sized value.
pub type Whole<K> = BkeyI<ValC<<K as TypedBkey>::Val>>;

/// A value type, as C has it: itself, or a DST's C struct - for what
/// C's typed keys hold, bkey_i_<name>.
pub trait BkeyValC {
    type C;
}

/// What C's typed inline key, bkey_i_<name>, holds: BkeyValC's.
pub type ValC<V> = <V as BkeyValC>::C;

/// Per value type, by BCH_BKEY_TYPES()' field column: `_`, every valid one
/// whole - &T; a field, the first a value may end before - a View; or one of
/// the value types that are DSTs, families (util/vstructs.rs), whose ArmRef
/// and length are their family's, listed here as they're made.
macro_rules! bkey_val {
    (inline_data, $f:tt)          => { bkey_val!(@family inline_data); };
    (indirect_inline_data, $f:tt) => { bkey_val!(@family indirect_inline_data); };
    (xattr, $f:tt)                => { bkey_val!(@family xattr); };
    (alloc, $f:tt)                => { bkey_val!(@family alloc); };
    (alloc_v2, $f:tt)             => { bkey_val!(@family alloc_v2); };
    (alloc_v3, $f:tt)             => { bkey_val!(@family alloc_v3); };
    (inode, $f:tt)                => { bkey_val!(@family inode); };
    (inode_v2, $f:tt)             => { bkey_val!(@family inode_v2); };
    (inode_v3, $f:tt)             => { bkey_val!(@family inode_v3); };
    (accounting, $f:tt)           => { bkey_val!(@family accounting); };
    (damage, $f:tt)               => { bkey_val!(@family damage); };
    (btree_ptr, $f:tt)            => { bkey_val!(@family btree_ptr); };
    (btree_ptr_v2, $f:tt)         => { bkey_val!(@family btree_ptr_v2); };
    (extent, $f:tt)               => { bkey_val!(@family extent); };
    (reflink_v, $f:tt)            => { bkey_val!(@family reflink_v); };

    (@family $name:ident) => { crate::paste! {
        impl BkeyValC for c::[<bch_ $name>] {
            type C = Ffi<c::[<bch_ $name>]>;
        }

        impl TypedBkey for c::[<bkey_i_ $name>] {
            type Val = c::[<bch_ $name>];
            const TYPE: c::bch_bkey_type = c::bch_bkey_type::[<KEY_TYPE_ $name>];
            const MIN_VAL_SIZE: usize = size_of::<Ffi<c::[<bch_ $name>]>>();
            const WHOLE_VAL_SIZE: usize = Self::MIN_VAL_SIZE;

            unsafe fn cast(k: *const c::bkey_i) -> *const BkeyI<Self::Val> {
                // SAFETY: the caller's: its value, after its header, as long
                // as k.u64s says
                unsafe {
                    let len = (*k).k.val_bytes();
                    let v = <Self::Val as Unsized>::from_raw(k.cast::<u8>().add(size_of::<c::bkey>()), len);
                    v.byte_sub(size_of::<c::bkey>()) as *const BkeyI<Self::Val>
                }
            }

            unsafe fn cast_mut(k: *mut c::bkey_i) -> *mut BkeyI<Self::Val> {
                // SAFETY: as cast()'s
                unsafe {
                    let len = (*k).k.val_bytes();
                    let v = <Self::Val as Unsized>::from_raw_mut(k.cast::<u8>().add(size_of::<c::bkey>()), len);
                    v.byte_sub(size_of::<c::bkey>()) as *mut BkeyI<Self::Val>
                }
            }
        }
    } };

    ($name:ident, $field:tt) => { crate::paste! {
        impl BkeyValC for c::[<bch_ $name>] {
            type C = Self;
        }

        impl TypedBkey for c::[<bkey_i_ $name>] {
            type Val = c::[<bch_ $name>];
            const TYPE: c::bch_bkey_type = c::bch_bkey_type::[<KEY_TYPE_ $name>];
            const MIN_VAL_SIZE: usize = bkey_val!(@min c::[<bch_ $name>], $field);
            const WHOLE_VAL_SIZE: usize = size_of::<Self::Val>();

            unsafe fn cast(k: *const c::bkey_i) -> *const BkeyI<Self::Val> {
                k.cast()
            }

            unsafe fn cast_mut(k: *mut c::bkey_i) -> *mut BkeyI<Self::Val> {
                k.cast()
            }
        }

        impl SizedBkey for c::[<bkey_i_ $name>] {
            fn new() -> Self {
                let mut k = c::bkey::new();
                k.type_ = Self::TYPE.0 as u8;
                k.set_val_bytes(size_of::<Self::Val>());
                Self { k, v: Default::default() }
            }

            fn val_copy_pad(k: BkeySC<'_>) -> Option<Self::Val> {
                k.k.is::<Self>().then(|| unsafe { k.val_copy_pad() })
            }

            fn from_key(k: BkeySC<'_>) -> Option<Self> {
                let v = Self::val_copy_pad(k)?;
                let mut r = Self::default();
                r.k = *k.k;
                r.v = v;
                Some(r)
            }
        }

        bkey_val!(@arm $name, $field);
    } };

    // BCH_BKEY_TYPES()' field column as a length, as C's
    // bch2_bkey_min_val_sizes[] has it: @val up to @field, or all of it
    (@min $val:ty, _)            => { size_of::<$val>() };
    (@min $val:ty, $field:ident) => { core::mem::offset_of!($val, $field) };

    // A value its type's ArmRef has whole, &T, and what a View of it derefs
    // to, Floored's Min, all of it - aligned, a value being at the u64 after
    // its key's header
    (@arm $name:ident, _) => { crate::paste! {
        impl ArmRef for c::[<bch_ $name>] {
            type Ref<'a> = &'a Self;
            type Mut<'a> = &'a mut Self;

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

        unsafe impl Floored for c::[<bch_ $name>] {
            type Min = Self;
        }
    } };
    // One that grew: a View, Floored by #[derive(View)]'s __bch_<name>_min!()
    (@arm $name:ident, $field:ident) => { crate::paste! {
        impl ArmRef for c::[<bch_ $name>] {
            type Ref<'a> = View<'a, Self>;
            type Mut<'a> = ViewMut<'a, Self>;

            fn arm_ref(b: &[u8]) -> Option<View<'_, Self>> {
                (b.len() >= <c::[<bkey_i_ $name>] as TypedBkey>::MIN_VAL_SIZE).then(|| View::new(b))
            }

            fn arm_mut(b: &mut [u8]) -> Option<ViewMut<'_, Self>> {
                (b.len() >= <c::[<bkey_i_ $name>] as TypedBkey>::MIN_VAL_SIZE).then(|| ViewMut::new(b))
            }
        }

        c::[<__bch_ $name _min>]!($field);
    } };
}

/// Everything defined per key type, from BCH_BKEY_TYPES(): (name, KEY_TYPE
/// number, ...).
macro_rules! bkey_types {
    ($(($name:ident, $nr:literal, $flags:expr, $field:tt $(, $($rest:tt)*)?)),* $(,)?) => { crate::paste! {
        $(
        pub type [<BkeyI $name:camel>] = c::[<bkey_i_ $name>];
        pub type [<BkeySC $name:camel>]<'a> = BkeySC<'a, c::[<bch_ $name>]>;
        pub type [<BkeyS $name:camel>]<'a> = BkeyS<'a, c::[<bch_ $name>]>;

        bkey_val!($name, $field);
        )*

        /// A key, by its type - C's switch on k.k->type: each arm the key as
        /// its type's, as bkey_s_c_to_<type>() makes it. BkeySC::typed().
        #[allow(non_camel_case_types)]
        #[derive(Clone, Copy)]
        pub enum BkeySCTyped<'a> {
            $($name(BkeySC<'a, c::[<bch_ $name>]>),)*
            /// A type this code doesn't know.
            unknown(BkeySC<'a>),
        }

        impl<'a> BkeySC<'a> {
            /// The key as its type's: C's switch on k.k->type.
            pub fn typed(self) -> BkeySCTyped<'a> {
                match self.k.type_ as u32 {
                    $($nr => BkeySCTyped::$name(self.cast()),)*
                    _ => BkeySCTyped::unknown(self),
                }
            }

        }

        /// A key to edit, by its type: as BkeySCTyped, each arm the key as its
        /// type's, as bkey_s_to_<type>() makes it. BkeyS::typed().
        #[allow(non_camel_case_types)]
        pub enum BkeySTyped<'a> {
            $($name(BkeyS<'a, c::[<bch_ $name>]>),)*
            /// As BkeySCTyped's.
            unknown(BkeyS<'a>),
        }

        impl BkeyS<'_> {
            /// The key as its type's, to edit: as BkeySC::typed().
            pub fn typed(&mut self) -> BkeySTyped<'_> {
                let k = self.reborrow();
                match k.k.type_ as u32 {
                    $($nr => BkeySTyped::$name(k.cast()),)*
                    _ => BkeySTyped::unknown(k),
                }
            }
        }

        /// A key with its value inline, by its type: as BkeySCTyped, each arm
        /// the key as its type's, bkey_i_to_<type>() - one whose value is all
        /// there. bkey_i::typed().
        #[allow(non_camel_case_types)]
        pub enum BkeyITyped<'a> {
            $($name(&'a [<BkeyI $name:camel>]),)*
            /// A type this code doesn't know, or a value shorter than its
            /// type - written before it grew, which a copy is grown from
            /// first (bkey_make_mut()), or damage.
            unknown(&'a c::bkey_i),
        }

        /// BkeyITyped, to edit.
        #[allow(non_camel_case_types)]
        pub enum BkeyITypedMut<'a> {
            $($name(&'a mut [<BkeyI $name:camel>]),)*
            /// As BkeyITyped's.
            unknown(&'a mut c::bkey_i),
        }

        impl c::bkey_i {
            /// The key as its type's, if its value is all there: C's switch
            /// on k.type.
            pub fn typed(&self) -> BkeyITyped<'_> {
                match self.k.type_ as u32 {
                    $($nr if self.k.holds_whole::<[<BkeyI $name:camel>]>() =>
                        // SAFETY: as arm()'s
                        BkeyITyped::$name(unsafe { &*<[<BkeyI $name:camel>] as TypedBkey>::cast(self) }),)*
                    _ => BkeyITyped::unknown(self),
                }
            }

            /// typed(), to edit.
            pub fn typed_mut(&mut self) -> BkeyITypedMut<'_> {
                match self.k.type_ as u32 {
                    $($nr if self.k.holds_whole::<[<BkeyI $name:camel>]>() =>
                        // SAFETY: as arm()'s
                        BkeyITypedMut::$name(unsafe { &mut *<[<BkeyI $name:camel>] as TypedBkey>::cast_mut(self) }),)*
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

    /// The key, of any type: C's .s_c, a typed key as the union.
    pub fn s_c(self) -> BkeySC<'a> {
        self.cast()
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
    /// Its value, as its type's ArmRef has it: whole for a type every valid
    /// value of which is, a View for one that grew - over its bytes, as long
    /// as k.u64s says. A value validate passed is at least as long as every
    /// valid one; a shorter one is a bug.
    pub fn v(&self) -> V::Ref<'a> {
        V::arm_ref(self.val_bytes())
            .unwrap_or_else(|| panic!("{}: {} bytes, shorter than validate passes", self.k,
                                      self.val_bytes().len()))
    }
}

impl<'a> BkeySC<'a> {
    /// The key as its type's, @K's, if it's that type: bkey_s_c_to_<type>().
    pub fn arm<K: TypedBkey + ?Sized>(self) -> Option<BkeySC<'a, K::Val>> {
        self.k.is::<K>().then(|| self.cast())
    }

    /// The key as C's bkey_s_c, for passing to C.
    pub(crate) fn to_raw(self) -> c::bkey_s_c {
        c::bkey_s_c {
            k: self.k as *const c::bkey as *mut c::bkey,
            v: self.v.cast_mut(),
        }
    }

    /// The value, copied into a @T and zero padded past what the key has: for
    /// reading a value that may predate fields @T has, which a reference to
    /// it would read past the end of - as C's bkey_val_copy_pad(). Typed
    /// wrappers check the key type, then call this. To read one in place,
    /// arm() then v().
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

    pub fn to_text<'f>(&self, fs: &'f Fs) -> BkeySCToText<'a, 'f> {
        BkeySCToText { k: *self, fs }
    }

    /// Where in the value its extent entries - its pointers - are: as
    /// bch2_bkey_ptrs_c(). Empty for a key type without them.
    fn extent_entry_range(&self) -> core::ops::Range<usize> {
        use core::mem::{offset_of, size_of};

        let len = self.val_bytes().len();
        match self.typed() {
            BkeySCTyped::btree_ptr(_)    => offset_of!(Ffi<c::bch_btree_ptr>, start)..len,
            BkeySCTyped::extent(_)       => offset_of!(Ffi<c::bch_extent>, start)..len,
            BkeySCTyped::reflink_v(_)    => offset_of!(Ffi<c::bch_reflink_v>, start)..len,
            BkeySCTyped::btree_ptr_v2(_) => offset_of!(Ffi<c::bch_btree_ptr_v2>, start)..len,
            BkeySCTyped::stripe(s) => {
                let start = offset_of!(c::bch_stripe, ptrs);
                start..(start + s.v().nr_blocks as usize * size_of::<c::bch_extent_ptr>()).min(len)
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
    /// The key as its type's, @K's, if it's that type and its value is all
    /// there: bkey_i_to_<type>().
    pub fn arm<K: TypedBkey + ?Sized>(&self) -> Option<&BkeyI<K::Val>> {
        // SAFETY: a K, its value all there; a key's value is at the u64 after
        // its header, aligned
        self.k.holds_whole::<K>().then(|| unsafe { &*K::cast(self) })
    }

    /// arm(), to edit.
    pub fn arm_mut<K: TypedBkey + ?Sized>(&mut self) -> Option<&mut BkeyI<K::Val>> {
        // SAFETY: as arm()'s
        self.k.holds_whole::<K>().then(|| unsafe { &mut *K::cast_mut(self) })
    }

    /// The key as @K's, whole, if it's that type and all of C's struct is
    /// there: every field a field - for a grown type, where arm() is the
    /// value as long as it is.
    pub fn whole<K: SizedBkey + ?Sized>(&self) -> Option<&Whole<K>> {
        // SAFETY: a K, all of its C struct there; a key's value is at the u64
        // after its header, aligned
        self.k.holds_whole::<K>().then(|| unsafe { &*(self as *const Self).cast::<Whole<K>>() })
    }

    /// whole(), to edit.
    pub fn whole_mut<K: SizedBkey + ?Sized>(&mut self) -> Option<&mut Whole<K>> {
        // SAFETY: as whole()'s
        self.k.holds_whole::<K>().then(|| unsafe { &mut *(self as *mut Self).cast::<Whole<K>>() })
    }
}

impl<'a> From<&'a c::bkey_i> for BkeySC<'a> {
    fn from(k: &'a c::bkey_i) -> Self {
        BkeySC {
            k:    &k.k,
            v:    &k.v,
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

/// A key and its value, borrowed to edit: C's struct bkey_s, which it's laid
/// out as. A tagged union, as BkeySC is: typed() gives the key as its
/// type's, BkeyS<'a, bch_extent> - C's struct bkey_s_extent, BkeySExtent -
/// whose value is v() and v_mut(); @v is a pointer, as BkeySC's is. Not
/// Copy: what edits it borrows it, &mut BkeyS, so one can be walked twice -
/// a read, then a rewrite - without aliasing.
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

    /// The key, to read: as bkey_s_to_s_c() - a typed key's C .c.
    pub fn as_sc(&self) -> BkeySC<'_, V> {
        BkeySC { k: &*self.k, v: self.v.cast_const(), iter: PhantomData, val: PhantomData }
    }

    /// The key, of any type: C's .s, a typed key as the union.
    pub fn s(self) -> BkeyS<'a> {
        self.cast()
    }

    /// The key, of any type, to read: C's .s_c.
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
    /// Its value: as BkeySC's v().
    pub fn v(&self) -> V::Ref<'_> {
        let k: &c::bkey = self.k;
        V::arm_ref(self.val_bytes())
            .unwrap_or_else(|| panic!("{k}: shorter than validate passes"))
    }

    /// Its value, to edit: a field a grown type's older value lacks is a
    /// set() error.
    pub fn v_mut(&mut self) -> V::Mut<'_> {
        let k = *self.k;
        V::arm_mut(self.val_bytes_mut())
            .unwrap_or_else(|| panic!("{k}: shorter than validate passes"))
    }
}

impl<'a> BkeyS<'a> {
    /// A key and its value, apart: C's struct bkey_s.
    ///
    /// # Safety
    /// @v is @k's value, as long as k.u64s says, nothing else touching it for
    /// 'a.
    pub unsafe fn from_parts(k: &'a mut c::bkey, v: *mut c::bch_val) -> Self {
        BkeyS { k, v, val: PhantomData }
    }

    /// The key as its type's, @K's, to edit, if it's that type:
    /// bkey_s_to_<type>(), as BkeySC::arm().
    pub fn arm<K: TypedBkey + ?Sized>(&mut self) -> Option<BkeyS<'_, K::Val>> {
        self.k.is::<K>().then(|| self.reborrow().cast())
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
            k:   &mut k.k,
            v:   &mut k.v,
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
                c::bch2_bkey_val_to_text(buf, self.fs.raw, self.k.to_raw())
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
    /// what's in the buffer past it.
    #[test]
    fn bkey_view_short_value() {
        assert_eq!(BkeyISnapshot::MIN_VAL_SIZE, 24);

        let mut k = BkeyISnapshot::new();
        k.v.subvol = Le32::new(5);
        k.v.children = [Le32::new(3), Le32::new(4)];
        k.v.depth = Le32::new(7);
        k.k.set_val_bytes(24);

        let s = BkeySC::from(k.k_i()).arm::<BkeyISnapshot>().expect("a snapshot").v();
        assert_eq!((s.subvol.get(), s.children[0].get(), s.children[1].get()), (5, 3, 4));
        assert_eq!(s.depth().get(), 0);
        assert!(s.whole().is_none());
        assert!(BkeySC::from(k.k_i()).arm::<BkeyISubvolume>().is_none());
    }

    /// A key by its type: a grown type's value a View, an older value of it
    /// read as one. The inline key is its type's only if its value is all
    /// there - an older grown one isn't.
    #[test]
    fn bkey_typed_short_values() {
        let mut k = BkeyISubvolume::new();
        k.v.snapshot = Le32::new(5);
        k.k.set_val_bytes(16);
        match BkeySC::from(k.k_i()).typed() {
            BkeySCTyped::subvolume(s) =>
                assert_eq!((s.v().snapshot.get(), s.v().fs_path_parent().get()), (5, 0)),
            _ => panic!("a subvolume"),
        }
        assert!(matches!(k.k_i().typed(), BkeyITyped::unknown(_)));

        let mut g = BkeyIInodeGeneration::new();
        g.v.bi_generation = Le32::new(3);
        assert_eq!(BkeySC::from(g.k_i()).arm::<BkeyIInodeGeneration>().map(|g| g.v().bi_generation.get()),
                   Some(3));
        assert!(matches!(g.k_i().typed(), BkeyITyped::inode_generation(_)));
    }

    /// A View of a snapshot over fewer bytes than every snapshot has - not
    /// one from a typed key's v(), whose key validate passed - panics at a
    /// field every one has, rather than reading past them.
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
        let mut k = BkeyISubvolume::new();
        k.k.set_val_bytes(16);
        let k = k.k_i_mut();

        assert!(k.arm_mut::<BkeyISubvolume>().is_none());
        assert!(k.arm_mut::<BkeyISnapshot>().is_none());

        let mut s = BkeyS::from(&mut *k);
        let BkeySTyped::subvolume(mut s) = s.typed() else { panic!("a subvolume") };
        let mut v = s.v_mut();
        v.snapshot().set(5).unwrap();
        let e = v.fs_path_parent().set(9).unwrap_err();
        assert_eq!((e.field, e.at, e.len), ("fs_path_parent", 20, 16));

        let mut full = BkeyISubvolume::new();
        full.k_i_mut().arm_mut::<BkeyISubvolume>().expect("whole").v_mut().fs_path_parent = Le32::new(9);
        assert_eq!(full.v.fs_path_parent, 9);
        assert_eq!(BkeySC::from(k as &c::bkey_i).arm::<BkeyISubvolume>().expect("a subvolume")
                   .v().fs_path_parent().get(), 0);
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
