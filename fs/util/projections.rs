// SPDX-License-Identifier: GPL-2.0

//! Field projections and places: what's projected through and assigned to,
//! and the fakes that stand in for projecting and for places until the
//! language has them.
//!
//! Field projections - a Rust project goal, which the Rust-for-Linux RcuMutex
//! is the motivating case for - will let a pointer type say what projecting
//! it to a field gives, and custom places what a load and a store through
//! one are: w.current_passes |= bit on a published word, sb.csum_type = x on
//! a superblock bitfield, v.btrees_lost_data on a short value. Until then,
//! in order - the fakes marked XXX where they are, to go when the real ones
//! arrive:
//!
//! - Places: FakePlaceRead and FakePlaceWrite, a place's load and store as
//!   methods - get(), set(), and place(): a FakePlaceRef, the value loaded
//!   to assign to through a &mut and stored back when it drops, so the real
//!   operators work - *w.nr.place() += 1, *w.passes.place() |= bit. Through
//!   &self, as a Cell's: what allows a store is how its writer was had - a
//!   held lock's guard, a &mut of what it's in - not the call. Fakes, all.
//!   The places here have get(), set() and place() inherent too, the traits
//!   calling them: callers need no import - fs.sb.version.get() - and the
//!   traits are for what's generic over places, FakePlaceRef first.
//!
//! - Publishing: Publish and published!(), projection faked by a mirror, for
//!   what a Locked publishes (locking/): a struct and its Writer,
//!   laid out the same, so the guard's published() is the &P::Writer of its
//!   own P, and field syntax reaches each leaf as its writer has it. The
//!   mirror goes; the lock stays.
//!
//! - Volatile words: Volatile, a word read by anyone and written one writer
//!   at a time - not an atomic, whose writers race each other - and
//!   VolatileWriter, it as its writer has it; VolatilePublish, one set only
//!   by showing the guard of the lock that publishes it. A value of more
//!   words is one only if it's TearSafe or a derived VolatileValue: a reader
//!   may see it torn, and its type says that's fine. They stay; their places
//!   go.
//!
//! - Published once: PublishedOnce, a value set once by its lock's holder
//!   and read by anyone from then on - a key a running filesystem gains,
//!   and never loses. Not a place: it's set, not stored to.
//!
//! - Views: View and ViewMut, a value as many of its bytes as there are -
//!   fewer, for one written before its type grew: a superblock section,
//!   journal entry or key value from an older version. A field past the end
//!   reads as zero, and setting one is an error, ShortField; a Floored
//!   type's fields every value has are fields of its View already. They
//!   stay; how they're projected - project(), and the v.field() methods
//!   #[derive(View)] makes of it, since a short value can't be a &T to
//!   project a reference from - goes. A View's fields read through
//!   FakePlaceRead; ViewMut's set() is its own, as it can fail.
//!
//! Bitfields aren't faked as places: a field access names memory that
//! exists, so *x.offset can't be a u64 loaded from its word - the copy has to
//! be made by a call, and then the call's receiver, & or &mut, is fixed.
//! They have their getters, set_x() and update_x(f) until the real ones
//! arrive.

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::mem::{align_of, size_of, transmute_copy, ManuallyDrop, MaybeUninit};
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicPtr, AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering, Ordering::Relaxed};

use crate::locking::{ExclusiveGuard, RawLock};
use crate::util::vstructs::{Native, NoUninit, Plain};

// ── Places ───────────────────────────────────────────────────────────────

/// A place to read: what a load through it gives.
pub trait FakePlaceRead {
    type Value: Copy;

    fn get(&self) -> Self::Value;
}

/// A place to write: a store through it.
pub trait FakePlaceWrite: FakePlaceRead {
    fn set(&self, v: Self::Value);

    /// It, as a place to assign to: *w.nr.place() += 1, *w.passes.place()
    /// |= bit - its value, loaded, to change through a &mut, and stored back
    /// when the FakePlaceRef drops, at the end of the statement.
    // XXX Deprecated when real field projections arrive: compound assignment
    // through a projected place - w.current_passes |= bit - instead.
    fn place(&self) -> FakePlaceRef<'_, Self>
    where
        Self: Sized,
    {
        FakePlaceRef { place: self, v: self.get(), dirty: false }
    }
}

/// A FakePlaceWrite's value, loaded, to change in place: stored back when
/// it drops, if it was changed - a FakePlaceRef only read stores nothing,
/// so *a.place() = (*a.place()).max(x) keeps the assignment.
///
/// A statement's worth, not a binding's: one held across another store to
/// the same place stores its stale copy over it when it drops. The borrow
/// checker doesn't see that - both borrow the place shared - so writers had
/// through &mut of what they're in, as a Locked's guard's published(),
/// keep the window to one writer reference.
#[must_use = "a FakePlaceRef stores when it drops - assign through it, *p += 1"]
pub struct FakePlaceRef<'a, P: FakePlaceWrite> {
    place: &'a P,
    v:     P::Value,
    dirty: bool,
}

impl<P: FakePlaceWrite> Deref for FakePlaceRef<'_, P> {
    type Target = P::Value;

    fn deref(&self) -> &P::Value {
        &self.v
    }
}

impl<P: FakePlaceWrite> DerefMut for FakePlaceRef<'_, P> {
    fn deref_mut(&mut self) -> &mut P::Value {
        self.dirty = true;
        &mut self.v
    }
}

impl<P: FakePlaceWrite> Drop for FakePlaceRef<'_, P> {
    fn drop(&mut self) {
        if self.dirty {
            self.place.set(self.v);
        }
    }
}

// ── Publishing ───────────────────────────────────────────────────────────

/// What a Locked publishes: read through &Self by anyone, held or not
/// - and written through &Self::Writer, which only its lock's guard hands
/// out: the same bytes, its leaves able to write. A struct of them is
/// published!(); its leaves are Volatile words and RcuArrays.
///
/// Writer is laid out as Self: a &Self is a valid &Writer. What Writer
/// writes, &Self's readers may read meanwhile: each leaf's writes are atomic,
/// or published as RCU publishes.
pub trait Publish {
    type Writer;

    /// Self as its writer has it: what the lock's guard hands out, for what
    /// holds the lock another way - a Locked's or LockedBy's own, or a lock
    /// of its owner's.
    ///
    /// # Safety
    /// The lock that publishes it is held, for as long as the reference lives.
    unsafe fn writer(&self) -> &Self::Writer
    where
        Self: Sized,
    {
        // Writer is laid out as Self
        unsafe { &*core::ptr::from_ref(self).cast::<Self::Writer>() }
    }
}

// nothing to write
impl Publish for () {
    type Writer = ();
}

// element for element, each T's
impl<T: Publish, const N: usize> Publish for [T; N] {
    type Writer = [T::Writer; N];
}

// its writes are atomic, so its writer is itself
impl Publish for core::sync::atomic::AtomicU64 {
    type Writer = Self;
}

// as AtomicU64's
impl Publish for core::sync::atomic::AtomicI64 {
    type Writer = Self;
}

// as AtomicU64's
impl<T> Publish for core::sync::atomic::AtomicPtr<T> {
    type Writer = Self;
}

// no bytes
impl Publish for crate::types::CacheAligned {
    type Writer = Self;
}

/// A struct a Locked publishes, and its writer: the struct as declared,
/// #[repr(C)], and NameWriter - each field as its writer has it, the Publish
/// Writer of its type - which the lock's guard hands out. Fields are Publish
/// types: Volatile words, RcuArrays, arrays of them, other published!()
/// structs.
#[macro_export]
macro_rules! published {
    (
        $(#[$m:meta])*
        $vis:vis struct $name:ident {
            $($(#[$fm:meta])* $fvis:vis $field:ident: $ty:ty),* $(,)?
        }
    ) => { ::paste::paste! {
        $(#[$m])*
        #[repr(C)]
        $vis struct $name {
            $($(#[$fm])* $fvis $field: $ty),*
        }

        #[doc = ::core::concat!(::core::stringify!($name), ", as its lock's holder has it.")]
        #[repr(C)]
        $vis struct [<$name Writer>] {
            $($fvis $field: <$ty as $crate::util::projections::Publish>::Writer),*
        }

        // field for field, each its type's Writer - laid out as it is
        // (Publish), and checked
        impl $crate::util::projections::Publish for $name {
            type Writer = [<$name Writer>];
        }

        const _: () = {
            assert!(::core::mem::size_of::<$name>() == ::core::mem::size_of::<[<$name Writer>]>());
            assert!(::core::mem::align_of::<$name>() == ::core::mem::align_of::<[<$name Writer>]>());
            $(assert!(::core::mem::offset_of!($name, $field) ==
                      ::core::mem::offset_of!([<$name Writer>], $field));)*
        };
    } };
}

// ── Volatile words ───────────────────────────────────────────────────────

/// A value whose readers are fine with seeing it torn - each word as some
/// write left it, not all from the same one: a position read for progress,
/// say. A Volatile of it is loaded and stored a word at a time - of its
/// alignment, up to a u64 - and it's Plain, so a torn one, any bytes, is a
/// value. One with a bool field, 0 or 1 in every value, derives VolatileValue
/// instead, field by field.
#[rustc_specialization_trait]
pub trait TearSafe: Plain {}

/// How a Volatile loads and stores a T: whole, by one atomic of its size - a
/// build error if it's more than a word at its alignment - or, if it's
/// TearSafe, a word at a time, as integers. Or, derived, field by field, each
/// as its own Volatile would: its padding never read.
///
/// # Safety
/// load() and store() read and write @p only by atomics, of what it is.
pub unsafe trait VolatileValue: Sized {
    /// # Safety
    /// @p is a live T, accessed only through VolatileValue meanwhile.
    unsafe fn load(p: *mut Self) -> Self;
    /// # Safety
    /// As load()'s.
    unsafe fn store(p: *mut Self, v: Self);
}

/// T's size, one word at its alignment - or a build error.
const fn one_word<T>() -> usize {
    assert!(size_of::<T>() == align_of::<T>() && size_of::<T>() <= size_of::<u64>(),
            "a Volatile of more than a word at its alignment: is it TearSafe, or a derived VolatileValue?");
    size_of::<T>()
}

/// @body, @a the atomic of @T's size at @p: unsafe, as from_ptr() is. A
/// pointer-sized @T's is an AtomicPtr, so a pointer keeps its provenance.
macro_rules! with_atomic {
    ($t:ty, $p:expr, |$a:ident| $body:expr) => {
        match const { one_word::<$t>() } {
            n if n == size_of::<usize>() => { let $a = AtomicPtr::<u8>::from_ptr($p.cast()); $body }
            1 => { let $a = AtomicU8::from_ptr($p.cast()); $body }
            2 => { let $a = AtomicU16::from_ptr($p.cast()); $body }
            4 => { let $a = AtomicU32::from_ptr($p.cast()); $body }
            _ => { let $a = AtomicU64::from_ptr($p.cast()); $body }
        }
    };
}

unsafe impl<T: NoUninit> VolatileValue for T {
    default unsafe fn load(p: *mut T) -> T {
        unsafe { with_atomic!(T, p, |a| transmute_copy(&a.load(Relaxed))) }
    }

    default unsafe fn store(p: *mut T, v: T) {
        let v = ManuallyDrop::new(v);
        unsafe { with_atomic!(T, p, |a| a.store(transmute_copy(&*v), Relaxed)) }
    }
}

/// A TearSafe T's word: its alignment, up to a u64's - which a T is a whole
/// number of.
const fn word<T>() -> usize {
    if align_of::<T>() < size_of::<u64>() { align_of::<T>() } else { size_of::<u64>() }
}

/// @body, @W the integer of a TearSafe @T's word.
macro_rules! with_word {
    ($t:ty, |$w:ident| $body:expr) => {
        match word::<$t>() {
            1 => { type $w = u8; $body }
            2 => { type $w = u16; $body }
            4 => { type $w = u32; $body }
            _ => { type $w = u64; $body }
        }
    };
}

/// # Safety
/// @src and @dst are @n words each, aligned, as VolatileValue::load()'s.
unsafe fn load_words<W: VolatileValue>(src: *mut W, dst: *mut W, n: usize) {
    for i in 0..n {
        unsafe { dst.add(i).write(W::load(src.add(i))) }
    }
}

/// # Safety
/// As load_words()'s.
unsafe fn store_words<W: VolatileValue>(src: *const W, dst: *mut W, n: usize) {
    for i in 0..n {
        unsafe { W::store(dst.add(i), src.add(i).read()) }
    }
}

unsafe impl<T: NoUninit + TearSafe> VolatileValue for T {
    default unsafe fn load(p: *mut T) -> T {
        let mut v = MaybeUninit::<T>::uninit();
        let (dst, n) = (v.as_mut_ptr(), size_of::<T>() / word::<T>());
        unsafe {
            with_word!(T, |W| load_words::<W>(p.cast(), dst.cast(), n));
            v.assume_init()
        }
    }

    default unsafe fn store(p: *mut T, v: T) {
        let v = ManuallyDrop::new(v);
        let (src, n) = (core::ptr::from_ref(&*v), size_of::<T>() / word::<T>());
        unsafe { with_word!(T, |W| store_words::<W>(src.cast(), p.cast(), n)) }
    }
}

/// A @T read by anyone, a word at a time, as C reads its fields. Relaxed
/// loads and stores, as C's plain ones: a reader sees each word as it was
/// written, not two of them as of one moment. To C, a plain @T. A @T of
/// one word at its alignment is loaded and stored whole; one of more is
/// TearSafe, or a derived VolatileValue.
///
/// Volatile, not atomic: its writers don't race each other, only its
/// readers. set() is a store, for a field with one writer - a thread's own
/// state, published - and a read-modify-write is a load and a store - place()
/// - not an atomic one, so it's a VolatileWriter's: had where writing is
/// serialized, a Locked's guard. A word several writers change at once is an
/// atomic, written with its fetch_add()s, and never a FakePlaceWrite: a
/// place's load and store would lose another writer's update between them.
#[repr(transparent)]
#[derive(::pin_init::Zeroable)]
pub struct Volatile<T>(UnsafeCell<T>);

unsafe impl<T: Send> Sync for Volatile<T> {}

impl<T> Volatile<T> {
    pub const fn new(v: T) -> Self {
        Self(UnsafeCell::new(v))
    }

    /// It, had whole: through a &mut, nothing reads it meanwhile.
    pub fn get_mut(&mut self) -> &mut T {
        self.0.get_mut()
    }
}

impl<T: Copy + VolatileValue> Volatile<T> {
    pub fn get(&self) -> T {
        unsafe { VolatileValue::load(self.0.get()) }
    }

    /// Store @v, by its one writer.
    pub fn set(&self, v: T) {
        unsafe { VolatileValue::store(self.0.get(), v) }
    }
}

impl<T: NoUninit> Volatile<T> {
    /// What it holds, copied but not owned: it's still the cell's, and drops
    /// with it.
    ///
    /// # Safety
    /// What the copy refers to stays alive while it's used - the cell's value
    /// isn't replaced and dropped meanwhile, or not freed till a grace period
    /// on.
    pub unsafe fn get_unowned(&self) -> ManuallyDrop<T> {
        ManuallyDrop::new(unsafe { VolatileValue::load(self.0.get()) })
    }

    /// @v in, what it held out: one atomic swap, so two writers can't both
    /// take the same value.
    pub fn replace(&self, v: T) -> T {
        let (p, v) = (self.0.get(), ManuallyDrop::new(v));
        unsafe { with_atomic!(T, p, |a| transmute_copy(&a.swap(transmute_copy(&*v), Relaxed))) }
    }
}

impl<T: Copy + VolatileValue> FakePlaceRead for Volatile<T> {
    type Value = T;

    fn get(&self) -> T {
        Volatile::get(self)
    }
}

/// Not Copy - it's read while it's written - so a copy is a load, spelled out.
impl<T: Copy + VolatileValue> Clone for Volatile<T> {
    fn clone(&self) -> Self {
        Self::new(self.get())
    }
}

impl<T: Default> Default for Volatile<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

unsafe impl<T: crate::util::vstructs::Plain> crate::util::vstructs::Plain for Volatile<T> {}
unsafe impl<T: NoUninit> NoUninit for Volatile<T> {}

// VolatileWriter is a Volatile; its stores are atomic
impl<T> Publish for Volatile<T> {
    type Writer = VolatileWriter<T>;
}

/// A Volatile, as its writer has it: the other writers excluded, so a
/// read-modify-write through place() is whole.
#[repr(transparent)]
pub struct VolatileWriter<T>(Volatile<T>);

impl<T: Copy + VolatileValue> VolatileWriter<T> {
    pub fn get(&self) -> T {
        self.0.get()
    }

    pub fn set(&self, v: T) {
        self.0.set(v)
    }

    /// FakePlaceWrite::place(): *w.nr.place() += 1.
    // XXX Deprecated when real field projections arrive, as that is.
    pub fn place(&self) -> FakePlaceRef<'_, Self> {
        FakePlaceWrite::place(self)
    }
}

impl<T: Copy + VolatileValue> FakePlaceRead for VolatileWriter<T> {
    type Value = T;

    fn get(&self) -> T {
        VolatileWriter::get(self)
    }
}

impl<T: Copy + VolatileValue> FakePlaceWrite for VolatileWriter<T> {
    fn set(&self, v: T) {
        VolatileWriter::set(self, v)
    }
}

/// A Volatile word published by the lock that protects a @G: read by anyone,
/// set by whoever shows its guard.
#[repr(transparent)]
#[derive(::pin_init::Zeroable)]
pub struct VolatilePublish<T, G>(Volatile<T>, PhantomData<fn() -> G>);

impl<T: Copy + VolatileValue, G> VolatilePublish<T, G> {
    pub fn get(&self) -> T {
        self.0.get()
    }

    /// get(), and what the set_release() that stored it was ordered after.
    pub fn get_acquire(&self) -> T {
        let v = self.0.get();
        core::sync::atomic::fence(Ordering::Acquire);
        v
    }

    pub fn set<R: RawLock, P>(&self, v: T, _: &ExclusiveGuard<'_, R, G, P>) {
        self.0.set(v)
    }

    /// set(), ordered after the stores before it: for what get_acquire()'s
    /// reader goes on to read.
    pub fn set_release<R: RawLock, P>(&self, v: T, _: &ExclusiveGuard<'_, R, G, P>) {
        core::sync::atomic::fence(Ordering::Release);
        self.0.set(v)
    }
}

// ── Published once ───────────────────────────────────────────────────────

#[cfg(kernel)]
use kernel::sync::SetOnce;
#[cfg(not(kernel))]
use bcachefs_shim::sync::SetOnce;

/// Unset, or set for good: read without the lock, set only by its holder,
/// once - PublishedOnceWriter. Zeroed, it's unset.
#[repr(transparent)]
pub struct PublishedOnce<T>(SetOnce<T>);

impl<T> PublishedOnce<T> {
    pub fn get(&self) -> Option<&T> {
        self.0.as_ref()
    }
}

/// A PublishedOnce, as its lock's holder has it.
#[repr(transparent)]
pub struct PublishedOnceWriter<T>(SetOnce<T>);

impl<T> PublishedOnceWriter<T> {
    /// Whether this set it: if it was already, @v is dropped.
    pub fn set(&self, v: T) -> bool {
        self.0.populate(v)
    }
}

// SAFETY: SetOnce's zeroes are its unset state
unsafe impl<T> ::pin_init::Zeroable for PublishedOnce<T> {}

// the same SetOnce, whose set is atomic
impl<T> Publish for PublishedOnce<T> {
    type Writer = PublishedOnceWriter<T>;
}

// ── Views ────────────────────────────────────────────────────────────────

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
    // XXX Deprecated when real field projections arrive, with the methods
    // #[derive(View)] makes of it: e.btrees_lost_data() becomes
    // e.btrees_lost_data.
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
    /// - to the value's end.
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

impl<T: Plain + Native> View<'_, T> {
    /// Its value - the integer, for a little-endian one: what's past the end
    /// reads as zero.
    pub fn get(&self) -> T::Native {
        self.read().to_native()
    }
}

impl<T: Plain + Native> FakePlaceRead for View<'_, T>
where
    T::Native: Copy,
{
    type Value = T::Native;

    fn get(&self) -> T::Native {
        View::get(self)
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
    // XXX Deprecated when real field projections arrive, as View's.
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

impl<T: Plain + Native> FakePlaceRead for ViewMut<'_, T>
where
    T::Native: Copy,
{
    type Value = T::Native;

    fn get(&self) -> T::Native {
        ViewMut::get(self)
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

/// A type whose values may be shorter than it, but never shorter than Min:
/// the fields every valid one has. A View of it derefs to them, so they're
/// fields - v.subvol - and only the later ones are projected, reading as
/// zero past the end. A sized key type's value is one, Min all of it: a key
/// type that grew is a #[grown] family.
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
    /// wasn't made by what floors it - a typed key's v() - and is a bug.
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
