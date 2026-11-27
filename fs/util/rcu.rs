// SPDX-License-Identifier: GPL-2.0

//! RCU: read-side sections, what's freed after them, and pointers published
//! under them.
//!
//! # Reading
//!
//! read_lock() is rcu_read_lock() until its ReadGuard drops - the kernel's,
//! or in userspace liburcu's memb flavor, which C's is too. Srcu::read_lock()
//! is the same for an SRCU domain. What's read under one is borrowed for
//! the guard's lifetime, so nothing read outlives the section.
//!
//! # Freeing
//!
//! What a reader may still have is freed only a grace period after it's out
//! of their reach. Its members say how - RcuMember, RcuDropMember,
//! RcuDropWorkMember - and auto traits carry that to what owns them; a
//! MaybeRcuAlloc (util/maybe_rcu_alloc.rs) frees by them. What's dropped after
//! the grace period is detached first, out of readers' way: Detach.
//!
//! synchronize() is synchronize_rcu(), for what has to wait out readers
//! before it goes on.
//!
//! # Publishing
//!
//! A pointer readers follow. Readers read() it under a guard. Its one writer
//! is had from the lock that publishes it - a Locked's or LockedBy's
//! published writer (Publish).
//!
//! - RcuPtr<P> publishes a P, a single-word pointer - NonZeroWord - whose
//!   drop frees its target as the target says: a MaybeRcuAlloc, a ThinBox, a
//!   ref on a count. Its target must be freed a grace period on -
//!   !FreeAnytime, a build error if not - and what's taken out may be
//!   dropped at once. Its writer, an RcuPtrWriter, publishes, replaces, takes
//!   and get()s.
//! - RcuArray<T, H> is one of C's `{ rcu_head; nr; H; T s[] }` - the count
//!   after H, DstArray's - an RcuPtr<ThinBox<..>>, read as a &[T] and grown
//!   by copy and replace - its elements zerocopy's FromZeros, made zeroed,
//!   and FromBytes to grow, copied bytewise. Its writer is an RcuArrayWriter.

use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::ops::Deref;
use core::ptr::NonNull;
use core::sync::atomic::{fence, AtomicPtr, Ordering};
use crate::c::rcu_head;
use crate::data_structures::dst_array::DstArray;
use crate::os::workqueue::RawWork;
use crate::util::alloc::{init_at, AllocError};
use crate::util::maybe_rcu_alloc::RcuFreeing;
use crate::util::projections::{Publish, Volatile};
use crate::util::refcount::{ARef, AlwaysRefCounted};
use crate::util::thin::ThinBox;
use crate::util::vstructs::NonZeroWord;
use pin_init::{pin_init_from_closure, PinInit, Zeroable};
use zerocopy::{FromBytes, FromZeros};

// ── Reading, and grace periods ───────────────────────────────────────────

pub use crate::os::rcu::{read_lock, synchronize, Flavor, GpCookie, ReadGuard, Srcu, SrcuReadGuard};

/// An Srcu initializer - new_srcu!("name") - with a lockdep class of its own.
#[macro_export]
macro_rules! new_srcu {
    ($name:literal) => {{
        let key = $crate::new_lock!(@key);
        let name = const { $crate::util::ffi::c_str(::core::concat!($name, "\0")) };
        $crate::util::rcu::Srcu::new(name, key)
    }};
}

// ── Freeing ──────────────────────────────────────────────────────────────

/// # Safety
/// An impl says that what an RCU reader may hold isn't freed with Self. No
/// impl may depend on a lifetime: RcuFreeing specializes on it.
#[rustc_specialization_trait]
pub unsafe auto trait FreeAnytime {}

/// # Safety
/// As FreeAnytime's: an impl says that nothing an RCU reader may hold is
/// dropped with Self - its memory may still have to wait for them.
#[rustc_specialization_trait]
pub unsafe auto trait DropAnytime {}

/// What hasn't an RcuDropWorkMember: what's dropped after a grace period, by
/// an RcuDropMember's promise, drops without sleeping - in an RCU callback.
///
/// # Safety
/// As FreeAnytime's.
#[rustc_specialization_trait]
pub unsafe auto trait DropAtomic {}

/// What it's in is freed a grace period on: dropped at once, its memory once
/// RCU readers are done with it - they reach nothing its drop frees. @H is
/// RcuHead for what the free may use, rather than a header of its own.
#[repr(transparent)]
#[derive(Default)]
pub struct RcuMember<H = ()>(H);

impl<H> !FreeAnytime for RcuMember<H> {}

/// What it's in is dropped and freed a grace period on, in the RCU callback:
/// its readers reach what its drop frees. @H as RcuMember's.
#[repr(transparent)]
pub struct RcuDropMember<H = ()>(H);

impl<H> !FreeAnytime for RcuDropMember<H> {}
impl<H> !DropAnytime for RcuDropMember<H> {}

impl<H: Default> RcuDropMember<H> {
    /// # Safety
    /// What it's in drops without sleeping: in softirq context, an RCU
    /// callback's.
    pub unsafe fn new() -> Self {
        Self(H::default())
    }
}

/// As RcuDropMember, for a drop that needs process context: it runs on the
/// free queue.
#[repr(transparent)]
#[derive(Default)]
pub struct RcuDropWorkMember<H = ()>(H);

impl<H> !FreeAnytime for RcuDropWorkMember<H> {}
impl<H> !DropAnytime for RcuDropWorkMember<H> {}
impl<H> !DropAtomic for RcuDropWorkMember<H> {}

pub use crate::os::rcu::RcuHead;

/// The rcu_head, then the work queued in its place - or a work first, whose
/// memory is the head once it's done: a task's.
#[repr(C)]
pub union RcuWorkHead {
    rcu:  ManuallyDrop<RcuHead>,
    work: ManuallyDrop<RawWork>,
}

impl RcuWorkHead {
    /// The work @init makes: what it is till its free.
    pub fn new_work<E>(init: impl PinInit<RawWork, E>) -> impl PinInit<Self, E> {
        unsafe {
            pin_init_from_closure(move |slot: *mut Self| {
                init_at(init, (&raw mut (*slot).work).cast())
            })
        }
    }

    /// The work new_work() made.
    pub fn work(&self) -> &RawWork {
        unsafe { &self.work }
    }
}

/// The head a free uses: an RcuHead, or an RcuWorkHead - room for the work.
/// Put before what it frees, it's the allocation's start.
pub(crate) trait HeadKind: HasRcuHead {}

impl HeadKind for RcuHead {}
impl HeadKind for RcuWorkHead {}

impl HasRcuHead for RcuHead {
    const HEAD: usize = 0;
    const WORK: bool = false;
}

impl HasRcuHead for RcuWorkHead {
    const HEAD: usize = 0;
    const WORK: bool = true;
}

macro_rules! heads {
    ($($h:ident),*) => {$(
        unsafe impl Send for $h {}
        unsafe impl Sync for $h {}

        impl Default for $h {
            fn default() -> Self {
                ::pin_init::zeroed()
            }
        }

        unsafe impl ::pin_init::Zeroable for $h {}
    )*};
}

heads!(RcuHead, RcuWorkHead);

/// A type's own head: #[derive(RcuHead)]'s, at the field marked #[rcu_head].
/// HEAD is the offset in Self of an RcuHead - an RcuWorkHead, if WORK -
/// nothing but its free uses.
#[rustc_specialization_trait]
pub trait HasRcuHead {
    const HEAD: usize;
    const WORK: bool;
}

impl HasRcuHead for RcuMember<RcuHead> {
    const HEAD: usize = 0;
    const WORK: bool = false;
}
impl HasRcuHead for RcuDropMember<RcuHead> {
    const HEAD: usize = 0;
    const WORK: bool = false;
}
impl HasRcuHead for RcuDropWorkMember<RcuWorkHead> {
    const HEAD: usize = 0;
    const WORK: bool = true;
}
impl<T: HasRcuHead> HasRcuHead for ManuallyDrop<T> {
    const HEAD: usize = T::HEAD;
    const WORK: bool = T::WORK;
}
// made or not, its T's head is memory nothing but the free uses
impl<T: HasRcuHead> HasRcuHead for core::mem::MaybeUninit<T> {
    const HEAD: usize = T::HEAD;
    const WORK: bool = T::WORK;
}

pub use cstruct_macros::RcuHead;

// ── Callbacks ────────────────────────────────────────────────────────────

/// An owner of something with an RcuHead, given up as its head while it waits
/// out a grace period and taken back after: the kernel's ForeignOwnable, for
/// RCU. Owning it, call() can't queue a head twice, or let what's queued go
/// before its callback.
///
/// # Safety
/// into_head()'s head is from_head()'s to take back, once, and owned until
/// then: nothing else frees what it's in, or queues it.
pub unsafe trait RcuOwned {
    fn into_head(self) -> NonNull<RcuHead>;

    /// # Safety
    /// @head is into_head()'s, not yet taken back.
    unsafe fn from_head(head: NonNull<RcuHead>) -> Self;
}

impl Flavor<'_> {
    /// @f(@p) a grace period on: call_rcu(). @f captures nothing - an
    /// rcu_head has room for a function, not its state; what it needs is @p.
    pub fn call<P: RcuOwned + Send, F: Fn(P) + Copy>(self, p: P, _f: F) {
        const { assert!(size_of::<F>() == 0, "call()'s callback can't capture: an rcu_head has no room") };
        unsafe { self.call_raw(p.into_head().as_ptr().cast(), callback::<P, F>) };
    }
}

unsafe extern "C" fn callback<P: RcuOwned, F: Fn(P) + Copy>(head: *mut rcu_head) {
    let p = unsafe { P::from_head(NonNull::new_unchecked(head.cast())) };
    // F is zero-sized, so this is its one value
    let f = unsafe { NonNull::<F>::dangling().read() };
    f(p)
}

/// What holds without owning, or owns as its T says: borrows and pointers,
/// which free nothing; an ARef, whose last put frees T as T says. The
/// kernel's Box and Vec, as their T would, are os::alloc's.
macro_rules! held_pointers {
    ($($trait:ident),*) => {$(
        unsafe impl<T: ?Sized> $trait for &T {}
        unsafe impl<T: ?Sized> $trait for &mut T {}
        unsafe impl<T: ?Sized> $trait for *const T {}
        unsafe impl<T: ?Sized> $trait for *mut T {}
        unsafe impl<T: ?Sized> $trait for NonNull<T> {}
        unsafe impl<T: AlwaysRefCounted> $trait for ARef<T> {}
    )*};
}

/// # Safety
/// As FreeAnytime's: no impl may depend on a lifetime.
#[rustc_specialization_trait]
pub unsafe auto trait NothingToDetach {}

pub struct DetachMember;

impl !NothingToDetach for DetachMember {}

held_pointers!(FreeAnytime, DropAnytime, DropAtomic, NothingToDetach);

pub trait Detach {
    fn detach(&self);
}

pub use cstruct_macros::Detach;

impl<T: ?Sized> Detach for T {
    default fn detach(&self) {
        let () = NoDetach::<T>::ERR;
    }
}

// default, so a derive on what turns out to have nothing to detach is
// another no-op, not a conflict
impl<T: ?Sized + NothingToDetach> Detach for T {
    default fn detach(&self) {}
}

struct NoDetach<T: ?Sized>(PhantomData<T>);

impl<T: ?Sized> NoDetach<T> {
    const ERR: () = panic!("has something to detach before it's freed - a DetachMember - and no Detach impl");
}

// ── RcuPtr ───────────────────────────────────────────────────────────────

/// A pointer walked under the RCU read lock alone - one word, so a reader
/// loads it whole. Its target is RCU-freed - !FreeAnytime - which publish()
/// checks, a build error if not: Rust has no negative bound to say it here.
#[repr(transparent)]
pub struct RcuPtr<P: Deref + NonZeroWord>(Volatile<Option<P>>);

unsafe impl<P: Deref + NonZeroWord> Zeroable for RcuPtr<P> {}

impl<P: Deref + NonZeroWord> RcuPtr<P> {
    /// Loaded relaxed, as rcu_dereference() is: its target is ordered after
    /// by the address dependency.
    pub fn read<'a>(&'a self, _g: &'a ReadGuard) -> Option<&'a P::Target> {
        unsafe { self.unguarded() }
    }

    /// Its target, read without the RCU read lock.
    ///
    /// # Safety
    /// Its holder's lock, or a ref, keeps the target alive while it's used.
    pub unsafe fn unguarded(&self) -> Option<&P::Target> {
        unsafe { self.0.get_unowned().as_deref().map(|t| &*core::ptr::from_ref(t)) }
    }
}

impl<P: Deref + NonZeroWord> Publish for RcuPtr<P> {
    type Writer = RcuPtrWriter<P>;
}

#[repr(transparent)]
pub struct RcuPtrWriter<P: Deref + NonZeroWord>(RcuPtr<P>);

impl<P: Deref + NonZeroWord> core::ops::Deref for RcuPtrWriter<P> {
    type Target = RcuPtr<P>;

    fn deref(&self) -> &RcuPtr<P> {
        &self.0
    }
}

impl<P: Deref + NonZeroWord> RcuPtrWriter<P> {
    /// Into an empty slot: released, as rcu_assign_pointer() is, so a reader
    /// that sees it sees what was written to its target first.
    pub(crate) fn publish(&self, p: P)
    where
        P::Target: RcuFreeing,
    {
        <P::Target as RcuFreeing>::assert_rcu_freed();
        fence(Ordering::Release);
        let old = self.0 .0.replace(Some(p));
        assert!(old.is_none(), "publishing into a slot already holding one");
    }

    /// @p in place of what's there, released as publish() is: readers see one
    /// or the other, never none. What it held is the caller's - dropped, its
    /// target freed as it says, past the readers that may still have it.
    pub(crate) fn replace(&self, p: P) -> Option<P>
    where
        P::Target: RcuFreeing,
    {
        <P::Target as RcuFreeing>::assert_rcu_freed();
        fence(Ordering::Release);
        self.0 .0.replace(Some(p))
    }

    pub fn take(&self) -> Option<P> {
        self.0 .0.replace(None)
    }

    /// An owner of its own, not a borrow - which a take() by its own writer
    /// would outlive.
    pub fn get(&self) -> Option<P>
    where
        P: Clone,
    {
        // no writer but us, to take it while we clone it
        Option::<P>::clone(&*unsafe { self.0 .0.get_unowned() })
    }
}

// ── C's RCU pointers ─────────────────────────────────────────────────────

/// rcu_assign_pointer(), into a pointer of C's that other threads
/// rcu_dereference(): a release store, so a reader that sees @p sees what was
/// written to it first. The Cell isn't Sync, so other threads reach it only
/// by pointer, reading it atomically: C's READ_ONCE(), the cycle detector's
/// load.
pub fn rcu_assign_pointer<T>(slot: &core::cell::Cell<*mut T>, p: *mut T) {
    unsafe { AtomicPtr::from_ptr(slot.as_ptr()) }.store(p, Ordering::Release);
}

/// rcu_dereference(): what's read through the pointer is ordered after the
/// load by its address dependency, on everything but Alpha - so a plain load,
/// as C's READ_ONCE() is. Rust's model has no consume ordering; Acquire would
/// say it formally, at an LDAR per load on arm64.
#[inline]
pub fn rcu_dereference<T>(p: &AtomicPtr<T>) -> *mut T {
    p.load(Ordering::Relaxed)
}

// ── RcuArray ─────────────────────────────────────────────────────────────

/// The head of C's `struct { struct rcu_head rcu; size_t nr; T s[]; }`, what
/// its pointer is to - with fields of its own between nr and s[] as @H:
/// bucket gens' first_bucket. Published, it's read by readers till a grace
/// period after it's replaced: an RcuMember's, freed by its own head.
#[repr(C)]
#[derive(RcuHead)]
pub struct RcuArrayHead<H = ()> {
    #[rcu_head]
    rcu: RcuMember<RcuHead>,
    h:   H,
}

impl<H> RcuArrayHead<H> {
    fn new(h: H) -> Self {
        Self { rcu: RcuMember::default(), h }
    }
}

pub type RcuArrayBuf<T, H = ()> = DstArray<RcuArrayHead<H>, T>;

/// An RcuArray's buffer, owned: freed as its head says, a grace period on.
pub type RcuArrayBox<T, H = ()> = ThinBox<RcuArrayBuf<T, H>>;

/// Its elements and header Sync, as readers share them. A new buffer's
/// elements are zeroes, so FromZeros; grow_to() copies them bytewise, so
/// FromBytes there.
#[repr(transparent)]
pub struct RcuArray<T: Sync, H: Sync = ()>(RcuPtr<RcuArrayBox<T, H>>);

unsafe impl<T: Sync, H: Sync> Zeroable for RcuArray<T, H> {}

impl<T: Sync, H: Sync> RcuArray<T, H> {
    /// None published reads as none.
    pub fn read<'a>(&'a self, g: &'a ReadGuard) -> &'a [T] {
        self.0.read(g).map_or(&[], |b| &b.elems)
    }

    pub fn read_head<'a>(&'a self, g: &'a ReadGuard) -> Option<(&'a H, &'a [T])> {
        self.0.read(g).map(|b| (&b.head().h, &b.elems))
    }

    pub fn with<R>(&self, f: impl FnOnce(&[T]) -> R) -> R {
        let g = read_lock();
        f(self.read(&g))
    }

    /// A zeroed buffer of exactly @nr elements and header @h, not published:
    /// the caller's to publish, to link from another buffer's header, or to
    /// drop.
    pub fn alloc(nr: usize, h: H) -> Result<RcuArrayBox<T, H>, AllocError>
    where
        T: FromZeros,
    {
        RcuArrayBuf::<T, H>::alloc(nr, RcuArrayHead::new(h))
    }

    /// alloc(), for what mustn't fail to allocate.
    pub fn alloc_nofail(nr: usize, h: H) -> RcuArrayBox<T, H>
    where
        T: FromZeros,
    {
        RcuArrayBuf::<T, H>::alloc_nofail(nr, RcuArrayHead::new(h))
    }

    /// A buffer's header and elements: one not published here, but linked
    /// from another's header.
    pub fn parts(b: &RcuArrayBuf<T, H>) -> (&H, &[T]) {
        (&b.head().h, &b.elems)
    }
}

impl<T: Sync, H: Sync> Publish for RcuArray<T, H> {
    type Writer = RcuArrayWriter<T, H>;
}

/// Readers' methods, read() and with(), in a read-side section: nothing it's
/// handed outlives one, so its own replace can't free what it's reading.
#[repr(transparent)]
pub struct RcuArrayWriter<T: Sync, H: Sync = ()>(RcuArray<T, H>);

impl<T: Sync, H: Sync> core::ops::Deref for RcuArrayWriter<T, H> {
    type Target = RcuArray<T, H>;

    fn deref(&self) -> &RcuArray<T, H> {
        &self.0
    }
}

impl<T: Sync, H: Sync> RcuArrayWriter<T, H> {
    fn ptr(&self) -> &RcuPtrWriter<RcuArrayBox<T, H>> {
        // SAFETY: we're the array's writer: its lock is held
        unsafe { self.0 .0.writer() }
    }

    /// The buffer as it stands, outside a read-side section: the writer's to
    /// read, as nothing else replaces it.
    ///
    /// # Safety
    /// The &[T] isn't held across this writer's own replace or clear, which
    /// free it a grace period on - one this read isn't in.
    pub unsafe fn get_head_locked(&self) -> Option<(&H, &[T])> {
        unsafe { self.0 .0.unguarded() }.map(|b| (&b.head().h, &b.elems))
    }

    /// As get_head_locked(), the elements alone.
    ///
    /// # Safety
    /// As get_head_locked()'s.
    pub unsafe fn get_locked(&self) -> &[T] {
        unsafe { self.get_head_locked() }.map_or(&[], |(_, elems)| elems)
    }

    /// @new published in the old one's place, which is freed a grace period
    /// on.
    pub fn replace(&self, new: RcuArrayBox<T, H>) {
        drop(self.ptr().replace(new));
    }

    pub fn clear(&self) {
        drop(self.ptr().take());
    }

    /// A zeroed buffer of exactly @nr elements and header @h, filled by
    /// @fill - given the old header and elements too, still published, and
    /// ours to read till the new replaces them - and published; if @fill
    /// fails, freed unpublished. AllocError if there's no buffer, else
    /// @fill's result.
    pub fn rebuild_head<E>(&self, nr: usize, h: H,
                           fill: impl FnOnce(Option<(&H, &[T])>, &mut [T]) -> Result<(), E>)
        -> Result<Result<(), E>, AllocError>
    where
        T: FromZeros,
    {
        let mut new = RcuArray::alloc(nr, h)?;
        // no reader sees @new until it's published; the old isn't freed till
        // after
        if let Err(e) = fill(unsafe { self.get_head_locked() }, &mut new.elems) {
            return Ok(Err(e));
        }
        self.replace(new);
        Ok(Ok(()))
    }
}

/// Without a header: a header's fields are its writer's to carry over, by
/// rebuild_head().
impl<T: Sync> RcuArrayWriter<T> {
    /// Allocates, and may sleep: not under a spinlock.
    pub fn rebuild(&self, nr: usize, fill: impl FnOnce(&mut [T])) -> Result<(), AllocError>
    where
        T: FromZeros,
    {
        self.rebuild_head(nr, (), |_, s| Ok::<_, core::convert::Infallible>(fill(s))).map(|_| ())
    }

    /// Room for at least @nr elements: if there isn't, a new buffer, sized to
    /// a power of two bytes as C's tables are, the old elements copied and the
    /// rest zero, published. Allocates, and may sleep.
    pub fn grow_to(&self, nr: usize) -> Result<(), AllocError>
    where
        T: FromBytes,
    {
        // the old one isn't freed till after replace()
        let old = unsafe { self.get_locked() };
        if nr <= old.len() {
            return Ok(());
        }

        let tail = RcuArrayBuf::<T>::TAIL;
        let bytes = nr.checked_mul(size_of::<T>())
            .and_then(|b| b.checked_add(tail))
            .and_then(usize::checked_next_power_of_two)
            .ok_or(AllocError)?;

        let mut new = RcuArray::alloc((bytes - tail) / size_of::<T>(), ())?;
        // FromBytes, so a bytewise copy is one - the old elements don't
        // change under the writers' lock, and readers keep reading them until
        // the grace period after replace()
        unsafe { core::ptr::copy_nonoverlapping(old.as_ptr(), new.elems.as_mut_ptr(), old.len()) };
        self.replace(new);
        Ok(())
    }
}
