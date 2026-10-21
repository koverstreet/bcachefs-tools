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
//!   what a Lock publishes (locking/): a struct and its Writer,
//!   laid out the same, so the guard's published() is the &P::Writer of its
//!   own P, and field syntax reaches each leaf as its writer has it. The
//!   mirror goes; the lock stays.
//!
//! - Volatile words: Volatile, a word read by anyone and written one writer
//!   at a time - not an atomic, whose writers race each other - and
//!   VolatileWriter, it as its writer has it. They stay; their places go.
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

use core::marker::PhantomData;
use core::mem::{align_of, size_of, MaybeUninit};
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering};

use crate::util::vstructs::{Native, Plain};

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
/// through &mut of what they're in, as a Lock's guard's published(),
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

/// What a Lock publishes: read through &Self by anyone, held or not
/// - and written through &Self::Writer, which only its lock's guard hands
/// out: the same bytes, its leaves able to write. A struct of them is
/// published!(); its leaves are Volatile words and RcuArrays.
///
/// # Safety
/// Writer is laid out as Self: a &Self is a valid &Writer. What Writer
/// writes, &Self's readers may read meanwhile: each leaf's writes are atomic,
/// or published as RCU publishes.
pub unsafe trait Publish {
    type Writer;
}

// SAFETY: nothing to write
unsafe impl Publish for () {
    type Writer = ();
}

// SAFETY: element for element, each T's
unsafe impl<T: Publish, const N: usize> Publish for [T; N] {
    type Writer = [T::Writer; N];
}

/// A struct a Lock publishes, and its writer: the struct as declared,
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

        // SAFETY: field for field, each its type's Writer - laid out as it is
        // (Publish), and checked
        unsafe impl $crate::util::projections::Publish for $name {
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

/// A value with an atomic of its own size: what a Volatile is stored as.
///
/// # Safety
/// Atomic is laid out as Self is, and all zeroes is a Self: a Volatile in
/// zeroed memory - as C allocates its structs - is a valid one.
pub unsafe trait AtomicWord: Copy {
    type Atomic;

    fn load(a: &Self::Atomic) -> Self;
    fn store(a: &Self::Atomic, v: Self);
}

macro_rules! atomic_word {
    ($($t:ty => $a:ty),* $(,)?) => { $(
        // SAFETY: core's atomic of the same type; zeroes are 0, or false
        unsafe impl AtomicWord for $t {
            type Atomic = $a;

            fn load(a: &$a) -> $t {
                a.load(Ordering::Relaxed)
            }

            fn store(a: &$a, v: $t) {
                a.store(v, Ordering::Relaxed)
            }
        }
    )* };
}
atomic_word!(bool => AtomicBool, u8 => AtomicU8, u16 => AtomicU16, u32 => AtomicU32, u64 => AtomicU64);

/// A @T read by anyone, a word at a time, as C reads its fields, and written
/// only by a writer had where writing it is serialized - a Lock's
/// guard: a VolatileWriter. Relaxed loads and stores, as C's plain ones: a
/// reader sees each word as it was written, not two of them as of one
/// moment. To C, a plain @T.
///
/// Volatile, not atomic: its writers don't race each other, only its
/// readers, so a read-modify-write is a load and a store - place() - not an
/// atomic one. A word several writers change at once is an atomic, written
/// with its fetch_add()s, and never a FakePlaceWrite: a place's load and
/// store would lose another writer's update between them.
#[repr(transparent)]
pub struct Volatile<T: AtomicWord>(T::Atomic);

impl<T: AtomicWord> Volatile<T> {
    pub fn get(&self) -> T {
        T::load(&self.0)
    }
}

impl<T: AtomicWord> FakePlaceRead for Volatile<T> {
    type Value = T;

    fn get(&self) -> T {
        Volatile::get(self)
    }
}

// SAFETY: AtomicWord's - zeroes are a @T
unsafe impl<T: AtomicWord> ::pin_init::Zeroable for Volatile<T> {}

// SAFETY: VolatileWriter is a Volatile; its stores are atomic
unsafe impl<T: AtomicWord> Publish for Volatile<T> {
    type Writer = VolatileWriter<T>;
}

/// A Volatile, as its writer has it: the other writers excluded, so a
/// read-modify-write through place() is whole.
#[repr(transparent)]
pub struct VolatileWriter<T: AtomicWord>(Volatile<T>);

impl<T: AtomicWord> VolatileWriter<T> {
    pub fn get(&self) -> T {
        self.0.get()
    }

    pub fn set(&self, v: T) {
        T::store(&self.0.0, v)
    }

    /// FakePlaceWrite::place(): *w.nr.place() += 1.
    // XXX Deprecated when real field projections arrive, as that is.
    pub fn place(&self) -> FakePlaceRef<'_, Self> {
        FakePlaceWrite::place(self)
    }
}

impl<T: AtomicWord> FakePlaceRead for VolatileWriter<T> {
    type Value = T;

    fn get(&self) -> T {
        VolatileWriter::get(self)
    }
}

impl<T: AtomicWord> FakePlaceWrite for VolatileWriter<T> {
    fn set(&self, v: T) {
        VolatileWriter::set(self, v)
    }
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
