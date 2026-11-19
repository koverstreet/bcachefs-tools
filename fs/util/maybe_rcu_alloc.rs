// SPDX-License-Identifier: GPL-2.0

//! MaybeRcuAlloc<T, A>: the owner of a typed allocation, pinned, and freed as
//! T says (util/rcu.rs's Freeing) - by RcuFreeing, which frees a DST's too,
//! a ThinBox's: SelfSized, A's allocation, its head its own or none.
//!
//! C holds one by into_raw() and gives it back by from_raw(), never to C's
//! kvfree(): what isn't FreeAnytime may be behind a header, not at its
//! allocation's start.

use core::alloc::Layout;
use core::marker::PhantomData;
use core::mem::{offset_of, ManuallyDrop};
use core::ops::Deref;
use core::pin::Pin;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, AtomicU32, Ordering::{Acquire, Relaxed, Release}};

use pin_init::{pin_init_from_closure, PinInit};

use crate::c;
use crate::util::alloc::{flags, init_at, stateless, Alloc, AllocError, AllocGfp, Flags, KVmalloc};
use crate::util::futex::{futex_wait, futex_wake};
use crate::util::rcu::{Detach, DropAnytime, DropAtomic, Flavor, FreeAnytime, HasRcuHead, HeadKind, NothingToDetach,
                       RcuHead, RcuOwned, RcuWorkHead};
use crate::util::sleep::Sleep;
use crate::util::vstructs::SelfSized;
use crate::os::module::Quiesced;
use crate::os::workqueue::{Queue, Workqueue, WqFlags};

pub(crate) fn alloc_at<T, A: AllocGfp, E: From<AllocError>>(init: impl PinInit<T, E>, gfp: Flags)
    -> Result<NonNull<T>, E>
{
    const { assert!(size_of::<T>() != 0, "an allocation of a zero sized type") };

    let layout = Layout::new::<T>();
    let p = stateless::<A>().allocate_zeroed_gfp(layout, gfp)?.cast::<T>();
    match unsafe { init_at(init, p.as_ptr()) } {
        Ok(()) => Ok(p),
        Err(e) => {
            unsafe { stateless::<A>().deallocate(p.cast(), layout) };
            Err(e)
        }
    }
}

#[repr(transparent)]
#[derive(cstruct_macros::NonZeroWord)]
pub struct MaybeRcuAlloc<T, A: Alloc = KVmalloc> {
    p:  NonNull<T>,
    _p: PhantomData<(T, A)>,
}

unsafe impl<T: Send, A: Alloc> Send for MaybeRcuAlloc<T, A> {}
unsafe impl<T: Sync, A: Alloc> Sync for MaybeRcuAlloc<T, A> {}

// Its drop frees T as T says, so what holds one needn't wait for T's readers
unsafe impl<T, A: Alloc> FreeAnytime for MaybeRcuAlloc<T, A> {}
unsafe impl<T, A: Alloc> DropAnytime for MaybeRcuAlloc<T, A> {}
unsafe impl<T, A: Alloc> DropAtomic for MaybeRcuAlloc<T, A> {}
unsafe impl<T, A: Alloc> NothingToDetach for MaybeRcuAlloc<T, A> {}

impl<T, A: Alloc> MaybeRcuAlloc<T, A> {
    /// @init's T, in memory that starts zeroed - padding too. Fails with
    /// @init's error, or the allocation's.
    pub fn new<E: From<AllocError>>(init: impl PinInit<T, E>) -> Result<Self, E>
    where
        A: AllocGfp,
    {
        Self::new_gfp(init, Flags::empty())
    }

    /// new(), for what mustn't fail to allocate: failing only with @init's
    /// error.
    pub fn new_nofail<E: From<AllocError>>(init: impl PinInit<T, E>) -> Result<Self, E>
    where
        A: AllocGfp,
    {
        Self::new_gfp(init, flags::NOFAIL)
    }

    fn new_gfp<E: From<AllocError>>(init: impl PinInit<T, E>, gfp: Flags) -> Result<Self, E>
    where
        A: AllocGfp,
    {
        Ok(Self { p: <T as RcuFreeing>::alloc::<A, E>(init, gfp)?, _p: PhantomData })
    }

    pub fn as_pin_mut(&mut self) -> Pin<&mut T> {
        unsafe { Pin::new_unchecked(self.p.as_mut()) }
    }

    /// The T, by pointer, borrowed mutably: for a handle to it.
    pub fn as_non_null(&mut self) -> NonNull<T> {
        self.p
    }

    pub fn into_raw(self) -> NonNull<T> {
        ManuallyDrop::new(self).p
    }

    /// # Safety
    /// @p is into_raw()'s, of a MaybeRcuAlloc<T, A>, taken back once.
    pub unsafe fn from_raw(p: NonNull<T>) -> Self {
        Self { p, _p: PhantomData }
    }

    /// T dropped now, and its memory given up to wait out readers by other
    /// means than its own free's. A build error for a T that's not an
    /// RcuMember's.
    pub fn retire(self) -> Retired<T, A> {
        let p = self.into_raw();
        unsafe { <T as RcuFreeing>::retire(p) };
        Retired(unsafe { Freeing::new(p) })
    }
}

/// retire()'s memory, which readers may still be in: dropping it leaks it.
pub struct Retired<T, A: Alloc = KVmalloc>(Freeing<T, RcuHead, A>);

// memory, its T gone
unsafe impl<T, A: Alloc> Sync for Retired<T, A> {}

impl<T, A: Alloc> Retired<T, A> {
    pub fn into_head(self) -> NonNull<c::rcu_head> {
        <T as RcuHeadAt<RcuHead>>::head(&self.0).cast()
    }

    /// # Safety
    /// @h is into_head()'s, of a Retired<T, A>, taken back once.
    pub unsafe fn from_head(h: NonNull<c::rcu_head>) -> Self {
        Retired(unsafe { Freeing::from_head(h.cast()) })
    }

    /// # Safety
    /// Its readers are past it.
    pub unsafe fn free(self) {
        self.0.free_mem()
    }
}

impl<T, A: Alloc> Deref for MaybeRcuAlloc<T, A> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { self.p.as_ref() }
    }
}

impl<T, A: Alloc> Drop for MaybeRcuAlloc<T, A> {
    fn drop(&mut self) {
        unsafe { <T as RcuFreeing>::free::<A>(self.p) }
    }
}

// ── Freeing ──────────────────────────────────────────────────────────────

/// The head a free uses: T's own, or a Headed<T, H>'s, which is then the
/// allocation. A DST - SelfSized - is never allocated here, A's alone: its
/// head is its own, or it has none, and then only a free that needs none may
/// be used on it, its memory a grace period on by kvfree_rcu().
trait RcuHeadAt<H: HeadKind> {
    fn alloc<A: AllocGfp, E: From<AllocError>>(init: impl PinInit<Self, E>, gfp: Flags) -> Result<NonNull<Self>, E>
    where
        Self: Sized;

    /// The head, in the allocation @f has.
    fn head<A: Alloc>(f: &Freeing<Self, H, A>) -> NonNull<H>;

    /// # Safety
    /// @h is head()'s, of a Freeing given up as it.
    unsafe fn of_head(h: NonNull<H>) -> NonNull<Self>;

    /// The memory alone: what's in it dropped already, or never to be.
    fn free_mem<A: Alloc>(f: Freeing<Self, H, A>);

    /// free_mem(), a grace period on, RCU readers using it till then: by
    /// kvfree_rcu(), batched, if kvfree() frees A's memory, else a callback.
    fn free_mem_rcu<A: Alloc>(f: Freeing<Self, H, A>);
}

#[repr(C)]
struct Headed<T, H> {
    head:  H,
    value: T,
}

impl<T, H: HeadKind> RcuHeadAt<H> for T {
    default fn alloc<A: AllocGfp, E: From<AllocError>>(init: impl PinInit<T, E>, gfp: Flags) -> Result<NonNull<T>, E> {
        let init = unsafe { pin_init_from_closure(move |slot: *mut Headed<T, H>| init_at(init, &raw mut (*slot).value)) };
        let p = alloc_at::<Headed<T, H>, A, E>(init, gfp)?;
        Ok(unsafe { NonNull::new_unchecked(&raw mut (*p.as_ptr()).value) })
    }

    default fn head<A: Alloc>(f: &Freeing<T, H, A>) -> NonNull<H> {
        // the allocation's start, a Headed<T, H>
        unsafe { f.0.byte_sub(offset_of!(Headed<T, H>, value)).cast() }
    }

    default unsafe fn of_head(h: NonNull<H>) -> NonNull<T> {
        unsafe { h.byte_add(offset_of!(Headed<T, H>, value)).cast() }
    }

    default fn free_mem<A: Alloc>(f: Freeing<T, H, A>) {
        let head = <T as RcuHeadAt<H>>::head(&f).cast();
        unsafe { stateless::<A>().deallocate(head, Layout::new::<Headed<T, H>>()) }
    }

    default fn free_mem_rcu<A: Alloc>(f: Freeing<T, H, A>) {
        #[cfg(kernel)]
        if A::KVFREE {
            let h = <T as RcuHeadAt<H>>::head(&f).as_ptr();
            return unsafe { crate::util::alloc::kvfree_rcu_head(h.cast(), h.cast()) };
        }
        Flavor::RCU.call(f, Freeing::free_mem);
    }
}

impl<T: HasRcuHead, H: HeadKind> RcuHeadAt<H> for T {
    fn alloc<A: AllocGfp, E: From<AllocError>>(init: impl PinInit<T, E>, gfp: Flags) -> Result<NonNull<T>, E> {
        const { assert!(!H::WORK || T::WORK, "an RcuDropWorkMember's head is an RcuWorkHead, with room for the work") };
        alloc_at::<T, A, E>(init, gfp)
    }

    fn head<A: Alloc>(f: &Freeing<T, H, A>) -> NonNull<H> {
        unsafe { f.0.byte_add(T::HEAD).cast() }
    }

    unsafe fn of_head(h: NonNull<H>) -> NonNull<T> {
        unsafe { h.byte_sub(T::HEAD).cast() }
    }

    fn free_mem<A: Alloc>(f: Freeing<T, H, A>) {
        unsafe { stateless::<A>().deallocate(f.0.cast(), Layout::new::<T>()) }
    }

    fn free_mem_rcu<A: Alloc>(f: Freeing<T, H, A>) {
        own_head_free_mem_rcu(f)
    }
}

// A DST's head is its own or none: what reaches it from its head is
// from_thin(), and it's sized by its reference.
impl<T: ?Sized + SelfSized, H: HeadKind> RcuHeadAt<H> for T {
    default fn alloc<A: AllocGfp, E: From<AllocError>>(_: impl PinInit<T, E>, _: Flags) -> Result<NonNull<T>, E>
    where
        T: Sized,
    {
        unreachable!("SelfSized is never Sized")
    }

    default fn head<A: Alloc>(_: &Freeing<T, H, A>) -> NonNull<H> {
        let () = NoHead::<T>::ERR;
        unreachable!()
    }

    default unsafe fn of_head(_: NonNull<H>) -> NonNull<T> {
        let () = NoHead::<T>::ERR;
        unreachable!()
    }

    default fn free_mem<A: Alloc>(f: Freeing<T, H, A>) {
        // dropped, but its size is its pointer's metadata: nothing's read
        unsafe { stateless::<A>().deallocate(f.0.cast(), Layout::for_value(f.0.as_ref())) }
    }

    /// No head to queue: kvfree_rcu() may wait out the grace period itself.
    default fn free_mem_rcu<A: Alloc>(f: Freeing<T, H, A>) {
        const { assert!(A::KVFREE, "a DST freed a grace period on with no rcu_head of its own: by kvfree_rcu(), \
                                    and kvfree() doesn't free its allocator's memory") };
        unsafe { crate::util::alloc::kfree_rcu_mightsleep(f.0.as_ptr().cast()) }
    }
}

impl<T: ?Sized + SelfSized + HasRcuHead, H: HeadKind> RcuHeadAt<H> for T {
    fn head<A: Alloc>(f: &Freeing<T, H, A>) -> NonNull<H> {
        unsafe { f.0.byte_add(T::HEAD).cast() }
    }

    unsafe fn of_head(h: NonNull<H>) -> NonNull<T> {
        unsafe { NonNull::new_unchecked(T::from_thin(h.byte_sub(T::HEAD).as_ptr().cast()).cast_mut()) }
    }

    fn free_mem_rcu<A: Alloc>(f: Freeing<T, H, A>) {
        own_head_free_mem_rcu(f)
    }
}

struct NoHead<T: ?Sized>(PhantomData<T>);

impl<T: ?Sized> NoHead<T> {
    const ERR: () = panic!("a DST freed by a callback - dropped after a grace period - needs an rcu_head of its own");
}

/// free_mem_rcu() by T's own head: kvfree_rcu() of T, batched, if kvfree()
/// frees A's memory.
fn own_head_free_mem_rcu<T: ?Sized + HasRcuHead + RcuHeadAt<H>, H: HeadKind, A: Alloc>(f: Freeing<T, H, A>) {
    #[cfg(kernel)]
    if A::KVFREE {
        // tiny RCU passes the head's offset as its callback
        const { assert!(T::HEAD < 4096, "kvfree_rcu() of a head more than a page into its object") };
        let h = <T as RcuHeadAt<H>>::head(&f);
        return unsafe { crate::util::alloc::kvfree_rcu_head(f.0.as_ptr().cast(), h.as_ptr().cast()) };
    }
    Flavor::RCU.call(f, Freeing::free_mem);
}

/// From the most a free has to wait for to the least: dropped after a grace
/// period on the free queue; dropped after one in the RCU callback; dropped
/// at once, its memory freed after one; freed at once.
pub(crate) trait RcuFreeing {
    /// A build error for a T that's dropped before RCU readers are done with
    /// it: what an owning RcuPtr publishes is read whole.
    fn assert_rcu_freed();

    fn alloc<A: AllocGfp, E: From<AllocError>>(init: impl PinInit<Self, E>, gfp: Flags) -> Result<NonNull<Self>, E>
    where
        Self: Sized;

    /// # Safety
    /// @p is alloc::<A>()'s - a DST's, A's - and nothing but RCU readers uses
    /// it after.
    unsafe fn free<A: Alloc>(p: NonNull<Self>);

    /// An RcuMember's free, but for the memory: T dropped, its memory left
    /// for the caller to free once readers are past it.
    ///
    /// # Safety
    /// As free()'s.
    unsafe fn retire(p: NonNull<Self>);
}

impl<T: ?Sized + RcuHeadAt<RcuHead> + RcuHeadAt<RcuWorkHead>> RcuFreeing for T {
    default fn assert_rcu_freed() {}

    default fn alloc<A: AllocGfp, E: From<AllocError>>(init: impl PinInit<T, E>, gfp: Flags) -> Result<NonNull<T>, E>
    where
        T: Sized,
    {
        <T as RcuHeadAt<RcuWorkHead>>::alloc::<A, E>(init, gfp)
    }

    default unsafe fn free<A: Alloc>(p: NonNull<T>) {
        <T as Detach>::detach(unsafe { p.as_ref() });
        PENDING.fetch_add(1, Relaxed);
        Flavor::RCU.call(unsafe { Freeing::<T, RcuWorkHead, A>::new(p) }, Freeing::queue);
    }

    default unsafe fn retire(_p: NonNull<T>) {
        let () = NotRetirable::<T>::ERR;
    }
}

impl<T: ?Sized + RcuHeadAt<RcuHead> + RcuHeadAt<RcuWorkHead> + DropAtomic> RcuFreeing for T {
    default fn assert_rcu_freed() {}

    default fn alloc<A: AllocGfp, E: From<AllocError>>(init: impl PinInit<T, E>, gfp: Flags) -> Result<NonNull<T>, E>
    where
        T: Sized,
    {
        <T as RcuHeadAt<RcuHead>>::alloc::<A, E>(init, gfp)
    }

    default unsafe fn free<A: Alloc>(p: NonNull<T>) {
        <T as Detach>::detach(unsafe { p.as_ref() });
        Flavor::RCU.call(unsafe { Freeing::<T, RcuHead, A>::new(p) }, Freeing::drop_free);
    }

    default unsafe fn retire(_p: NonNull<T>) {
        let () = NotRetirable::<T>::ERR;
    }
}

impl<T: ?Sized + RcuHeadAt<RcuHead> + RcuHeadAt<RcuWorkHead> + DropAtomic + DropAnytime> RcuFreeing for T {
    default fn assert_rcu_freed() {
        let () = NotRcuFreed::<T>::ERR;
    }

    default fn alloc<A: AllocGfp, E: From<AllocError>>(init: impl PinInit<T, E>, gfp: Flags) -> Result<NonNull<T>, E>
    where
        T: Sized,
    {
        <T as RcuHeadAt<RcuHead>>::alloc::<A, E>(init, gfp)
    }

    /// No detach: its drop takes it out of readers' way.
    default unsafe fn free<A: Alloc>(p: NonNull<T>) {
        unsafe { core::ptr::drop_in_place(p.as_ptr()) };
        <T as RcuHeadAt<RcuHead>>::free_mem_rcu(unsafe { Freeing::<T, RcuHead, A>::new(p) });
    }

    default unsafe fn retire(p: NonNull<T>) {
        unsafe { core::ptr::drop_in_place(p.as_ptr()) }
    }
}

impl<T: ?Sized + RcuHeadAt<RcuHead> + RcuHeadAt<RcuWorkHead> + DropAtomic + DropAnytime + FreeAnytime> RcuFreeing
    for T
{
    fn alloc<A: AllocGfp, E: From<AllocError>>(init: impl PinInit<T, E>, gfp: Flags) -> Result<NonNull<T>, E>
    where
        T: Sized,
    {
        alloc_at::<T, A, E>(init, gfp)
    }

    unsafe fn free<A: Alloc>(p: NonNull<T>) {
        unsafe {
            let layout = Layout::for_value(p.as_ref());
            core::ptr::drop_in_place(p.as_ptr());
            stateless::<A>().deallocate(p.cast(), layout);
        }
    }

    unsafe fn retire(_p: NonNull<T>) {
        let () = NotRetirable::<T>::ERR;
    }
}

struct NotRcuFreed<T: ?Sized>(PhantomData<T>);

impl<T: ?Sized> NotRcuFreed<T> {
    const ERR: () = panic!("what's read whole under RCU must be dropped a grace period on: it needs an RcuDropMember");
}

struct NotRetirable<T: ?Sized>(PhantomData<T>);

impl<T: ?Sized> NotRetirable<T> {
    const ERR: () = panic!("retire() is an RcuMember's free - dropped at once, its memory a grace period on");
}

/// RcuDropWorkMembers whose drop is still to come - the word their last write
/// goes to, so barrier() waits on it.
static PENDING: AtomicU32 = AtomicU32::new(0);

/// From module init to exit.
static FREE_WQ: AtomicPtr<c::workqueue_struct> = AtomicPtr::new(core::ptr::null_mut());

pub fn free_init() -> Result<(), AllocError> {
    let wq = Workqueue::new(c"bcachefs_rcu_free", WqFlags::UNBOUND, 0)?;
    FREE_WQ.store(wq.into_raw(), Release);
    Ok(())
}

/// Nothing's on it, nor will be.
pub fn free_exit(_: &Quiesced<'_>) {
    drop(unsafe { Workqueue::from_raw(FREE_WQ.swap(core::ptr::null_mut(), Acquire)) });
}

/// An alloc::<A>()'s, out of readers' reach and its free begun: given up to a
/// grace period at its head, H.
struct Freeing<T: ?Sized, H: HeadKind, A: Alloc>(NonNull<T>, PhantomData<(H, A)>);

// memory, and what's in it dropped on whichever thread its callback runs:
// RcuFreeing's impls hold T to what may be
unsafe impl<T: ?Sized, H: HeadKind, A: Alloc> Send for Freeing<T, H, A> {}

// free()'s alone, once it's begun
unsafe impl<T: ?Sized + RcuHeadAt<H>, H: HeadKind, A: Alloc> RcuOwned for Freeing<T, H, A> {
    fn into_head(self) -> NonNull<RcuHead> {
        <T as RcuHeadAt<H>>::head(&self).cast()
    }

    unsafe fn from_head(head: NonNull<RcuHead>) -> Self {
        Freeing(unsafe { <T as RcuHeadAt<H>>::of_head(head.cast()) }, PhantomData)
    }
}

impl<T: ?Sized + RcuHeadAt<H>, H: HeadKind, A: Alloc> Freeing<T, H, A> {
    /// # Safety
    /// @p is alloc::<A>()'s, and nothing but RCU readers uses it after.
    unsafe fn new(p: NonNull<T>) -> Self {
        Freeing(p, PhantomData)
    }

    fn drop_free(self) {
        unsafe { core::ptr::drop_in_place(self.0.as_ptr()) };
        <T as RcuHeadAt<H>>::free_mem(self);
    }

    /// What was dropped at once: its memory alone - for what kvfree() doesn't
    /// free, or userspace's, whose kvfree_rcu() allocates.
    fn free_mem(self) {
        <T as RcuHeadAt<H>>::free_mem(self)
    }
}

impl<T: ?Sized + RcuHeadAt<RcuWorkHead>, A: Alloc> Freeing<T, RcuWorkHead, A> {
    /// Its drop needs process context: on the free queue, the rcu_head's
    /// room become the work's.
    fn queue(self) {
        let work = <T as RcuHeadAt<RcuWorkHead>>::head(&self).cast::<c::work_struct>().as_ptr();
        let init = crate::new_work!(work_fn::<T, A>, "rcu_free");
        let Ok(()) = unsafe { init_at(init, work) };
        let wq = FREE_WQ.load(Acquire);
        assert!(!wq.is_null(), "an RcuDropWorkMember's freed outside module init and exit");
        unsafe { Queue::from_raw(wq).queue_work(work) };
    }
}

unsafe extern "C" fn work_fn<T: ?Sized + RcuHeadAt<RcuWorkHead>, A: Alloc>(work: *mut c::work_struct) {
    unsafe { Freeing::<T, RcuWorkHead, A>::from_head(NonNull::new_unchecked(work.cast())) }.drop_free();

    if PENDING.fetch_sub(1, Release) == 1 {
        futex_wake(core::ptr::from_ref(&PENDING).cast(), usize::MAX);
    }
}

/// Before what pending frees' drops need goes, and at module exit.
pub fn barrier() {
    Flavor::RCU.barrier();

    loop {
        let n = PENDING.load(Acquire);
        if n == 0 {
            return;
        }
        let Ok(()) = futex_wait(&PENDING, n, Sleep::UNINTERRUPTIBLE);
    }
}
