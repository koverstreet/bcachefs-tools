// SPDX-License-Identifier: GPL-2.0

//! A value's place: the field of a parent it lives at - and container_of()
//! from it, safe because the type system knows the value is there.
//!
//! container_of() needs two things true: the value is a P's field - its
//! location - and the P is valid while it's used. This is the first: a type
//! that's Placed is, every value of it, its parent's field N, so its
//! container() is a safe call. The parent's validity is the parent's own:
//! a &child borrowed from it is had only while it is.
//!
//! #[placed(Fs.snapshots)] on the type is all that's written, on it or its
//! parent - cstruct-macros' place.rs is what it makes:
//!
//! - Field<N> is a struct's field N, a hash of its name - field!(snapshots) -
//!   with its type and offset: a field as a type, which the language doesn't
//!   have yet. It's field representing types' Field (field_of!(), nightly),
//!   faked as util/projections.rs's fakes are, to go when they arrive.
//!   #[derive(Init)] makes one for each field of the structs it's on.
//!
//! - At<B, N> is the proof: a field the placed type holds, zero bytes, that
//!   only at() makes. Nothing else can - its fields are private, and it's
//!   not Clone, Default or Zeroable - so neither a literal nor a zeroed value
//!   nor a pin_init!() of the type can be had anywhere: a value of it is one
//!   at() made.
//!
//! - at() makes it, called only by InitAt<N>'s init(): B's initializer for
//!   field N, which #[placed] implements on B and B's #[derive(Init)] picks
//!   for that field - Pick. So the At a field's value is given is its own
//!   field's: an At is made only in the line that puts it in place. (A
//!   PinInit can be run anywhere: that's why it can't be a value anyone
//!   holds, and InitAt::init() is unsafe.)
//!
//! - AtIndex<B, N, I> is an array element's: #[placed(B.field[], index =
//!   name: I)] - its index, which C sees as the field it replaces, as C named
//!   it, and at_index() writes for each element as the array's InitAt makes
//!   them. Its container() is the array's offset, plus that many elements.
//!
//! - Placed is the child's side - its Parent, and its At, which ties the two
//!   - and container() the field's address back to its parent's.
//!
//! What keeps a placed value at its place is that no value of it can be had
//! anywhere else - not Pin. A swap of two, through &muts, is harmless: each
//! is still some parent's field N, and nothing else can be swapped in -
//! but not two array elements, each holding its index; and that takes a
//! &mut to the array, the parent's discipline again. The way out is the
//! parent's: had by value, it can be taken apart - let P {
//! child, .. } = p - leaving the child nowhere. Fs never is: it lives in its
//! pinned allocation, a &Fs to everyone. At is !Unpin all the same, for what
//! needs its address to stay put - C's &c->snapshots.
//!
//! container() widens a reference to the child to one to its parent. Under
//! Tree Borrows that's allowed, the parent's bytes outside the child getting
//! the permission the child's type implies: read-only, from a type with no
//! interior mutability - writing an atomic of the parent's through the &P
//! would be UB - and as a shared reference's, from one with some. So At
//! holds an UnsafeCell, and every placed type has interior mutability.
//!
//! Known gaps, to come back to:
//!
//! - The parent's own discipline. A &mut P lets a &p.child and writes to p's
//!   other fields coexist - split borrows, the paths disjoint - and the &P
//!   container() makes of the child would see those fields change under it:
//!   UB. The borrow checker could see that a &child's container() borrows
//!   all of P, and doesn't know to. And a P had by value can be taken apart,
//!   as above. So container() is sound only for a parent never had by &mut
//!   or by value: Fs. Unchecked.
//!
//! - Drop. Fields drop in order, so a placed type's own drop would find its
//!   parent's earlier fields dropped, its container() a &P to them. So it
//!   has none: #[placed] refuses #[pin_data(PinnedDrop)], pin_data()'s
//!   check refuses a Drop, and a struct without #[pin_data] gets an empty
//!   Drop for one to conflict with. A placed type that needs teardown has
//!   nowhere to put it yet: when one does, its hook must be had without
//!   container().
//!
//! An element of a container - #[placed(Base.field[..])] - has a place too,
//! but no address in it: it's in the Container at Base's field, wherever
//! the container keeps it. There's no container() for one; what its place
//! is for is the guarantee that it never outlives its Base. A Container is
//! made only at its field - its InitAt, by container() - and its insert()
//! is the only thing that makes an AtIn, so a T is had only from the
//! container's insert(); Holds, the container proper's promise, keeps it
//! there - made in place, dropped in place, never had by value - until the
//! container drops it, with itself. An element may have a Drop of its own:
//! its container's whole while it runs.
//!
//! An AtIn is given to the element's initializer, and only to that: what
//! insert() takes is a PlacedInit - sealed, its one implementation made by
//! placed_init!(), which is written as pin_init!() is and puts the AtIn in
//! _placed_at, under a name its fields can't name. So the code that wrote
//! an initializer never has the AtIn - to keep, and make a T somewhere else
//! with.
//!
//! A type lives at one place. One that lives at several, as a kobject or a
//! work_struct does, isn't Placed: C hands those back as pointers, and how
//! a pointer was made - projected from its container, by the only code that
//! hands it to C - is what locates them.

use crate::errcode::BchError;
use crate::util::alloc::{flags::GFP_KERNEL, kbox_pin_init, AllocError, KBox, KVVec};
use core::cell::UnsafeCell;
use core::marker::{PhantomData, PhantomPinned};
use core::mem::size_of;
use core::pin::Pin;
use pin_init::{Init, PinInit, Zeroable};

/// A field's name, as Field<N>'s N: its FNV-1a hash.
pub const fn name(s: &str) -> u64 {
    let b = s.as_bytes();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut i = 0;
    while i < b.len() {
        h ^= b[i] as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
        i += 1;
    }
    h
}

/// Field @f's N, for Field<N> and At<B, N>: `At<Fs, { field!(snapshots) }>`.
#[macro_export]
macro_rules! field {
    ($f:ident) => {
        $crate::util::place::name(stringify!($f))
    };
}

/// Self's field N: its type, and where it is.
///
/// # Safety
/// Self has a field of type Type at OFFSET, named N.
pub(crate) unsafe trait Field<const N: u64> {
    type Type;
    const OFFSET: usize;
}

/// Proof that what holds it was made at B's field N - by at(), which alone
/// makes one. Zero bytes, as C's char[0] for it: FFI-safe, as CacheAligned
/// is, for the C structs that hold one. The UnsafeCell, of no bytes, is so
/// container() widens to a usable &P - see the file comment.
#[repr(C)]
pub(crate) struct At<B, const N: u64> {
    _c:   UnsafeCell<[u8; 0]>,
    _b:   PhantomData<fn() -> B>,
    /// !Unpin - a PhantomData of it, which improper_ctypes skips, as it
    /// doesn't a PhantomPinned field
    _pin: PhantomData<PhantomPinned>,
}

// SAFETY: no bytes, for threads to race on
unsafe impl<B, const N: u64> Sync for At<B, N> {}

mod sealed {
    pub(crate) trait Sealed {}
}

/// What an At says: B's field N - the type there, and where in B the value
/// holding the At is.
pub(crate) trait Place: sealed::Sealed {
    type Base;
    type Type;
    fn offset(&self) -> usize;
}

impl<B, const N: u64> sealed::Sealed for At<B, N> {}

impl<B: Field<N>, const N: u64> Place for At<B, N> {
    type Base = B;
    type Type = B::Type;
    fn offset(&self) -> usize {
        B::OFFSET
    }
}

/// An index, as an array's element holds its own: C's integer for it.
pub(crate) trait Index: Copy {
    fn from_usize(i: usize) -> Self;
    fn to_usize(self) -> usize;
}

macro_rules! index {
    ($($t:ty),*) => {$(
        impl Index for $t {
            fn from_usize(i: usize) -> Self {
                Self::try_from(i).expect("an array index past its integer")
            }
            fn to_usize(self) -> usize {
                self as usize
            }
        }
    )*};
}
index!(u8, u16, u32, u64, usize);

/// An array, by its element type.
pub(crate) trait Array {
    type Elem;
}

impl<T, const L: usize> Array for [T; L] {
    type Elem = T;
}

/// Proof that what holds it was made as element @index of B's array field N
/// - by at_index(), which alone makes one. The index itself to C: the field
/// it's in is the element's own index, as C named it.
#[repr(transparent)]
pub(crate) struct AtIndex<B, const N: u64, I> {
    index: I,
    _c:    UnsafeCell<[u8; 0]>,
    _b:    PhantomData<fn() -> B>,
    _pin:  PhantomData<PhantomPinned>,
}

// SAFETY: its index is never written after at_index() made it
unsafe impl<B, const N: u64, I: Sync> Sync for AtIndex<B, N, I> {}

impl<B, const N: u64, I: Index> AtIndex<B, N, I> {
    /// Which element it is, as C's integer for it.
    pub(crate) fn get(&self) -> I {
        self.index
    }
}

impl<B, const N: u64, I> sealed::Sealed for AtIndex<B, N, I> {}

impl<B: Field<N>, const N: u64, I: Index> Place for AtIndex<B, N, I>
where
    B::Type: Array,
{
    type Base = B;
    type Type = <B::Type as Array>::Elem;
    fn offset(&self) -> usize {
        B::OFFSET + self.index.to_usize() * size_of::<Self::Type>()
    }
}

/// The At for B's field N, given to @init - the field's type's initializer -
/// for the initializer it returns. Only #[placed]'s InitAt calls it.
///
/// # Safety
/// What it returns is run into B's field N, of a B being initialized.
#[doc(hidden)]
pub(crate) unsafe fn at<B: Field<N>, const N: u64, I, E>(init: impl FnOnce(At<B, N>) -> I) -> I
where
    I: PinInit<B::Type, E>,
{
    init(At { _c: UnsafeCell::new([]), _b: PhantomData, _pin: PhantomData })
}

/// The AtIndex for element @i of B's array field N, given to @init - the
/// element type's initializer - for the initializer it returns. Only
/// #[placed]'s InitAt calls it, for each element.
///
/// # Safety
/// What it returns is run into element @i of B's field N, of a B being
/// initialized.
#[doc(hidden)]
pub(crate) unsafe fn at_index<B: Field<N>, const N: u64, X: Index, I, E>(
    i:    usize,
    init: impl FnOnce(AtIndex<B, N, X>) -> I,
) -> I
where
    B::Type: Array,
    I: PinInit<<B::Type as Array>::Elem, E>,
{
    init(AtIndex { index: X::from_usize(i), _c: UnsafeCell::new([]), _b: PhantomData, _pin: PhantomData })
}

/// B's field N's initializer, where a placed type lives there: #[placed]
/// on the type implements it, its At from at(), for B's #[derive(Init)] to
/// pick for the field.
///
/// # Safety
/// init() gives its field an At: what it returns is run into B's field N.
#[doc(hidden)]
pub(crate) unsafe trait InitAt<const N: u64>: Field<N> {
    /// # Safety
    /// What it returns is run into Self's field N, of a Self being
    /// initialized.
    unsafe fn init() -> impl PinInit<Self::Type, BchError>;
}

/// #[derive(Init)]'s initializer for B's field N, without #[init]: its
/// InitAt, if a placed type lives there, else zeroed - picked by which
/// trait's pick() method resolution finds first, PickPlaced's on a Pick,
/// before PickZeroed's on a &Pick. A wrong pick doesn't compile: a placed
/// type isn't Zeroable, and its init() takes only its own field's At. A
/// placed field must be #[pin]: its initializer's only a PinInit.
#[doc(hidden)]
pub(crate) struct Pick<B, const N: u64>(PhantomData<fn() -> B>);

impl<B, const N: u64> Pick<B, N> {
    pub(crate) fn new() -> Self {
        Pick(PhantomData)
    }
}

#[doc(hidden)]
pub(crate) trait PickPlaced<T> {
    /// # Safety
    /// As InitAt::init()'s.
    unsafe fn pick(&self) -> impl PinInit<T, BchError>;
}

impl<B: InitAt<N>, const N: u64> PickPlaced<B::Type> for Pick<B, N> {
    unsafe fn pick(&self) -> impl PinInit<B::Type, BchError> {
        // SAFETY: the caller's
        unsafe { B::init() }
    }
}

#[doc(hidden)]
pub(crate) trait PickZeroed<T> {
    /// # Safety
    /// None: unsafe as PickPlaced's is, the call being the same.
    unsafe fn pick(&self) -> impl Init<T>;
}

impl<B: Field<N>, const N: u64> PickZeroed<B::Type> for &Pick<B, N>
where
    B::Type: Zeroable,
{
    unsafe fn pick(&self) -> impl Init<B::Type> {
        pin_init::init_zeroed()
    }
}

/// Every Self is its parent's field: it holds an At, which at() made where
/// the At says. #[placed] implements it.
///
/// # Safety
/// Self has a field of type Self::At, so a Self is had only from an
/// initializer at() was given.
pub(crate) unsafe trait Placed: Sized {
    /// What it's the field of.
    type Parent;
    /// Its proof - which ties Parent to where at() made it.
    #[doc(hidden)]
    type At: Place<Type = Self, Base = Self::Parent>;

    /// Its At field, of the Self at @p.
    ///
    /// # Safety
    /// @p points to a Self.
    #[doc(hidden)]
    unsafe fn placed_at_raw(p: *const Self) -> *const Self::At;

    /// The parent this is the field of.
    fn container(&self) -> &Self::Parent {
        // SAFETY: a Self, borrowed
        unsafe { container_of(self) }
    }
}

/// container(), from a pointer: for a placed value had only by one - behind
/// an Opaque, which C writes through.
///
/// # Safety
/// @p points to a live Self, initialized - had for 'a.
pub(crate) unsafe fn container_of<'a, T: Placed>(p: *const T) -> &'a T::Parent {
    // SAFETY: Placed's - p is in a parent, its At's offset() in it, the At
    // fixed once made; had for 'a, so the parent's live for 'a. Its known
    // gap, the parent's discipline: see the file comment
    unsafe {
        let at = &*T::placed_at_raw(p);
        &*p.byte_sub(at.offset()).cast::<T::Parent>()
    }
}

// ── Elements of a container ──────────────────────────────────────────────

/// Every Self lives in the container at Base's field - a Container, which
/// Self's #[placed(Base.field[..])] makes there: a Self is had only from
/// that container's insert(), and dies in it. So none outlives its Base.
///
/// # Safety
/// Self has a field of type AtIn<Self>, so a Self is had only from a
/// PlacedInit its Container's insert() ran.
pub(crate) unsafe trait PlacedIn: Sized {
    type Base;
}

/// Proof that what holds it was made by its container's insert(), which
/// alone makes one. Zero bytes, as At, and as At an UnsafeCell and !Unpin.
#[repr(C)]
pub(crate) struct AtIn<T> {
    _c:   UnsafeCell<[u8; 0]>,
    _t:   PhantomData<fn() -> T>,
    _pin: PhantomData<PhantomPinned>,
}

// SAFETY: no bytes, for threads to race on
unsafe impl<T> Sync for AtIn<T> {}

/// What a Container keeps its elements in.
///
/// # Safety
/// An element gets in only by insert(), made in place and never moved; it
/// leaves only by retain(), dropped; and what's held is dropped when the
/// container is. No element is had by value, before or after.
pub(crate) unsafe trait Holds<T>: Sized {
    fn empty() -> Self;
    fn insert<E: From<AllocError>>(&mut self, init: impl PinInit<T, E>) -> Result<(), E>;
    fn iter<'a>(&'a self) -> impl Iterator<Item = &'a T> where T: 'a;
    /// Keep the elements @f says to; drop the rest.
    fn retain(&mut self, f: impl FnMut(&T) -> bool);
}

// SAFETY: each element in a pinned box of its own, made there; room for
// its box is made first, so an element made is an element held
unsafe impl<T> Holds<T> for KVVec<Pin<KBox<T>>> {
    fn empty() -> Self {
        KVVec::new()
    }

    fn insert<E: From<AllocError>>(&mut self, init: impl PinInit<T, E>) -> Result<(), E> {
        self.reserve(1, GFP_KERNEL)?;
        let b = kbox_pin_init(init)?;
        self.push(b, GFP_KERNEL).expect("room reserved");
        Ok(())
    }

    fn iter<'a>(&'a self) -> impl Iterator<Item = &'a T> where T: 'a {
        self[..].iter().map(|b| &**b)
    }

    fn retain(&mut self, mut f: impl FnMut(&T) -> bool) {
        KVVec::retain(self, |b| f(&**b))
    }
}

/// The container of a placed field's elements: T's #[placed(Base.field[..])]
/// makes it, at Base's field - its InitAt, by container(), the only way to
/// have one - and its insert() is the only thing that makes a T. It keeps
/// its C to itself: no &mut C, and no T by value, for an element to leave
/// by.
pub(crate) struct Container<T, C> {
    c:  C,
    _t: PhantomData<fn() -> T>,
}

impl<T: PlacedIn, C: Holds<T>> Container<T, C> {
    /// Make an element here, by @init - placed_init!()'s, given its AtIn.
    pub(crate) fn insert<E: From<AllocError>>(&mut self, init: impl PlacedInit<T, E>) -> Result<(), E> {
        self.c.insert(init.__init(AtIn { _c: UnsafeCell::new([]), _t: PhantomData, _pin: PhantomData }))
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &T> {
        self.c.iter()
    }

    /// Keep the elements @f says to; drop the rest.
    pub(crate) fn retain(&mut self, f: impl FnMut(&T) -> bool) {
        self.c.retain(f)
    }
}

/// An element's initializer, for its container's insert(): placed_init!()'s,
/// which is pin_init!()'s, but for the element's _placed_at - which it's
/// given the AtIn for, by insert(). Sealed, its one implementation made by
/// the macro alone: so the code that wrote the initializer never has the
/// AtIn, to put anywhere else.
pub(crate) trait PlacedInit<T, E>: sealed::Sealed {
    #[doc(hidden)]
    fn __init(self, at: AtIn<T>) -> impl PinInit<T, E>;
}

/// placed_init!()'s PlacedInit: a closure of the AtIn.
#[doc(hidden)]
pub(crate) struct InitIn<T, F>(F, PhantomData<fn() -> T>);

impl<T, F> InitIn<T, F> {
    /// # Safety
    /// @f puts the AtIn it's given in the T's _placed_at, and nowhere else:
    /// placed_init!()'s.
    #[doc(hidden)]
    pub(crate) unsafe fn __new(f: F) -> Self {
        InitIn(f, PhantomData)
    }
}

impl<T, F> sealed::Sealed for InitIn<T, F> {}

impl<T, E, I: PinInit<T, E>, F: FnOnce(AtIn<T>) -> I> PlacedInit<T, E> for InitIn<T, F> {
    fn __init(self, at: AtIn<T>) -> impl PinInit<T, E> {
        (self.0)(at)
    }
}

/// An element's initializer, written as pin_init!()'s is, but for its
/// _placed_at: that's its container's to give it, by insert().
///
/// ```text
/// fn new(n: u64) -> impl PlacedInit<Self, BchError> {
///     placed_init!(Self { n }? BchError)
/// }
/// // elems.insert(Elem::new(n))?
/// ```
#[macro_export]
macro_rules! placed_init {
    ($t:path { $($fields:tt)* } $(? $e:ty)?) => {{
        // the AtIn's name is the macro's: the fields can't name it
        let f = move |__placed_at| ::pin_init::pin_init!($t { _placed_at: __placed_at, $($fields)* } $(? $e)?);
        // SAFETY: the AtIn goes in _placed_at, and nowhere else
        unsafe { $crate::util::place::InitIn::__new(f) }
    }};
}

/// The Container for B's field N - its InitAt's, which #[placed] on the
/// element type makes.
///
/// # Safety
/// What it returns is run into B's field N, of a B being initialized.
#[doc(hidden)]
pub(crate) unsafe fn container<B, const N: u64, T, C>() -> impl PinInit<Container<T, C>, BchError>
where
    B: Field<N, Type = Container<T, C>>,
    T: PlacedIn<Base = B>,
    C: Holds<T>,
{
    // SAFETY: all of it written
    unsafe {
        pin_init::init_from_closure(|slot: *mut Container<T, C>| {
            slot.write(Container { c: C::empty(), _t: PhantomData });
            Ok(())
        })
    }
}
