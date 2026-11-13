//! Keys' types and traits - mod.rs has how they fit together; this is what
//! each is.
//!
//! The forms: BkeySC/BkeyS, C's bkey_s_c/bkey_s - a header pointer and a thin
//! value pointer, V phantom, v() making the reference - and BkeyI, header and
//! value inline, a DST, a reference covering all of the key.
//!
//! Over the forms:
//!  - BkeyV: a key in any form, TransBkey too - for code generic over how a
//!    key is held: its header, as_sc(), arm(), typed();
//!  - BkeyVMut: one that can be edited - as_s(), v_mut().
//!
//! Over value types, V:
//!  - BkeyVal: what V can be - a key type's value, or a family's;
//!  - BkeyFamily: a set of key types, what arm() narrows a key to - which keys
//!    are in it, what one is read as in place, Validated and Unvalidated;
//!  - KeyState: the state a key is in, its value's (BkeyVal::State) - which
//!    narrowing keeps: Of<K> is K's value in it, Narrowed<K, V> a V's key's;
//!  - Implies: what a state is too - itself, and the weaker ones: what a
//!    conversion may make a key, never stronger;
//!  - InFamily: a value type whose keys are a family's by type - what a key
//!    converts to the family by, From;
//!  - BkeyTag: a key type's tag and minimum value size - the row's data, all
//!    bkey_val! writes for every type; a key type is a BkeyTag, and a
//!    BkeyFamily of just itself;
//!  - BkeyTyped: what typed() switches a key over, by V - every type, or a
//!    family's;
//!  - FamilyTypes: a key family's types, F in FamilyVal<F> - its value, any of
//!    theirs, as long as the key is: ByFlag<F>, the key types with a
//!    BkeyTypeFlags, for most - Ptrs, Indirect - or a family's own,
//!    Extents';
//!  - BkeyWhole: a key type whose value can be had by value - BkeyI::new(),
//!    from_key();
//!  - MutBkey: what a mutable copy of a key read as V is - bkey_make_mut();
//!  - SizedVal: a sized key type's value, itself in place - what the generic
//!    impls of the sized types' traits go by - or an unvalidated one's,
//!    UnvalidatedSized. Unsized too, and its derive InState, so every key
//!    type's BkeyFamily is the one impl.
//!
//! What's known of a key type as data is BkeyType's: its flags, its value's
//! StructInfo, its methods. A family with a field to find - an indirect
//! extent's refcount - has FieldOffsets, built from them in the family's file:
//! the field by name in each of its types, not a trait per type.
//!
//! The macros generate what has to be an impl or an enum: bkey_types! per row
//! its BkeyTag and its InFamily markers - AllTypes and each of its
//! flags' - and the typed() enum of every type; bkey_family! a family read
//! per type, with its typed() (Ptrs); bkey_family_conv! a family's From impls -
//! one per form pair, generic over the key's type and state, so a single
//! into() erases, narrows the family and weakens the state at once. What's per
//! kind is at a value's definition: #[whole(bkey)], #[grown(.., bkey)],
//! #[derive(SizedVal)].

use super::BkeyP;
use crate::btree::bkey::{ops, BkeyOps, Ops};
use crate::btree::iter::{BtreeIter, TransBkey};
use crate::c;
use crate::format::{BkeyType, BkeyTypeFlags, Bpos};
use crate::typeinfo::{StructInfo, TypeInfo};
use crate::util::projections::Floored;
use crate::util::vstructs::{ArmRef, InState, State, Unsized, Unvalidated, UnvalidatedSized, Validated, Whole};
use core::marker::PhantomData;

#[repr(C)]
pub struct BkeySC<'a, V: ?Sized + BkeyVal = FamilyVal<AllTypes>> {
    pub k:           &'a c::bkey,
    pub(crate) v:    *const c::bch_val,
    pub(crate) iter: PhantomData<&'a mut BtreeIter<'a>>,
    pub(crate) val:  PhantomData<*const V>,
}

// Copy whatever V is: a derive would require V: Copy.
impl<V: ?Sized + BkeyVal> Clone for BkeySC<'_, V> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<V: ?Sized + BkeyVal> Copy for BkeySC<'_, V> {}

/// Not Copy: editing borrows it, so one can be walked twice - a read, then a
/// rewrite - without aliasing.
#[repr(C)]
pub struct BkeyS<'a, V: ?Sized + BkeyVal = FamilyVal<AllTypes>> {
    pub k:          &'a mut c::bkey,
    pub(crate) v:   *mut c::bch_val,
    pub(crate) val: PhantomData<*mut V>,
}

const _: () = {
    use core::mem::{align_of, offset_of, size_of};
    assert!(size_of::<BkeySC<'static>>() == size_of::<c::bkey_s_c>());
    assert!(align_of::<BkeySC<'static>>() == align_of::<c::bkey_s_c>());
    assert!(offset_of!(BkeySC<'static>, k) == offset_of!(c::bkey_s_c, k));
    assert!(offset_of!(BkeySC<'static>, v) == offset_of!(c::bkey_s_c, v));
};

/// Not Clone or Copy: the value can run past V - copying a key is bkey_copy().
///
/// XXX: the header is writable under a key - BkeyI's and BkeyS's pub k,
/// TransBkey::k_mut() - so safe code can set k.u64s past the value's bytes,
/// which every form reads its length from, or retag a typed key. u64s and
/// type_ should change only through what knows the room and the type.
#[derive(Default)]
#[repr(C)]
pub struct BkeyI<V: ?Sized = FamilyVal<AllTypes>> {
    pub k: c::bkey,
    pub v: V,
}

impl<'a, V: ?Sized + BkeyVal> BkeySC<'a, V> {
    /// The same key, header and value: bkey_and_val_eq().
    pub fn and_val_eq<W: ?Sized + BkeyVal>(&self, r: BkeySC<'_, W>) -> bool {
        self.k.fields_eq(r.k) && self.val_bytes() == r.val_bytes()
    }

    pub fn val_bytes(&self) -> &'a [u8] {
        // SAFETY: its value follows its header, as long as u64s says, for 'a
        unsafe { core::slice::from_raw_parts(self.v.cast::<u8>(), self.k.val_bytes()) }
    }

    fn cast<W: ?Sized + BkeyVal>(self) -> BkeySC<'a, W> {
        BkeySC { k: self.k, v: self.v, iter: PhantomData, val: PhantomData }
    }
}

impl<'a, V: ?Sized + BkeyVal + ArmRef> BkeySC<'a, V> {
    /// A value shorter than validate passes is a bug.
    pub fn v(&self) -> &'a V {
        V::arm_ref(self.val_bytes())
            .unwrap_or_else(|| panic!("{}: {} bytes, shorter than validate passes", self.k,
                                      self.val_bytes().len()))
    }
}

impl<'a> BkeySC<'a> {
    /// A key C hands over.
    ///
    /// # Safety
    /// @k's header and value are a key's, valid for 'a.
    pub unsafe fn from_c(k: &'a c::bkey_s_c) -> Self {
        BkeySC { k: unsafe { &*k.k }, v: k.v, iter: PhantomData, val: PhantomData }
    }

    /// bkey_i_to_s_c(): a whole key, header and value.
    pub fn from_bkey_i(k: &'a c::bkey_i) -> Self {
        BkeySC { k: &k.k, v: &k.v, iter: PhantomData, val: PhantomData }
    }
}

impl<'a, V: ?Sized + BkeyVal> BkeyS<'a, V> {
    pub fn into_val_bytes_mut(self) -> &'a mut [u8] {
        // SAFETY: as val_bytes()'s, for 'a, whose borrow this consumes
        unsafe { core::slice::from_raw_parts_mut(self.v.cast::<u8>(), self.k.val_bytes()) }
    }

    fn cast<W: ?Sized + BkeyVal>(self) -> BkeyS<'a, W> {
        BkeyS { k: self.k, v: self.v, val: PhantomData }
    }
}

impl<'a> BkeyS<'a> {
    /// # Safety
    /// @v is @k's value, as long as k.u64s says, nothing else touching it for
    /// 'a.
    pub unsafe fn from_parts(k: &'a mut c::bkey, v: *mut c::bch_val) -> Self {
        BkeyS { k, v, val: PhantomData }
    }
}

impl BkeyI {
    /// # Safety
    /// @p is a key, its u64s right and all of them there, and it may reach
    /// them - not a pointer widened from a reference to the header.
    pub unsafe fn from_raw(p: *const c::bkey_i) -> *const Self {
        unsafe { cast_unsized(p) }
    }

    pub unsafe fn from_raw_mut(p: *mut c::bkey_i) -> *mut Self {
        unsafe { cast_unsized_mut(p) }
    }
}

/// A thin entry's key, in the state it is: its container's.
impl<'a, S: KeyState> crate::util::vstructs::VstructRef<'a, c::bkey_i, S> {
    pub fn key(&self) -> &'a BkeyI<FamilyVal<AllTypes, S>> {
        // SAFETY: the walk checked its u64s against its bytes
        unsafe { &*cast_unsized(self.as_bytes().as_ptr().cast()) }
    }
}

impl<'a, S: KeyState> crate::util::vstructs::VstructMut<'a, c::bkey_i, S> {
    pub fn key_mut(&mut self) -> &mut BkeyI<FamilyVal<AllTypes, S>> {
        // SAFETY: the walk checked its u64s against its bytes
        unsafe { &mut *cast_unsized_mut(self.as_mut_ptr()) }
    }

    pub fn into_key_mut(mut self) -> &'a mut BkeyI<FamilyVal<AllTypes, S>> {
        unsafe { &mut *cast_unsized_mut(self.as_mut_ptr()) }
    }
}

impl<V: ?Sized> BkeyI<V> {
    pub fn as_ffi(&self) -> *const c::bkey_i {
        (self as *const Self).cast()
    }

    /// All of it, as its u64s say: header and value.
    pub fn as_bytes(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.as_ffi().cast::<u8>(), self.k.bytes()) }
    }

    pub fn as_ffi_mut(&mut self) -> *mut c::bkey_i {
        (self as *mut Self).cast()
    }

    pub fn v(&self) -> &V {
        &self.v
    }

    pub fn v_mut(&mut self) -> &mut V {
        &mut self.v
    }
}

impl<V: BkeyWhole> BkeyI<V> {
    pub fn new() -> Self {
        let mut k = c::bkey::new();
        k.type_ = V::TYPE.0 as u8;
        k.set_val_bytes(size_of::<V>());
        BkeyI { k, v: V::default() }
    }

    /// One at @p.
    pub fn at(p: Bpos) -> Self {
        let mut k = Self::new();
        k.k.p = p;
        k
    }

    /// u64s clamped to V, as __bch2_bkey_get_i_typed() does: a newer version's
    /// fields are dropped - this one couldn't have kept them consistent with
    /// its update. An older value's stay short, so to_text() doesn't print
    /// fields it hasn't.
    pub fn from_key(k: BkeySC<'_>) -> Option<Self> {
        let v = V::val_copy_pad(k)?;
        let mut hdr = *k.k;
        hdr.set_val_bytes(hdr.val_bytes().min(size_of::<V>()));
        Some(BkeyI { k: hdr, v })
    }
}

impl<V: ?Sized + BkeyVal> From<BkeySC<'_, V>> for c::bkey_s_c {
    fn from(k: BkeySC<'_, V>) -> Self {
        c::bkey_s_c { k: k.k as *const c::bkey as *mut c::bkey, v: k.v.cast_mut() }
    }
}

impl<'a, V: ?Sized + BkeyVal> From<&'a BkeyI<V>> for BkeySC<'a, V> {
    fn from(k: &'a BkeyI<V>) -> Self {
        BkeySC { k: &k.k, v: (&raw const k.v).cast(), iter: PhantomData, val: PhantomData }
    }
}

impl<'a, V: ?Sized + BkeyVal> From<&'a BkeyS<'_, V>> for BkeySC<'a, V> {
    fn from(k: &'a BkeyS<'_, V>) -> Self {
        BkeySC { k: &*k.k, v: k.v.cast_const(), iter: PhantomData, val: PhantomData }
    }
}

impl<'a, V: ?Sized + BkeyVal> From<&'a mut BkeyI<V>> for BkeyS<'a, V> {
    fn from(k: &'a mut BkeyI<V>) -> Self {
        BkeyS { k: &mut k.k, v: (&raw mut k.v).cast(), val: PhantomData }
    }
}

/// The bkey_i it is, of any type - a validated key's, a key in a state that
/// implies Validated: an unvalidated one converts by From, to one of its own.
impl<V: ?Sized + ValidatedVal> AsRef<BkeyI> for BkeyI<V> {
    fn as_ref(&self) -> &BkeyI {
        unsafe { &*cast_unsized(self.as_ffi()) }
    }
}

impl<V: ?Sized + ValidatedVal> AsMut<BkeyI> for BkeyI<V> {
    fn as_mut(&mut self) -> &mut BkeyI {
        unsafe { &mut *cast_unsized_mut(self.as_ffi_mut()) }
    }
}

pub trait BkeyV: Sized {
    type V: ?Sized + BkeyVal;

    type As<W: ?Sized + BkeyVal + 'static>: BkeyV<V = W>;

    fn as_sc(&self) -> BkeySC<'_, Self::V>;

    fn k(&self) -> &c::bkey {
        self.as_sc().k
    }

    fn v(&self) -> &Self::V
        where Self::V: ArmRef
    {
        self.as_sc().v()
    }

    fn to_text<'f>(&self, fs: &'f crate::fs::Fs) -> super::BkeySCToText<'_, 'f, Self::V> {
        super::BkeySCToText { k: self.as_sc(), fs }
    }

    fn typed(self) -> <Self::V as BkeyTyped>::Typed<Self>
        where Self::V: BkeyTyped
    {
        <Self::V as BkeyTyped>::typed(self)
    }

    /// The same key, of any type, in the state it's in.
    fn untyped(self) -> Self::As<Narrowed<FamilyVal<AllTypes>, Self::V>> {
        // SAFETY: every key is of AllTypes
        unsafe { self.arm_unchecked::<FamilyVal<AllTypes>>() }
    }

    fn arm<K: BkeyFamily + ?Sized>(self) -> Option<Self::As<Narrowed<K, Self::V>>> {
        // SAFETY: a K, as K::contains() says; its length is validate's
        K::contains(self.k()).then(|| unsafe { self.arm_unchecked::<K>() })
    }

    /// # Safety
    /// The key is a @K, as K::contains() says, its length validate's.
    unsafe fn arm_unchecked<K: BkeyFamily + ?Sized>(self) -> Self::As<Narrowed<K, Self::V>>;
}

pub trait BkeyVMut: BkeyV {
    fn as_s(&mut self) -> BkeyS<'_, Self::V>;

    fn val_bytes_mut(&mut self) -> &mut [u8] {
        self.as_s().into_val_bytes_mut()
    }

    /// A field a grown type's older value lacks is a set() error.
    fn v_mut(&mut self) -> &mut Self::V
        where Self::V: ArmRef
    {
        let k = *self.k();
        <Self::V as ArmRef>::arm_mut(self.val_bytes_mut())
            .unwrap_or_else(|| panic!("{k}: {} bytes, shorter than validate passes", k.val_bytes()))
    }
}

pub mod prelude {
    pub use super::{BkeyV, BkeyVMut};
    pub use crate::data::durability::BkeyVDurability;
    pub use crate::data::extents::BkeyVExtents;
    pub use crate::data::ptrs::BkeyVPtrs;
    pub use crate::inode::BkeyVInode;
}

impl<'a, V: ?Sized + BkeyVal> BkeyV for BkeySC<'a, V> {
    type V = V;
    type As<W: ?Sized + BkeyVal + 'static> = BkeySC<'a, W>;

    fn as_sc(&self) -> BkeySC<'_, V> {
        // anew: a BkeySC is invariant in its lifetime
        BkeySC { k: self.k, v: self.v, iter: PhantomData, val: PhantomData }
    }

    unsafe fn arm_unchecked<K: BkeyFamily + ?Sized>(self) -> BkeySC<'a, Narrowed<K, V>> {
        self.cast()
    }
}

impl<'a, V: ?Sized + BkeyVal> BkeyV for BkeyS<'a, V> {
    type V = V;
    type As<W: ?Sized + BkeyVal + 'static> = BkeyS<'a, W>;

    fn as_sc(&self) -> BkeySC<'_, V> {
        BkeySC::from(self)
    }

    unsafe fn arm_unchecked<K: BkeyFamily + ?Sized>(self) -> BkeyS<'a, Narrowed<K, V>> {
        self.cast()
    }
}

impl<V: ?Sized + BkeyVal> BkeyVMut for BkeyS<'_, V> {
    fn as_s(&mut self) -> BkeyS<'_, V> {
        BkeyS { k: &mut *self.k, v: self.v, val: PhantomData }
    }
}

impl<'a, V: ?Sized + BkeyVal> BkeyV for &'a BkeyI<V> {
    type V = V;
    type As<W: ?Sized + BkeyVal + 'static> = &'a BkeyI<W>;

    fn as_sc(&self) -> BkeySC<'_, V> {
        BkeySC::from(&**self)
    }

    unsafe fn arm_unchecked<K: BkeyFamily + ?Sized>(self) -> &'a BkeyI<Narrowed<K, V>> {
        &*cast_unsized(self.as_ffi())
    }
}

impl<'a, V: ?Sized + BkeyVal> BkeyV for &'a mut BkeyI<V> {
    type V = V;
    type As<W: ?Sized + BkeyVal + 'static> = &'a mut BkeyI<W>;

    fn as_sc(&self) -> BkeySC<'_, V> {
        BkeySC::from(&**self)
    }

    unsafe fn arm_unchecked<K: BkeyFamily + ?Sized>(self) -> &'a mut BkeyI<Narrowed<K, V>> {
        &mut *cast_unsized_mut(self.as_ffi_mut())
    }
}

impl<V: ?Sized + BkeyVal> BkeyVMut for &mut BkeyI<V> {
    fn as_s(&mut self) -> BkeyS<'_, V> {
        BkeyS::from(&mut **self)
    }
}

pub trait BkeyVal {
    type State: KeyState;
}

impl<T: InState<State: KeyState> + ?Sized> BkeyVal for T {
    type State = T::State;
}

/// A key's value in each state it can be in, as a key read in place has it,
/// as long as k.u64s says: KeyState::Of picks one.
pub trait BkeyFamily {
    type Validated: ?Sized + Unsized + BkeyVal<State = Validated> + 'static;
    type Unvalidated: ?Sized + Unsized + BkeyVal<State = Unvalidated> + 'static;

    /// Its type, and anything else the set requires - an extent's size.
    fn contains(k: &c::bkey) -> bool;
}

/// The state a key is in, as its value's is: what narrowing it keeps.
pub trait KeyState: State {
    /// @K's value in this state.
    type Of<K: BkeyFamily + ?Sized>: ?Sized + Unsized + BkeyVal + 'static;
}

impl KeyState for Validated {
    type Of<K: BkeyFamily + ?Sized> = K::Validated;
}

impl KeyState for Unvalidated {
    type Of<K: BkeyFamily + ?Sized> = K::Unvalidated;
}

// XXX: a whole key narrows to its value in place - Validated - for now
impl KeyState for Whole {
    type Of<K: BkeyFamily + ?Sized> = K::Validated;
}

/// A key whose value is a @V narrowed to @K: K's value, in V's state.
pub type Narrowed<K, V> = <<V as BkeyVal>::State as KeyState>::Of<K>;

/// A key whose value is Self is one of family F's, by its type alone - what
/// a key converts to a family's by, From: bkey_types!' per row, and a family
/// in a wider one.
///
/// # Safety
/// Every key of Self's is a key of F's: F's contains() is true of it.
pub unsafe trait InFamily<F> {}

/// What a key in state Self is too: itself, and every weaker state - Whole
/// a Validated key, and Validated an Unvalidated one. Never stronger:
/// validating is the only way up.
pub trait Implies<S: KeyState>: KeyState {}

impl Implies<Whole> for Whole {}
impl Implies<Validated> for Whole {}
impl Implies<Unvalidated> for Whole {}
impl Implies<Validated> for Validated {}
impl Implies<Unvalidated> for Validated {}
impl Implies<Unvalidated> for Unvalidated {}

pub trait ValidatedVal: BkeyVal<State: Implies<Validated>> {}

impl<T: ?Sized + BkeyVal<State: Implies<Validated>>> ValidatedVal for T {}

pub trait BkeyTag {
    const TYPE: BkeyType;

    /// Up to its #[grown(from)] field: shorter than the struct is an older
    /// version, shorter than this damage - bkey_val_size_too_small.
    const MIN_VAL_SIZE: usize;
}

pub trait BkeyTyped: BkeyVal {
    type Typed<K: BkeyV<V = Self>>;

    fn typed<K: BkeyV<V = Self>>(k: K) -> Self::Typed<K>;
}

/// A plain type, or a grown type's struct - not a type with a tail.
///
/// # Safety
/// Any bytes, all zeroes among them, are a valid Self in each of its fields -
/// padding aside, which may hold anything: val_copy_pad() copies the key's
/// bytes over a zeroed one.
pub unsafe trait BkeyWhole: Sized + Default + BkeyTag + BkeyFamily<Validated: ArmRef>
    + InFamily<AllTypes> + ValidatedVal
{
    /// @k's value, if it's one, zero padded: it may predate fields Self has,
    /// which a reference would read past its end for.
    fn val_copy_pad(k: BkeySC<'_>) -> Option<Self> {
        if !k.k.key_type().is::<Self>() {
            return None;
        }
        let mut v = core::mem::MaybeUninit::<Self>::zeroed();
        let n = k.k.val_bytes().min(size_of::<Self>());
        // SAFETY: @n of a Self's bytes, into a zeroed one: BkeyWhole's
        unsafe {
            core::ptr::copy_nonoverlapping(k.v.cast::<u8>(), v.as_mut_ptr().cast::<u8>(), n);
            Some(v.assume_init())
        }
    }
}

/// bkey_make_mut_noupdate() makes a TransBkey<Self::Mut>, grown to MIN_BYTES.
///
/// # Safety
/// Mut is a BkeyWhole whose key is MIN_BYTES: TransBkey<Mut>'s value is over
/// that many - or Self, MIN_BYTES 0, a value read as long as its key's u64s
/// whose every valid value is one: a whole type's, a key family's.
pub unsafe trait MutBkey: BkeyVal {
    type Mut: ?Sized + BkeyVal;
    const MIN_BYTES: usize;
}

// SAFETY: a BkeyWhole, its key whole
unsafe impl<T: BkeyWhole + BkeyVal> MutBkey for T {
    type Mut = Self;
    const MIN_BYTES: usize = size_of::<BkeyI<Self>>();
}

/// A key value that's itself in place, all of it: #[derive(SizedVal)].
///
/// # Safety
/// Any bytes are a valid Self.
pub unsafe trait SizedVal: Sized + 'static {}

// SAFETY: SizedVal's
unsafe impl<T> BkeyWhole for T
    where T: SizedVal + Default + BkeyTag + BkeyFamily<Validated: ArmRef> + InFamily<AllTypes>
           + ValidatedVal
{}

// A value is at the u64 after its key's header, so it's aligned.
impl<T: SizedVal> ArmRef for T {
    const MIN_LEN: usize = size_of::<Self>();

    fn arm_ref(b: &[u8]) -> Option<&Self> {
        (b.len() >= size_of::<Self>() && b.as_ptr() as usize % align_of::<Self>() == 0)
            .then(|| unsafe { &*b.as_ptr().cast() })
    }

    fn arm_mut(b: &mut [u8]) -> Option<&mut Self> {
        (b.len() >= size_of::<Self>() && b.as_ptr() as usize % align_of::<Self>() == 0)
            .then(|| unsafe { &mut *b.as_mut_ptr().cast() })
    }
}

unsafe impl<T: SizedVal> Floored for T {
    type Min = Self;
}

// SAFETY: a reference from it claims size_of::<T>() of @bytes, the first -
// which a valid value has, MIN_VAL_SIZE being all of it - not all of them
unsafe impl<T: SizedVal> Unsized for T {
    type Head = T;
    type Elem = u8;
    const TAIL: usize = size_of::<T>();

    fn from_raw(p: *const u8, _bytes: usize) -> *const Self {
        p.cast()
    }

    fn from_raw_mut(p: *mut u8, _bytes: usize) -> *mut Self {
        p.cast()
    }
}

// SAFETY: #[repr(transparent)] over T
unsafe impl<T: SizedVal> SizedVal for UnvalidatedSized<T> {}

impl<T: SizedVal> InState for UnvalidatedSized<T> {
    type In<S: State> = S::OfSized<T>;
    type State = Unvalidated;
}

impl<T: BkeyTag + InState> BkeyFamily for T
    where T::In<Validated>: Unsized + BkeyVal<State = Validated> + 'static,
          T::In<Unvalidated>: Unsized + BkeyVal<State = Unvalidated> + 'static,
{
    type Validated = T::In<Validated>;
    type Unvalidated = T::In<Unvalidated>;

    fn contains(k: &c::bkey) -> bool {
        k.key_type().is::<T>()
    }
}

/// The types of a key family, F in FamilyVal<F>: which keys are one.
pub trait FamilyTypes: 'static {
    fn contains(k: &c::bkey) -> bool;
}

/// A value of any of family F's types, as long as the key is, in state S -
/// the state its key's value is in, which narrowing it keeps.
#[repr(transparent)]
pub struct FamilyVal<F, S: State = Validated>(PhantomData<(F, S)>, [u64]);

/// Every key type, and those this code doesn't know: FamilyVal<AllTypes> is a
/// key of any type's value.
pub enum AllTypes {}

impl FamilyTypes for AllTypes {
    fn contains(_k: &c::bkey) -> bool {
        true
    }
}

/// What's known of a key type as data, from its value type: its struct, its
/// shortest valid value, its methods.
macro_rules! key_types {
    ($(($name:ident, $nr:literal, $ty:ty, $flags:tt)),* $(,)?) => {
        impl BkeyType {
            /// Its value's struct: None for a type this code doesn't know.
            pub const fn info(self) -> Option<&'static StructInfo> {
                match self {
                    $(Self::$name => Some(<$ty as TypeInfo>::INFO),)*
                    _ => None,
                }
            }

            /// The shortest a valid value is, BkeyTag's - shorter than its
            /// struct is an older version: 0 for a type this code doesn't know.
            pub const fn min_val_size(self) -> usize {
                match self {
                    $(Self::$name => <$ty as BkeyTag>::MIN_VAL_SIZE,)*
                    _ => 0,
                }
            }

            /// Its methods: none for a type this code doesn't know.
            pub fn ops(self) -> &'static Ops {
                match self {
                    $(Self::$name => { static OPS: Ops = ops::<$ty>(); &OPS })*
                    _ => { static OPS: Ops = ops::<Unknown>(); &OPS }
                }
            }
        }
    };
}
c::BkeyType_arms!(key_types);

enum Unknown {}
impl BkeyOps for Unknown {}

const NR_KEY_TYPES: usize = BkeyType::NR as usize;

/// The key types with @flag, a bit each, by number.
pub const fn types_with(flag: BkeyTypeFlags) -> u64 {
    let (mut mask, mut i) = (0, 0);
    while i < NR_KEY_TYPES {
        if BkeyType(i as u32).flags().intersects(flag) {
            mask |= 1 << i;
        }
        i += 1;
    }
    mask
}

/// Where a family's field is in each of its types' values: the family a
/// flag's, the field found by its name in each type's struct.
pub struct FieldOffsets([Option<usize>; NR_KEY_TYPES]);

impl FieldOffsets {
    /// In every @flag type: a build error if one hasn't it.
    pub const fn new(flag: BkeyTypeFlags, name: &str) -> Self {
        let t = Self::optional(flag, name);
        let mut i = 0;
        while i < NR_KEY_TYPES {
            assert!(!BkeyType(i as u32).flags().intersects(flag) || t.0[i].is_some(),
                    "a key type with the flag hasn't the field its family reads");
            i += 1;
        }
        t
    }

    /// In the @flag types that have it.
    pub const fn optional(flag: BkeyTypeFlags, name: &str) -> Self {
        let mut t = [None; NR_KEY_TYPES];
        let mut i = 0;
        while i < NR_KEY_TYPES {
            let ty = BkeyType(i as u32);
            if let (true, Some(info)) = (ty.flags().intersects(flag), ty.info()) {
                t[i] = info.field_offset(name);
            }
            i += 1;
        }
        Self(t)
    }

    /// Where it is in @k's value: None if its type hasn't it.
    pub fn of(&self, k: &c::bkey) -> Option<usize> {
        *self.0.get(k.type_ as usize)?
    }
}

/// The key types with flag F, a BkeyTypeFlags bit: a family's types -
/// FamilyVal<ByFlag<F>> - by their bkey_val arms' flags.
pub enum ByFlag<const F: u32> {}

impl<const F: u32> FamilyTypes for ByFlag<F> {
    fn contains(k: &c::bkey) -> bool {
        k.key_type().flags().bits() & F != 0
    }
}

// SAFETY: every key is one of AllTypes'
unsafe impl<const F: u32, S: KeyState> InFamily<AllTypes> for FamilyVal<ByFlag<F>, S> {}

// SAFETY: the value's u64s, all of them
unsafe impl<F: 'static, S: State> Unsized for FamilyVal<F, S> {
    type Head = ();
    type Elem = u64;
    const TAIL: usize = 0;

    fn from_raw(p: *const u8, bytes: usize) -> *const Self {
        core::ptr::slice_from_raw_parts(p.cast::<u64>(), bytes / size_of::<u64>()) as *const Self
    }

    fn from_raw_mut(p: *mut u8, bytes: usize) -> *mut Self {
        core::ptr::slice_from_raw_parts_mut(p.cast::<u64>(), bytes / size_of::<u64>()) as *mut Self
    }
}

impl<F: 'static, S: KeyState> BkeyVal for FamilyVal<F, S> {
    type State = S;
}

// SAFETY: any key of the family, as long as it is
unsafe impl<F: 'static, S: KeyState> MutBkey for FamilyVal<F, S> {
    type Mut = Self;
    const MIN_BYTES: usize = 0;
}

impl<F: FamilyTypes, S: KeyState> BkeyFamily for FamilyVal<F, S> {
    type Validated = FamilyVal<F, Validated>;
    type Unvalidated = FamilyVal<F, Unvalidated>;

    fn contains(k: &c::bkey) -> bool {
        F::contains(k)
    }
}

/// # Safety
/// @k is a key whose value is a @V, its length validate's.
pub(crate) unsafe fn cast_unsized<V: Unsized + ?Sized>(k: *const c::bkey_i) -> *const BkeyI<V> {
    unsafe {
        let v = V::from_raw(k.cast::<u8>().add(size_of::<c::bkey>()), (*k).k.val_bytes());
        v.byte_sub(size_of::<c::bkey>()) as *const BkeyI<V>
    }
}

pub(crate) unsafe fn cast_unsized_mut<V: Unsized + ?Sized>(k: *mut c::bkey_i) -> *mut BkeyI<V> {
    unsafe {
        let v = V::from_raw_mut(k.cast::<u8>().add(size_of::<c::bkey>()), (*k).k.val_bytes());
        v.byte_sub(size_of::<c::bkey>()) as *mut BkeyI<V>
    }
}

/// `bkey_typed! { Enum for F [, unknown]; rows }`: typed() of a
/// FamilyVal<F, S> key, in any state S, its arms in S. `unknown`, an arm for
/// a type this code doesn't know - else there's none.
macro_rules! bkey_typed {
    ($(#[$meta:meta])* $enum:ident for $f:ty $(, $unknown:ident)?;
     $(($name:ident, $nr:literal, $ty:ty, $($rest:tt)*)),* $(,)?) => {
        $(#[$meta])*
        #[allow(non_camel_case_types)]
        pub enum $enum<K: crate::btree::bkey::BkeyV> {
            $($name(K::As<crate::btree::bkey::Narrowed<$ty, K::V>>),)*
            $(
            $unknown(K),
            )?
        }

        // Copy as a BkeySC is: a derive would require only K: Copy, not its arms
        impl<'a, S: crate::btree::bkey::KeyState> Clone
            for $enum<crate::btree::bkey::BkeySC<'a, crate::btree::bkey::FamilyVal<$f, S>>>
        {
            fn clone(&self) -> Self {
                *self
            }
        }
        impl<'a, S: crate::btree::bkey::KeyState> Copy
            for $enum<crate::btree::bkey::BkeySC<'a, crate::btree::bkey::FamilyVal<$f, S>>> {}

        impl<S: crate::btree::bkey::KeyState> crate::btree::bkey::BkeyTyped
            for crate::btree::bkey::FamilyVal<$f, S>
        {
            type Typed<K: crate::btree::bkey::BkeyV<V = Self>> = $enum<K>;

            fn typed<K: crate::btree::bkey::BkeyV<V = Self>>(k: K) -> $enum<K> {
                match k.k().type_ as u32 {
                    $($nr => $enum::$name(unsafe { k.arm_unchecked::<$ty>() }),)*
                    t => crate::btree::bkey::bkey_typed!(@other $enum, $f, k, t $(, $unknown)?),
                }
            }
        }
    };
    (@other $enum:ident, $f:ty, $k:ident, $t:ident, $unknown:ident) => {{ let _ = $t; $enum::$unknown($k) }};
    (@other $enum:ident, $f:ty, $k:ident, $t:ident) => {
        unreachable!(concat!("a ", stringify!($f), " key of type {}"), $t)
    };
}
pub(crate) use bkey_typed;

/// `bkey_family!(Ptrs: ptrs)`: the key family of the bkey_val arms
/// with that flag - FamilyVal<ByFlag<flag>> - and its typed(), for a
/// family read per type. Its doc is the caller's: a family is defined where
/// it's used.
macro_rules! bkey_family {
    ($(#[$meta:meta])* $family:ident: $flag:ident) => { crate::paste! {
        pub type [<$family Types>] = crate::btree::bkey::ByFlag<{ crate::format::BkeyTypeFlags_bits::$flag }>;

        $(#[$meta])*
        pub type $family = crate::btree::bkey::FamilyVal<[<$family Types>]>;

        crate::c::BkeyType_arms!(cstruct_macros::xmacro_select [$flag =>
            crate::btree::bkey::bkey_typed { [<$family Typed>] for [<$family Types>]; }]);
    } };
}
pub(crate) use bkey_family;

/// `bkey_family_conv!(F)`: a key of F's - InFamily - becomes FamilyVal<F, S>
/// by From, in any state its own implies, in its form or any it converts to:
/// one impl for each, the axes in its bounds, so one into() does all three.
/// The forms don't convert back: BkeyI -> BkeyS -> BkeySC.
macro_rules! bkey_family_conv {
    ($f:ty) => {
        bkey_family_conv!(@forms $f;
            ['a]      [BkeySC<'a,]               => [BkeySC<'a,]:   |k| k.cast();
            ['a]      [BkeyS<'a,]                => [BkeyS<'a,]:    |k| k.cast();
            ['a]      [&'a BkeyI<]               => [&'a BkeyI<]:   |k| &*cast_unsized(k.as_ffi());
            ['a]      [&'a mut BkeyI<]           => [&'a mut BkeyI<]:
                |k| &mut *cast_unsized_mut(k.as_ffi_mut());
            ['a]      [&'a BkeyP<]               => [&'a BkeyP<]:   |k| k.cast();
            ['a]      [&'a mut BkeyP<]           => [&'a mut BkeyP<]: |k| k.cast_mut();
            ['a]      [&'a BkeyI<]               => [&'a BkeyP<]:   |k| <&BkeyP<V>>::from(k).cast();
            ['a]      [&'a mut BkeyI<]           => [&'a mut BkeyP<]:
                |k| <&mut BkeyP<V>>::from(k).cast_mut();
            ['a, 't]  [TransBkey<'a, 't,]        => [TransBkey<'a, 't,]: |k| k.cast();
            ['a]      [&'a BkeyI<]               => [BkeySC<'a,]:   |k| BkeySC::<V>::from(k).cast();
            ['a, 'b]  [&'a BkeyS<'b,]            => [BkeySC<'a,]:   |k| BkeySC::<V>::from(k).cast();
            ['a]      [&'a mut BkeyI<]           => [BkeyS<'a,]:    |k| BkeyS::<V>::from(k).cast();
            ['a]      [&'a TransBkey<'_, '_,]    => [BkeySC<'a,]:   |k| BkeySC::<V>::from(k).cast();
            ['a]      [&'a mut TransBkey<'_, '_,] => [BkeyS<'a,]:   |k| BkeyS::<V>::from(k).cast();
            ['a]      [&'a TransBkey<'_, '_,]    => [&'a BkeyI<]:   |k| &*cast_unsized(k.as_ptr());
            ['a]      [&'a mut TransBkey<'_, '_,] => [&'a mut BkeyI<]:
                |k| &mut *cast_unsized_mut(k.as_ptr()););
    };

    // each form, up to its V: Form<.., V>
    (@forms $f:ty; $([$($g:tt)*] [$($src:tt)*] => [$($dst:tt)*]: |$k:ident| $body:expr;)*) => {$(
        impl<$($g)*, V, S> From<$($src)* V>> for $($dst)* FamilyVal<$f, S>>
            where V: ?Sized + InFamily<$f> + BkeyVal<State: Implies<S>>, S: KeyState
        {
            #[allow(unused_unsafe)]
            fn from($k: $($src)* V>) -> Self {
                // SAFETY: a key of @f's, by InFamily, in a state its own implies
                unsafe { $body }
            }
        }
    )*};
}

/// A key type's impls from its bkey_val arm; what's per kind is at its
/// value's definition - #[whole(bkey)], #[grown(.., bkey)], #[derive(SizedVal)].
macro_rules! bkey_types {
    ($(($name:ident, $nr:literal, $ty:ty, [$($flag:ident),*])),* $(,)?) => {
        $(
        // the shortest valid value: its value in place's head - a family's C
        // struct or a grown type's floor, a sized type all of it
        impl BkeyTag for $ty {
            const TYPE: BkeyType = BkeyType($nr);
            const MIN_VAL_SIZE: usize =
                size_of::<<<Self as BkeyFamily>::Validated as Unsized>::Head>();
        }
        bkey_types!(@in $ty, AllTypes $(, ByFlag<{ crate::format::BkeyTypeFlags_bits::$flag }>)*);
        )*

        bkey_typed! {
            Typed for AllTypes, unknown;
            $(($name, $nr, $ty, [$($flag),*])),*
        }
    };

    (@in $ty:ty $(, $f:ty)+) => { $(
        // SAFETY: a key whose value is one is of the type, which @f has
        unsafe impl InFamily<$f> for <$ty as BkeyFamily>::Validated {}
        unsafe impl InFamily<$f> for <$ty as BkeyFamily>::Unvalidated {}
    )+ };
}

c::BkeyType_arms!(bkey_types);

bkey_family_conv!(AllTypes);
bkey_family_conv!(crate::data::ptrs::PtrsTypes);
bkey_family_conv!(crate::data::ptrs::PtrsMutTypes);
bkey_family_conv!(crate::data::reflink::IndirectTypes);
