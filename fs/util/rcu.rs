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
//! A pointer readers follow, laid out as C's `T __rcu *` so C reads it too.
//! Readers read() it under a guard. Its one writer is had from the lock that
//! publishes it - a Locked's or LockedBy's published writer (Publish).
//!
//! - RcuPtr<P> publishes a P, an RcuPointer, and owns what P owns. An
//!   RcuOwner - ARef - owns its target, which readers read whole: so it must
//!   be dropped a grace period on - an RcuDropMember's or an
//!   RcuDropWorkMember's, a build error if not - and what's taken out may be
//!   dropped at once. A NonNull owns nothing, and publishing one is unsafe: a
//!   promise that it stays valid until a grace period after it's taken out.
//!   Its writer, an RcuPtrWriter, publishes, takes and get()s.
//! - RcuBuf<T> is a buffer its writer replaces whole, kvfree_rcu()ing the
//!   old; RcuArray<T, H> is one of C's `{ rcu_head; nr; H; T s[] }`, read as
//!   a &[T] and grown by copy and replace - its elements RcuElements, plain
//!   data or atomics, copied bytewise. Its writer is an RcuArrayWriter;
//!   without one, a writer's methods are unsafe, the caller holding the lock.

use core::ffi::{c_int, c_void, CStr};
use core::marker::PhantomData;
use core::mem::size_of;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicPtr, Ordering};
use cstruct_macros::c_extern;
use pin_init::PinInit;

use crate::c;
use crate::data_structures::dst_array::{DstArray, DstHead};
use crate::errcode::{ret_to_result_void, BchError};
use crate::locking::MemallocFlags;
use crate::util::alloc::AllocError;
use crate::util::ffi::c_char;
use crate::util::maybe_rcu_alloc::RcuFreeing;
use crate::util::projections::Publish;
use crate::util::refcount::{ARef, AlwaysRefCounted};

#[cfg(not(kernel))]
unsafe extern "C" {
    fn urcu_memb_read_lock();
    fn urcu_memb_read_unlock();
    fn urcu_memb_synchronize_rcu();
    fn urcu_memb_call_rcu(rcu: *mut c::rcu_head, func: unsafe extern "C" fn(*mut c::rcu_head));
    fn urcu_memb_barrier();
    fn urcu_memb_start_poll_synchronize_rcu() -> c::urcu_gp_poll_state;
    fn urcu_memb_poll_state_synchronize_rcu(state: c::urcu_gp_poll_state) -> bool;
}

// ── Reading ──────────────────────────────────────────────────────────────

/// Not Send: the section is the thread's. A NORECLAIM scope while it lives.
pub struct ReadGuard {
    #[cfg(kernel)]
    _guard:     kernel::sync::rcu::Guard,
    _noreclaim: MemallocFlags,
    _not_send:  PhantomData<*mut ()>,
}

pub fn read_lock() -> ReadGuard {
    #[cfg(not(kernel))]
    unsafe { urcu_memb_read_lock() };

    ReadGuard {
        #[cfg(kernel)]
        _guard:     kernel::sync::rcu::read_lock(),
        _noreclaim: MemallocFlags::noreclaim(),
        _not_send:  PhantomData,
    }
}

#[cfg(not(kernel))]
impl Drop for ReadGuard {
    fn drop(&mut self) {
        unsafe { urcu_memb_read_unlock() };
    }
}

c_extern! {
    fn rust_init_srcu_struct(ssp: *mut c::srcu_struct, name: *const c_char, key: *mut c_void) -> c_int;
    fn rust_synchronize_srcu_expedited(ssp: *mut c::srcu_struct);
    fn rust_cleanup_srcu_struct(ssp: *mut c::srcu_struct);
    fn rust_srcu_read_lock(ssp: *mut c::srcu_struct) -> c_int;
    fn rust_srcu_read_unlock(ssp: *mut c::srcu_struct, idx: c_int);
}

/// An SRCU domain, C's srcu_struct: cleaned up when it drops.
#[repr(C)]
pub struct Srcu(
    #[cfg(kernel)] crate::util::ffi::Opaque<c::srcu_struct>,
    // C's empty struct: improper_ctypes won't have a struct with no fields
    #[cfg(not(kernel))] [u8; 0],
);

// an srcu_struct is any thread's to read under and wait on
unsafe impl Send for Srcu {}
unsafe impl Sync for Srcu {}

/// An Srcu initializer - new_srcu!("name") - with a lockdep class of its own.
#[macro_export]
macro_rules! new_srcu {
    ($name:literal) => {{
        let key = $crate::new_lock!(@key);
        let name = const { $crate::util::ffi::c_str(::core::concat!($name, "\0")) };
        unsafe { $crate::util::rcu::Srcu::new(name, key) }
    }};
}

impl Srcu {
    /// # Safety
    /// @key is a static lockdep class.
    pub unsafe fn new(name: &'static CStr, key: *mut c_void) -> impl PinInit<Self, BchError> {
        unsafe {
            ::pin_init::pin_init_from_closure(move |slot: *mut Self| {
                ret_to_result_void(rust_init_srcu_struct(slot.cast(), name.as_ptr().cast(), key))
            })
        }
    }

    fn as_ptr(&self) -> *mut c::srcu_struct {
        #[cfg(kernel)]
        return self.0.as_ptr();
        #[cfg(not(kernel))]
        return ptr::from_ref(self).cast_mut().cast();
    }

    pub fn read_lock(&self) -> SrcuReadGuard<'_> {
        SrcuReadGuard { srcu: self, idx: unsafe { rust_srcu_read_lock(self.as_ptr()) } }
    }

    /// Its grace periods, for what waits on or queues to them.
    pub fn flavor(&self) -> Flavor<'_> {
        Flavor(self.as_ptr(), PhantomData)
    }
}

impl Drop for Srcu {
    fn drop(&mut self) {
        // what's queued through a flavor() outlives its borrow
        self.flavor().barrier();
        unsafe {
            rust_synchronize_srcu_expedited(self.as_ptr());
            rust_cleanup_srcu_struct(self.as_ptr());
        }
    }
}

#[must_use = "the SRCU read section ends as soon as the guard is dropped"]
pub struct SrcuReadGuard<'a> {
    srcu: &'a Srcu,
    idx:  c_int,
}

impl<'a> SrcuReadGuard<'a> {
    /// The section left held, for what holds it where a guard can't be - a
    /// transaction's, across its unlocks: its index, for from_raw().
    pub fn into_raw(self) -> c_int {
        core::mem::ManuallyDrop::new(self).idx
    }

    /// # Safety
    /// @idx is into_raw()'s, of a read_lock() of @srcu, taken back once.
    pub unsafe fn from_raw(srcu: &'a Srcu, idx: c_int) -> Self {
        SrcuReadGuard { srcu, idx }
    }
}

impl Drop for SrcuReadGuard<'_> {
    fn drop(&mut self) {
        unsafe { rust_srcu_read_unlock(self.srcu.as_ptr(), self.idx) };
    }
}

pub fn synchronize() {
    #[cfg(kernel)]
    unsafe { kernel::bindings::synchronize_rcu() };
    #[cfg(not(kernel))]
    unsafe { urcu_memb_synchronize_rcu() };
}

// ── Grace periods, polled ────────────────────────────────────────────────

/// A grace period, as the poll API's cookie names one.
#[derive(Clone, Copy, Default)]
pub struct GpCookie(#[cfg(kernel)] usize, #[cfg(not(kernel))] c::urcu_gp_poll_state);

impl PartialEq for GpCookie {
    fn eq(&self, o: &Self) -> bool {
        #[cfg(kernel)]
        return self.0 == o.0;
        #[cfg(not(kernel))]
        return self.0.grace_period_id == o.0.grace_period_id;
    }
}

/// Which RCU: the kernel's, or an SRCU domain's, borrowed - C's srcu_struct
/// pointer, null for RCU. In userspace, SRCU is RCU.
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct Flavor<'a>(*mut c::srcu_struct, PhantomData<&'a Srcu>);

// an srcu_struct is any thread's to wait on and queue on
unsafe impl Send for Flavor<'_> {}
unsafe impl Sync for Flavor<'_> {}

impl Flavor<'static> {
    pub const RCU: Self = Flavor(core::ptr::null_mut(), PhantomData);
}

impl<'a> Flavor<'a> {
    /// # Safety
    /// @srcu outlives 'a, and everything waited for or queued on it through
    /// this.
    pub unsafe fn srcu(srcu: *mut c::srcu_struct) -> Self {
        Flavor(srcu, PhantomData)
    }

    #[cfg(kernel)]
    fn ssp(self) -> Option<*mut kernel::bindings::srcu_struct> {
        (!self.0.is_null()).then(|| self.0.cast())
    }

    /// The grace period a reader now would have to be past, without starting
    /// one: get_state_synchronize_rcu().
    pub fn get_state(self) -> GpCookie {
        #[cfg(kernel)]
        return GpCookie(match self.ssp() {
            Some(ssp) => unsafe { kernel::bindings::get_state_synchronize_srcu(ssp) },
            None      => unsafe { kernel::bindings::get_state_synchronize_rcu() },
        });
        #[cfg(not(kernel))]
        return GpCookie(unsafe { urcu_memb_start_poll_synchronize_rcu() });
    }

    /// get_state(), and that grace period started: start_poll_synchronize_rcu().
    pub fn start_poll(self) -> GpCookie {
        #[cfg(kernel)]
        return GpCookie(match self.ssp() {
            Some(ssp) => unsafe { kernel::bindings::start_poll_synchronize_srcu(ssp) },
            None      => unsafe { kernel::bindings::start_poll_synchronize_rcu() },
        });
        #[cfg(not(kernel))]
        return GpCookie(unsafe { urcu_memb_start_poll_synchronize_rcu() });
    }

    /// Whether @c's grace period is over: poll_state_synchronize_rcu().
    pub fn poll(self, c: GpCookie) -> bool {
        #[cfg(kernel)]
        return match self.ssp() {
            Some(ssp) => unsafe { kernel::bindings::poll_state_synchronize_srcu(ssp, c.0) },
            None      => unsafe { kernel::bindings::poll_state_synchronize_rcu(c.0) },
        };
        #[cfg(not(kernel))]
        return unsafe { urcu_memb_poll_state_synchronize_rcu(c.0) };
    }

    /// Every callback queued with call() run: rcu_barrier().
    pub fn barrier(self) {
        #[cfg(kernel)]
        match self.ssp() {
            Some(ssp) => unsafe { kernel::bindings::srcu_barrier(ssp) },
            None      => unsafe { kernel::bindings::rcu_barrier() },
        }
        #[cfg(not(kernel))]
        unsafe { urcu_memb_barrier() };
    }

    /// @func(@head) a grace period on: call_rcu().
    ///
    /// # Safety
    /// @head isn't queued already, and lives until @func runs.
    pub unsafe fn call(self, head: *mut c::rcu_head, func: unsafe extern "C" fn(*mut c::rcu_head)) {
        #[cfg(kernel)]
        match self.ssp() {
            Some(ssp) => unsafe { kernel::bindings::call_srcu(ssp, head, Some(func)) },
            None      => unsafe { kernel::bindings::call_rcu(head, Some(func)) },
        }
        #[cfg(not(kernel))]
        unsafe { urcu_memb_call_rcu(head, func) };
    }
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

#[repr(transparent)]
pub struct RcuHead(c::rcu_head);

/// The rcu_head, then the work queued in its place.
#[repr(C)]
pub union RcuWorkHead {
    rcu:  core::mem::ManuallyDrop<c::rcu_head>,
    work: core::mem::ManuallyDrop<c::work_struct>,
}

pub(crate) trait HeadKind {
    const WORK: bool;
}

impl HeadKind for RcuHead {
    const WORK: bool = false;
}

impl HeadKind for RcuWorkHead {
    const WORK: bool = true;
}

macro_rules! heads {
    ($($h:ident),*) => {$(
        unsafe impl Send for $h {}
        unsafe impl Sync for $h {}

        impl Default for $h {
            fn default() -> Self {
                unsafe { core::mem::zeroed() }
            }
        }

        unsafe impl ::pin_init::Zeroable for $h {}
    )*};
}

heads!(RcuHead, RcuWorkHead);

/// A type's own head: #[derive(RcuHead)]'s, at the field marked #[rcu_head].
///
/// # Safety
/// HEAD is the offset in Self of an RcuHead - an RcuWorkHead, if WORK -
/// nothing but its free uses.
#[rustc_specialization_trait]
pub unsafe trait HasRcuHead {
    const HEAD: usize;
    const WORK: bool;
}

unsafe impl HasRcuHead for RcuMember<RcuHead> {
    const HEAD: usize = 0;
    const WORK: bool = false;
}
unsafe impl HasRcuHead for RcuDropMember<RcuHead> {
    const HEAD: usize = 0;
    const WORK: bool = false;
}
unsafe impl HasRcuHead for RcuDropWorkMember<RcuWorkHead> {
    const HEAD: usize = 0;
    const WORK: bool = true;
}
unsafe impl<T: HasRcuHead> HasRcuHead for core::mem::ManuallyDrop<T> {
    const HEAD: usize = T::HEAD;
    const WORK: bool = T::WORK;
}
// made or not, its T's head is memory nothing but the free uses
unsafe impl<T: HasRcuHead> HasRcuHead for core::mem::MaybeUninit<T> {
    const HEAD: usize = T::HEAD;
    const WORK: bool = T::WORK;
}

pub use cstruct_macros::RcuHead;

/// What holds without owning, or owns as its T says: borrows and pointers,
/// which free nothing; the kernel's Box and Vec, as their T would; an ARef,
/// whose last put frees T as T says.
macro_rules! held_pointers {
    ($($trait:ident),*) => {$(
        unsafe impl<T: ?Sized> $trait for &T {}
        unsafe impl<T: ?Sized> $trait for &mut T {}
        unsafe impl<T: ?Sized> $trait for *const T {}
        unsafe impl<T: ?Sized> $trait for *mut T {}
        unsafe impl<T: ?Sized> $trait for NonNull<T> {}
        #[cfg(kernel)]
        unsafe impl<T: ?Sized + $trait, A: kernel::alloc::Allocator> $trait for kernel::alloc::Box<T, A> {}
        #[cfg(kernel)]
        unsafe impl<T: $trait, A: kernel::alloc::Allocator> $trait for kernel::alloc::kvec::Vec<T, A> {}
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
/// The target lives as long as Self does, and is dropped and freed only as
/// its type says: a grace period on, if its members say so.
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
    unsafe { c::rust_kvfree_rcu(p.cast(), p.cast()) };
}

/// rcu_assign_pointer(), into a pointer of C's that other threads
/// rcu_dereference(): a release store, so a reader that sees @p sees what was
/// written to it first. The Cell isn't Sync, so other threads reach it only
/// by pointer, reading it atomically: C's READ_ONCE(), the cycle detector's
/// load.
pub fn rcu_assign_pointer<T>(slot: &core::cell::Cell<*mut T>, p: *mut T) {
    unsafe { AtomicPtr::from_ptr(slot.as_ptr()) }.store(p, Ordering::Release);
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
    /// kvzalloc(), which this takes.
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

/// The head of C's `struct { struct rcu_head rcu; size_t nr; T s[]; }`, what
/// its pointer is to - with fields of its own between nr and s[] as @H:
/// bucket gens' first_bucket.
#[repr(C)]
pub struct RcuArrayHead<H = ()> {
    rcu: c::rcu_head,
    nr:  usize,
    h:   H,
}

impl<H> RcuArrayHead<H> {
    fn new(nr: usize, h: H) -> Self {
        // an rcu_head is two pointers, and kvfree_rcu() fills it in
        Self { rcu: unsafe { core::mem::zeroed() }, nr, h }
    }
}

// as its header: rcu is touched only by kvfree_rcu(), freeing it
unsafe impl<H: Send> Send for RcuArrayHead<H> {}
unsafe impl<H: Sync> Sync for RcuArrayHead<H> {}

unsafe impl<H> DstHead for RcuArrayHead<H> {
    fn nr(&self) -> usize {
        self.nr
    }
}

pub type RcuArrayBuf<T, H = ()> = DstArray<RcuArrayHead<H>, T>;

unsafe impl<T: RcuElement, H: RcuElement> RcuPointee for RcuArrayBuf<T, H> {
    type Head = RcuArrayHead<H>;

    unsafe fn from_thin(p: *mut RcuArrayHead<H>) -> *mut Self {
        unsafe { RcuArrayBuf::<T, H>::from_thin(p) }
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
    pub const NR: usize = core::mem::offset_of!(RcuArrayHead<H>, nr);
    pub const HEAD: usize = core::mem::offset_of!(RcuArrayHead<H>, h);
    pub const TAIL: usize = RcuArrayBuf::<T, H>::TAIL;

    /// None published reads as none.
    pub fn read<'a>(&'a self, g: &'a ReadGuard) -> &'a [T] {
        self.0.read(g).map_or(&[], |b| &b.elems)
    }

    pub fn read_head<'a>(&'a self, g: &'a ReadGuard) -> Option<(&'a H, &'a [T])> {
        self.0.read(g).map(|b| (&b.head.h, &b.elems))
    }

    pub fn with<R>(&self, f: impl FnOnce(&[T]) -> R) -> R {
        self.0.with(|b| f(b.map_or(&[], |b| &b.elems)))
    }

    /// # Safety
    /// As RcuBuf::get_locked().
    pub unsafe fn get_locked(&self) -> &[T] {
        unsafe { self.0.get_locked() }.map_or(&[], |b| &b.elems)
    }

    /// # Safety
    /// As RcuBuf::get_locked().
    pub unsafe fn get_head_locked(&self) -> Option<(&H, &[T])> {
        unsafe { self.0.get_locked() }.map(|b| (&b.head.h, &b.elems))
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
        let new = RcuArrayBuf::<T, H>::alloc(RcuArrayHead::new(nr, h))?;

        // no reader sees it until replace() publishes it
        unsafe {
            if let Err(e) = fill(&mut (*RcuArrayBuf::<T, H>::from_thin(new.as_ptr())).elems) {
                RcuArrayBuf::<T, H>::free(new);
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

        let head = RcuArrayHead::new((bytes - tail) / size_of::<T>(), ());
        // the old elements don't change under the writers' lock, and readers
        // keep reading them until the grace period after replace()
        let new = match NonNull::new(self.0.p.load(Ordering::Relaxed)) {
            Some(p) => unsafe { RcuArrayBuf::<T>::realloc(p, old.len(), head)? },
            None => RcuArrayBuf::<T>::alloc(head)?,
        };
        unsafe { self.0.replace(new) };
        Ok(())
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
