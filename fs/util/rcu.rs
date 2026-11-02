// SPDX-License-Identifier: GPL-2.0

//! RCU: read-side sections, what's freed after them, and pointers published
//! under them.
//!
//! # Reading
//!
//! read_lock() is rcu_read_lock() until its ReadGuard drops - the kernel's,
//! or in userspace liburcu's memb flavor, which C's is too. srcu_read_lock()
//! is the same for a C srcu_struct. What's read under one is borrowed for
//! the guard's lifetime, so nothing read outlives the section.
//!
//! # Freeing
//!
//! What a reader may still have is freed only a grace period after it's out
//! of their reach - and before that, taken out of their reach. Two auto
//! traits say which types need which, so that nothing has to say it by hand:
//!
//! - FreeAnytime: a type is, unless it has an RcuMember - a member, or
//!   anything it owns has one. Borrows and pointers free nothing, so don't
//!   count; an owner through a pointer says it owns - std's with a
//!   PhantomData<T>, the kernel's Box and Vec with an impl here.
//! - NothingToDetach: likewise, unless it has a DetachMember - something
//!   readers find it by that has to be taken out of their way first, a
//!   Registration's entry in a cuckoo table. Its Detach::detach() does that;
//!   #[derive(Detach)] detaches every field.
//!
//! alloc_pinned() and free_pinned() (util/alloc.rs) decide by them, through
//! RcuFreeing's specialization. free_pinned() detaches - a build error, for
//! something with a DetachMember and no Detach - then frees at once what's
//! FreeAnytime, and what isn't a grace period on: allocated as an RcuFreed,
//! behind a header C doesn't see, it's call_rcu()'d, and the callback queues
//! its drop and free on the free queue, as a drop may sleep. barrier() waits
//! for all of those begun - before what their drops need goes, and at module
//! exit, before their code does. A free that can't wait - a KBox's, from
//! kbox_pin_init() - requires FreeAnytime.
//!
//! synchronize() is synchronize_rcu(), for what has to wait out readers
//! before it goes on.
//!
//! # Publishing
//!
//! A pointer readers follow, laid out as C's `T __rcu *` so C reads it too.
//! Readers read() it under a guard. Its one writer is had from the lock that
//! publishes it - a Locked's or LockedBy's published writer (Publish).
//!
//! - RcuPtr<P> publishes a P, an RcuPointer, and owns what P owns. An
//!   RcuOwner - ARef - owns its target, which mustn't be FreeAnytime, a build
//!   error if it is, so what's taken out may be dropped at once. A NonNull
//!   owns nothing, and publishing one is unsafe: a promise that it stays
//!   valid until a grace period after it's taken out. Its writer, an
//!   RcuPtrWriter, publishes, takes and get()s.
//! - RcuBuf<T> is a buffer its writer replaces whole, kvfree_rcu()ing the
//!   old; RcuArray<T, H> is one of C's `{ rcu_head; nr; H; T s[] }`, read as
//!   a &[T] and grown by copy and replace - its elements RcuElements, plain
//!   data or atomics, copied bytewise. Its writer is an RcuArrayWriter;
//!   without one, a writer's methods are unsafe, the caller holding the lock.

use core::ffi::c_int;
use core::marker::PhantomData;
use core::mem::{align_of, offset_of, size_of};
use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, AtomicU32, Ordering::{self, Acquire, Relaxed, Release}};
use cstruct_macros::c_extern;
use pin_init::{pin_init_from_closure, PinInit};

use crate::c;
use crate::util::alloc::{alloc_in, flags::GFP_KERNEL, free_pinned_now, init_at, AllocError};
use crate::util::futex::{futex_wait, futex_wake};
use crate::util::projections::Publish;
use crate::util::refcount::{ARef, AlwaysRefCounted};
use crate::util::sleep::Sleep;
use crate::util::workqueue::{Queue, Workqueue, WqFlags};

#[cfg(not(kernel))]
unsafe extern "C" {
    fn urcu_memb_read_lock();
    fn urcu_memb_read_unlock();
    fn urcu_memb_synchronize_rcu();
    fn urcu_memb_call_rcu(rcu: *mut c::rcu_head, func: unsafe extern "C" fn(*mut c::rcu_head));
    fn urcu_memb_barrier();
}

// ── Reading ──────────────────────────────────────────────────────────────

/// Not Send: the section is the thread's.
pub struct ReadGuard {
    #[cfg(kernel)]
    _guard:   kernel::sync::rcu::Guard,
    _not_send: PhantomData<*mut ()>,
}

pub fn read_lock() -> ReadGuard {
    #[cfg(not(kernel))]
    unsafe { urcu_memb_read_lock() };

    ReadGuard {
        #[cfg(kernel)]
        _guard:   kernel::sync::rcu::read_lock(),
        _not_send: PhantomData,
    }
}

#[cfg(not(kernel))]
impl Drop for ReadGuard {
    fn drop(&mut self) {
        unsafe { urcu_memb_read_unlock() };
    }
}

c_extern! {
    pub fn rust_srcu_read_lock(ssp: *mut c::srcu_struct) -> c_int;
    pub fn rust_srcu_read_unlock(ssp: *mut c::srcu_struct, idx: c_int);
}

#[must_use = "the SRCU read section ends as soon as the guard is dropped"]
pub struct SrcuReadGuard<'a> {
    srcu: &'a c::srcu_struct,
    idx:  c_int,
}

pub fn srcu_read_lock(srcu: &c::srcu_struct) -> SrcuReadGuard<'_> {
    // XXX: c::srcu_struct isn't an Opaque, as C's locks are - written
    // through a &
    let idx = unsafe { rust_srcu_read_lock(core::ptr::from_ref(srcu).cast_mut()) };
    SrcuReadGuard { srcu, idx }
}

impl Drop for SrcuReadGuard<'_> {
    fn drop(&mut self) {
        unsafe { rust_srcu_read_unlock(core::ptr::from_ref(self.srcu).cast_mut(), self.idx) };
    }
}

pub fn synchronize() {
    #[cfg(kernel)]
    unsafe { kernel::bindings::synchronize_rcu() };
    #[cfg(not(kernel))]
    unsafe { urcu_memb_synchronize_rcu() };
}

// ── Freeing ──────────────────────────────────────────────────────────────

/// # Safety
/// An impl says that what an RCU reader may hold isn't freed with Self. No
/// impl may depend on a lifetime: RcuFreeing specializes on it.
#[rustc_specialization_trait]
pub unsafe auto trait FreeAnytime {}

pub struct RcuMember;

impl !FreeAnytime for RcuMember {}

unsafe impl<T: ?Sized> FreeAnytime for &T {}
unsafe impl<T: ?Sized> FreeAnytime for &mut T {}
unsafe impl<T: ?Sized> FreeAnytime for *const T {}
unsafe impl<T: ?Sized> FreeAnytime for *mut T {}
unsafe impl<T: ?Sized> FreeAnytime for NonNull<T> {}
#[cfg(kernel)]
unsafe impl<T: ?Sized + FreeAnytime, A: kernel::alloc::Allocator> FreeAnytime for kernel::alloc::Box<T, A> {}
#[cfg(kernel)]
unsafe impl<T: FreeAnytime, A: kernel::alloc::Allocator> FreeAnytime for kernel::alloc::kvec::Vec<T, A> {}
// its last put frees T as T says
unsafe impl<T: AlwaysRefCounted> FreeAnytime for ARef<T> {}

/// # Safety
/// As FreeAnytime's: no impl may depend on a lifetime.
#[rustc_specialization_trait]
pub unsafe auto trait NothingToDetach {}

pub struct DetachMember;

impl !NothingToDetach for DetachMember {}

unsafe impl<T: ?Sized> NothingToDetach for &T {}
unsafe impl<T: ?Sized> NothingToDetach for &mut T {}
unsafe impl<T: ?Sized> NothingToDetach for *const T {}
unsafe impl<T: ?Sized> NothingToDetach for *mut T {}
unsafe impl<T: ?Sized> NothingToDetach for NonNull<T> {}
#[cfg(kernel)]
unsafe impl<T: ?Sized + NothingToDetach, A: kernel::alloc::Allocator> NothingToDetach for kernel::alloc::Box<T, A> {}
#[cfg(kernel)]
unsafe impl<T: NothingToDetach, A: kernel::alloc::Allocator> NothingToDetach for kernel::alloc::kvec::Vec<T, A> {}
unsafe impl<T: AlwaysRefCounted> NothingToDetach for ARef<T> {}

pub trait Detach {
    fn detach(&self);
}

pub use cstruct_macros::Detach;

impl<T> Detach for T {
    default fn detach(&self) {
        let () = NoDetach::<T>::ERR;
    }
}

// default, so a derive on what turns out to have nothing to detach is
// another no-op, not a conflict
impl<T: NothingToDetach> Detach for T {
    default fn detach(&self) {}
}

struct NoDetach<T>(PhantomData<T>);

impl<T> NoDetach<T> {
    const ERR: () = panic!("has something to detach before it's freed - a DetachMember - and no Detach impl");
}

pub(crate) trait RcuFreeing: Sized {
    /// A build error for a T that's FreeAnytime.
    fn assert_rcu_freed();

    fn alloc_pinned<E: From<AllocError>>(init: impl PinInit<Self, E>) -> Result<*mut Self, E>;

    /// # Safety
    /// @p is from alloc_pinned(), and nothing but RCU readers uses it after.
    unsafe fn free_pinned(p: *mut Self);
}

impl<T> RcuFreeing for T {
    default fn assert_rcu_freed() {}

    default fn alloc_pinned<E: From<AllocError>>(init: impl PinInit<T, E>) -> Result<*mut T, E> {
        let p = alloc_in::<false, RcuFreed<T>, E>(RcuFreed::init(init), GFP_KERNEL)?;
        Ok(unsafe { &raw mut (*p).value })
    }

    default unsafe fn free_pinned(p: *mut T) {
        <T as Detach>::detach(unsafe { &*p });
        unsafe { RcuFreed::free(p) }
    }
}

impl<T: FreeAnytime> RcuFreeing for T {
    fn assert_rcu_freed() {
        let () = NotRcuFreed::<T>::ERR;
    }

    fn alloc_pinned<E: From<AllocError>>(init: impl PinInit<T, E>) -> Result<*mut T, E> {
        alloc_in::<false, T, E>(init, GFP_KERNEL)
    }

    unsafe fn free_pinned(p: *mut T) {
        <T as Detach>::detach(unsafe { &*p });
        unsafe { free_pinned_now(p) }
    }
}

struct NotRcuFreed<T>(PhantomData<T>);

impl<T> NotRcuFreed<T> {
    const ERR: () = panic!("what RCU readers reach must be freed a grace period on: it needs an RcuMember");
}

#[repr(C)]
pub(crate) struct RcuFreed<T> {
    rcu:   c::rcu_head,
    work:  c::work_struct,
    value: T,
}

/// RcuFreeds whose drop is still to come - the word their last write goes
/// to, so barrier() waits on it.
static PENDING: AtomicU32 = AtomicU32::new(0);

/// From module init to exit.
static FREE_WQ: AtomicPtr<c::workqueue_struct> = AtomicPtr::new(core::ptr::null_mut());

pub fn free_init() -> Result<(), AllocError> {
    let wq = Workqueue::new(c"bcachefs_rcu_free", WqFlags::UNBOUND, 0)?;
    FREE_WQ.store(core::mem::ManuallyDrop::new(wq).as_ptr(), Release);
    Ok(())
}

/// After barrier(), so nothing's on it.
pub fn free_exit() {
    let wq = FREE_WQ.swap(core::ptr::null_mut(), Acquire);
    if !wq.is_null() {
        unsafe { c::destroy_workqueue(wq) };
    }
}

impl<T> RcuFreed<T> {
    fn init<E>(value: impl PinInit<T, E>) -> impl PinInit<Self, E> {
        let work = crate::new_work!(Self::work_fn, "rcu_freed");
        unsafe {
            pin_init_from_closure(move |slot: *mut Self| {
                (&raw mut (*slot).rcu).write(core::mem::zeroed());
                let Ok(()) = init_at(work, &raw mut (*slot).work);
                init_at(value, &raw mut (*slot).value)
            })
        }
    }

    /// # Safety
    /// @p is an RcuFreed's value, and nothing but RCU readers uses it after.
    unsafe fn free(p: *mut T) {
        let this = unsafe { p.byte_sub(offset_of!(Self, value)).cast::<Self>() };
        PENDING.fetch_add(1, Relaxed);
        unsafe { call_rcu(&raw mut (*this).rcu, Self::rcu_fn) };
    }

    unsafe extern "C" fn rcu_fn(rcu: *mut c::rcu_head) {
        let this = unsafe { rcu.byte_sub(offset_of!(Self, rcu)).cast::<Self>() };
        let wq = FREE_WQ.load(Acquire);
        assert!(!wq.is_null(), "an RcuFreed freed outside module init and exit");
        unsafe { Queue::from_raw(wq).queue_work(&raw mut (*this).work) };
    }

    unsafe extern "C" fn work_fn(work: *mut c::work_struct) {
        let this = unsafe { work.byte_sub(offset_of!(Self, work)).cast::<Self>() };
        unsafe { free_pinned_now(this) };

        if PENDING.fetch_sub(1, Release) == 1 {
            futex_wake(core::ptr::from_ref(&PENDING).cast(), usize::MAX);
        }
    }
}

unsafe fn call_rcu(rcu: *mut c::rcu_head, func: unsafe extern "C" fn(*mut c::rcu_head)) {
    #[cfg(kernel)]
    unsafe { kernel::bindings::call_rcu(rcu, Some(func)) };
    #[cfg(not(kernel))]
    unsafe { urcu_memb_call_rcu(rcu, func) };
}

pub fn barrier() {
    #[cfg(kernel)]
    unsafe { kernel::bindings::rcu_barrier() };
    #[cfg(not(kernel))]
    unsafe { urcu_memb_barrier() };

    loop {
        let n = PENDING.load(Acquire);
        if n == 0 {
            return;
        }
        let Ok(()) = futex_wait(&PENDING, n, Sleep::UNINTERRUPTIBLE);
    }
}

// ── RcuPtr ───────────────────────────────────────────────────────────────

/// # Safety
/// into_raw() gives itself up as a pointer to the target, which from_raw()
/// takes back.
pub unsafe trait RcuPointer: Sized {
    type Target;

    fn into_raw(self) -> NonNull<Self::Target>;

    /// # Safety
    /// @p is into_raw()'s, and taken back once.
    unsafe fn from_raw(p: NonNull<Self::Target>) -> Self;
}

/// # Safety
/// The target lives as long as Self does, and is freed only as its type
/// frees it: a grace period on, if it isn't FreeAnytime.
pub unsafe trait RcuOwner: RcuPointer {}

unsafe impl<T> RcuPointer for NonNull<T> {
    type Target = T;

    fn into_raw(self) -> NonNull<T> {
        self
    }

    unsafe fn from_raw(p: NonNull<T>) -> Self {
        p
    }
}

unsafe impl<T: AlwaysRefCounted> RcuPointer for ARef<T> {
    type Target = T;

    fn into_raw(self) -> NonNull<T> {
        ARef::into_raw(self)
    }

    unsafe fn from_raw(p: NonNull<T>) -> Self {
        unsafe { ARef::from_raw(p) }
    }
}

unsafe impl<T: AlwaysRefCounted> RcuOwner for ARef<T> {}

#[repr(transparent)]
pub struct RcuPtr<P: RcuPointer> {
    p:  AtomicPtr<P::Target>,
    _p: PhantomData<P>,
}

unsafe impl<P: RcuPointer> ::pin_init::Zeroable for RcuPtr<P> {}

impl<P: RcuPointer> RcuPtr<P> {
    pub fn read<'a>(&'a self, _g: &'a ReadGuard) -> Option<&'a P::Target> {
        unsafe { self.p.load(Ordering::Acquire).as_ref() }
    }

    /// For C: its holder's lock, or a ref, keeps what it points to alive -
    /// not this.
    pub fn as_ptr(&self) -> *mut P::Target {
        self.p.load(Ordering::Relaxed)
    }

    fn swap(&self, new: Option<P>) -> Option<P> {
        let new = new.map_or(core::ptr::null_mut(), |p| P::into_raw(p).as_ptr());
        NonNull::new(self.p.swap(new, Ordering::Release)).map(|old| unsafe { P::from_raw(old) })
    }

    fn fill(&self, p: P) {
        let old = self.swap(Some(p));
        assert!(old.is_none(), "publishing into a slot already holding one");
    }
}

impl<P: RcuPointer> Drop for RcuPtr<P> {
    fn drop(&mut self) {
        drop(self.swap(None));
    }
}

unsafe impl<P: RcuPointer> Publish for RcuPtr<P> {
    type Writer = RcuPtrWriter<P>;
}

#[repr(transparent)]
pub struct RcuPtrWriter<P: RcuPointer>(RcuPtr<P>);

impl<P: RcuPointer> core::ops::Deref for RcuPtrWriter<P> {
    type Target = RcuPtr<P>;

    fn deref(&self) -> &RcuPtr<P> {
        &self.0
    }
}

impl<P: RcuOwner> RcuPtrWriter<P> {
    /// Into an empty slot.
    pub fn publish(&self, p: P) {
        <P::Target as RcuFreeing>::assert_rcu_freed();
        self.0.fill(p);
    }
}

impl<T> RcuPtrWriter<NonNull<T>> {
    /// Into an empty slot.
    ///
    /// # Safety
    /// @p stays valid until a grace period after it's taken out.
    pub unsafe fn publish(&self, p: NonNull<T>) {
        self.0.fill(p);
    }
}

impl<P: RcuPointer> RcuPtrWriter<P> {
    pub fn take(&self) -> Option<P> {
        self.0.swap(None)
    }

    /// An owner of its own, not a borrow - which a take() by its own writer
    /// would outlive.
    pub fn get(&self) -> Option<P>
    where
        P: Clone,
    {
        // no writer but us, to take it while we clone it
        let p = core::mem::ManuallyDrop::new(unsafe { P::from_raw(NonNull::new(self.0.as_ptr())?) });
        Some((*p).clone())
    }
}

// ── RcuBuf ───────────────────────────────────────────────────────────────

/// kvfree_rcu(), its rcu_head over the first bytes.
///
/// # Safety
/// @p is kmalloc()'d or kvmalloc()'d, already dropped, and reached after
/// only by readers under the RCU read lock - none of them reading its first
/// bytes.
pub unsafe fn kvfree_rcu<T>(p: *mut T) {
    unsafe { c::rust_kvfree_rcu(p.cast()) };
}

/// A buffer starting with an rcu_head, which a thin pointer - C's - reaches
/// all of: sized, or a DST whose head records its length.
///
/// # Safety
/// Head starts with a c::rcu_head; from_thin() of a pointer to the Head of a
/// valid Self is a pointer to all of it. Self has no Drop.
pub unsafe trait RcuPointee {
    type Head;

    /// # Safety
    /// @p points to the Head of a valid Self.
    unsafe fn from_thin(p: *mut Self::Head) -> *mut Self;
}

#[repr(transparent)]
pub struct RcuBuf<T: ?Sized + RcuPointee> {
    p:  AtomicPtr<T::Head>,
    _t: PhantomData<*const T>,
}

// readers on any thread share the T, and the writer's thread frees it
unsafe impl<T: ?Sized + RcuPointee + Send + Sync> Send for RcuBuf<T> {}
unsafe impl<T: ?Sized + RcuPointee + Send + Sync> Sync for RcuBuf<T> {}

unsafe impl<T: ?Sized + RcuPointee> ::pin_init::Zeroable for RcuBuf<T> {}

impl<T: ?Sized + RcuPointee> RcuBuf<T> {
    pub fn read<'a>(&'a self, _g: &'a ReadGuard) -> Option<&'a T> {
        let p = self.p.load(Ordering::Acquire);
        (!p.is_null()).then(|| unsafe { &*T::from_thin(p) })
    }

    /// read(), in a section of its own for as long as @f runs.
    pub fn with<R>(&self, f: impl FnOnce(Option<&T>) -> R) -> R {
        let g = read_lock();
        f(self.read(&g))
    }

    /// # Safety
    /// The caller holds the writers' lock, and doesn't replace() - which
    /// frees this one, and the writer isn't in a read-side section - until
    /// it's done with the &T.
    pub unsafe fn get_locked(&self) -> Option<&T> {
        let p = self.p.load(Ordering::Relaxed);
        (!p.is_null()).then(|| unsafe { &*T::from_thin(p) })
    }

    /// # Safety
    /// The caller holds the writers' lock. @new is a valid T from
    /// rust_kvzalloc(), which this takes.
    pub unsafe fn replace(&self, new: NonNull<T::Head>) {
        let old = self.p.swap(new.as_ptr(), Ordering::Release);
        if !old.is_null() {
            unsafe { kvfree_rcu(old) };
        }
    }

    /// # Safety
    /// The caller holds the writers' lock.
    pub unsafe fn clear(&self) {
        let old = self.p.swap(core::ptr::null_mut(), Ordering::Release);
        if !old.is_null() {
            unsafe { kvfree_rcu(old) };
        }
    }

    /// For owners C frees, which never run Drop.
    ///
    /// # Safety
    /// No readers or writers are left.
    pub unsafe fn free(&self) {
        let p = self.p.swap(core::ptr::null_mut(), Ordering::Relaxed);
        if !p.is_null() {
            unsafe { c::rust_kvfree(p.cast()) };
        }
    }
}

impl<T: ?Sized + RcuPointee> Drop for RcuBuf<T> {
    /// No readers are left: whatever held this is going away.
    fn drop(&mut self) {
        let p = *self.p.get_mut();
        if !p.is_null() {
            unsafe { c::rust_kvfree(p.cast()) };
        }
    }
}

// ── RcuArray ─────────────────────────────────────────────────────────────

/// C's `struct { struct rcu_head rcu; size_t nr; T s[]; }` - or with fields
/// of its own between nr and s[], as @H: bucket gens' first_bucket.
#[repr(C)]
pub struct RcuArrayBuf<T, H = ()> {
    rcu: c::rcu_head,
    nr:  usize,
    h:   H,
    s:   [T],
}

/// What C's pointer is to.
#[repr(C)]
pub struct RcuArrayHead<H = ()> {
    rcu: c::rcu_head,
    nr:  usize,
    h:   H,
}

// as its elements and header: rcu is touched only by kvfree_rcu(), freeing it
unsafe impl<T: Send, H: Send> Send for RcuArrayBuf<T, H> {}
unsafe impl<T: Sync, H: Sync> Sync for RcuArrayBuf<T, H> {}

impl<T, H> RcuArrayBuf<T, H> {
    /// offsetof(), for checking a C struct's layout matches.
    pub const NR: usize = core::mem::offset_of!(RcuArrayHead<H>, nr);
    pub const HEAD: usize = core::mem::offset_of!(RcuArrayHead<H>, h);
    pub const TAIL: usize = size_of::<RcuArrayHead<H>>().next_multiple_of(align_of::<T>());
}

unsafe impl<T: RcuElement, H: RcuElement> RcuPointee for RcuArrayBuf<T, H> {
    type Head = RcuArrayHead<H>;

    unsafe fn from_thin(p: *mut RcuArrayHead<H>) -> *mut Self {
        // a slice pointer's length is the DST's tail's
        core::ptr::slice_from_raw_parts_mut(p.cast::<T>(), unsafe { (*p).nr }) as *mut Self
    }
}

/// # Safety
/// A bytewise copy of a T is a valid T, owning nothing: no Drop. All-zero
/// bytes are a valid T. A writer copies them into a grown buffer while
/// readers still read the old one, so changing one in place, with readers
/// looking, needs it to be atomics.
pub unsafe trait RcuElement: Sync {}

macro_rules! rcu_element {
    ($($t:ty),*) => { $(unsafe impl RcuElement for $t {})* };
}
rcu_element!((), u8, u16, u32, u64, usize,
             core::sync::atomic::AtomicU8, core::sync::atomic::AtomicU16,
             core::sync::atomic::AtomicU32, core::sync::atomic::AtomicU64,
             core::sync::atomic::AtomicUsize);

unsafe impl<T: RcuElement, const N: usize> RcuElement for [T; N] {}

#[repr(transparent)]
pub struct RcuArray<T: RcuElement, H: RcuElement = ()>(RcuBuf<RcuArrayBuf<T, H>>);

unsafe impl<T: RcuElement, H: RcuElement> ::pin_init::Zeroable for RcuArray<T, H> {}

impl<T: RcuElement, H: RcuElement> RcuArray<T, H> {
    /// For c_rcu_array!'s record, which C's definition is checked against.
    pub const NR: usize = RcuArrayBuf::<T, H>::NR;
    pub const HEAD: usize = RcuArrayBuf::<T, H>::HEAD;
    pub const TAIL: usize = RcuArrayBuf::<T, H>::TAIL;

    /// None published reads as none.
    pub fn read<'a>(&'a self, g: &'a ReadGuard) -> &'a [T] {
        self.0.read(g).map_or(&[], |b| &b.s)
    }

    pub fn read_head<'a>(&'a self, g: &'a ReadGuard) -> Option<(&'a H, &'a [T])> {
        self.0.read(g).map(|b| (&b.h, &b.s))
    }

    pub fn with<R>(&self, f: impl FnOnce(&[T]) -> R) -> R {
        self.0.with(|b| f(b.map_or(&[], |b| &b.s)))
    }

    /// # Safety
    /// As RcuBuf::get_locked().
    pub unsafe fn get_locked(&self) -> &[T] {
        unsafe { self.0.get_locked() }.map_or(&[], |b| &b.s)
    }

    /// # Safety
    /// As RcuBuf::get_locked().
    pub unsafe fn get_head_locked(&self) -> Option<(&H, &[T])> {
        unsafe { self.0.get_locked() }.map(|b| (&b.h, &b.s))
    }

    /// A zeroed buffer of exactly @nr elements, and header @h, filled by
    /// @fill - the old one isn't copied - and published; if @fill fails,
    /// freed unpublished. AllocError if there's no buffer, else @fill's
    /// result.
    ///
    /// # Safety
    /// As RcuBuf::replace(): the caller holds the writers' lock.
    pub unsafe fn rebuild_head<E>(&self, nr: usize, h: H, fill: impl FnOnce(&mut [T]) -> Result<(), E>)
        -> Result<Result<(), E>, AllocError>
    {
        const { assert!(size_of::<T>() != 0) };

        let tail = RcuArrayBuf::<T, H>::TAIL;
        let bytes = nr.checked_mul(size_of::<T>())
            .and_then(|b| b.checked_add(tail))
            .filter(|&b| b <= i32::MAX as usize)
            .ok_or(AllocError)?;

        let new = NonNull::new(unsafe { c::rust_kvzalloc(bytes) }.cast::<RcuArrayHead<H>>())
            .ok_or(AllocError)?;

        // no reader sees it until replace() publishes it
        unsafe {
            (*new.as_ptr()).nr = nr;
            (&raw mut (*new.as_ptr()).h).write(h);
            if let Err(e) = fill(core::slice::from_raw_parts_mut(new.as_ptr().byte_add(tail).cast::<T>(), nr)) {
                c::rust_kvfree(new.as_ptr().cast());
                return Ok(Err(e));
            }
            self.0.replace(new);
        }
        Ok(Ok(()))
    }

    /// # Safety
    /// As RcuBuf::clear().
    pub unsafe fn clear(&self) {
        unsafe { self.0.clear() }
    }

    /// # Safety
    /// As RcuBuf::free().
    pub unsafe fn free(&self) {
        unsafe { self.0.free() }
    }
}

/// Without a header: a header's fields are its writer's to carry over, by
/// rebuild_head().
impl<T: RcuElement> RcuArray<T> {
    /// Room for at least @nr elements: if there isn't, a new buffer, sized to
    /// a power of two bytes as C's tables are, the old elements copied and the
    /// rest zero, published. An error if it would be more than INT_MAX bytes,
    /// which kvmalloc() won't do.
    ///
    /// # Safety
    /// As RcuBuf::replace(): the caller holds the writers' lock.
    pub unsafe fn grow_to(&self, nr: usize) -> Result<(), AllocError> {
        let old = unsafe { self.get_locked() };
        if nr <= old.len() {
            return Ok(());
        }

        let tail = RcuArrayBuf::<T>::TAIL;
        let bytes = nr.checked_mul(size_of::<T>())
            .and_then(|b| b.checked_add(tail))
            .and_then(usize::checked_next_power_of_two)
            .ok_or(AllocError)?;

        // the old elements don't change under the writers' lock
        unsafe {
            self.rebuild((bytes - tail) / size_of::<T>(), |new| {
                core::ptr::copy_nonoverlapping(old.as_ptr(), new.as_mut_ptr(), old.len())
            })
        }
    }

    /// # Safety
    /// As RcuBuf::replace(): the caller holds the writers' lock.
    pub unsafe fn rebuild(&self, nr: usize, fill: impl FnOnce(&mut [T])) -> Result<(), AllocError> {
        unsafe { self.rebuild_head(nr, (), |s| Ok::<_, core::convert::Infallible>(fill(s))) }.map(|_| ())
    }
}

unsafe impl<T: RcuElement, H: RcuElement> Publish for RcuArray<T, H> {
    type Writer = RcuArrayWriter<T, H>;
}

/// Readers' methods, read() and with(), in a read-side section: nothing it's
/// handed outlives one, so its own replace can't free what it's reading.
#[repr(transparent)]
pub struct RcuArrayWriter<T: RcuElement, H: RcuElement = ()>(RcuArray<T, H>);

impl<T: RcuElement, H: RcuElement> core::ops::Deref for RcuArrayWriter<T, H> {
    type Target = RcuArray<T, H>;

    fn deref(&self) -> &RcuArray<T, H> {
        &self.0
    }
}

impl<T: RcuElement> RcuArrayWriter<T> {
    /// Allocates, and may sleep: not under a spinlock.
    pub fn rebuild(&self, nr: usize, fill: impl FnOnce(&mut [T])) -> Result<(), AllocError> {
        unsafe { self.0.rebuild(nr, fill) }
    }

    /// Allocates, and may sleep.
    pub fn grow_to(&self, nr: usize) -> Result<(), AllocError> {
        unsafe { self.0.grow_to(nr) }
    }
}

impl<T: RcuElement, H: RcuElement> RcuArrayWriter<T, H> {
    pub fn clear(&self) {
        unsafe { self.0.clear() }
    }
}
