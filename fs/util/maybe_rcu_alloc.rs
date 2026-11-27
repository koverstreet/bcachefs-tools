// SPDX-License-Identifier: GPL-2.0

//! MaybeRcuAlloc<T, A>: the owner of a typed allocation, pinned, and freed as
//! T says (util/rcu.rs's Freeing) - by RcuFreeing, which frees a DST's too,
//! a ThinBox's: SelfSized, A's allocation, its head its own or before it.
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
use crate::util::alloc::{flags, init_at, stateless, Alloc, AllocError, AllocGfp, Flags, Global};
use crate::util::futex::{futex_wait, futex_wake};
use crate::util::rcu::{Detach, DropAnytime, DropAtomic, Flavor, FreeAnytime, HasRcuHead, HeadKind, NothingToDetach,
                       RcuHead, RcuOwned, RcuWorkHead};
use crate::util::sleep::Sleep;
use crate::util::vstructs::SelfSized;
use crate::os::module::Quiesced;
use crate::os::workqueue::{Queue, RawWork, Workqueue, WqFlags};

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
pub struct MaybeRcuAlloc<T, A: Alloc = Global> {
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

// None is its pointer null - zeroed
unsafe impl<T, A: Alloc> ::pin_init::ZeroableOption for MaybeRcuAlloc<T, A> {}

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

    pub fn new_gfp<E: From<AllocError>>(init: impl PinInit<T, E>, gfp: Flags) -> Result<Self, E>
    where
        A: AllocGfp,
    {
        Ok(Self { p: <T as RcuFreeing>::alloc::<A, E>(init, gfp)?, _p: PhantomData })
    }

    pub fn as_pin(&self) -> Pin<&T> {
        unsafe { Pin::new_unchecked(self.p.as_ref()) }
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
}

impl<T, A: Alloc> Deref for MaybeRcuAlloc<T, A> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { self.p.as_ref() }
    }
}

impl<T, A: Alloc> Drop for MaybeRcuAlloc<T, A> {
    fn drop(&mut self) {
        unsafe { <T as RcuFreeing>::free(self.p, &stateless::<A>()) }
    }
}

// ── Freeing ──────────────────────────────────────────────────────────────

/// The head a free uses: T's own, or one put before it - a Headed<T, H>, or a
/// DST's maker's allocation at layout_of() - which is then the allocation's
/// start. A DST isn't reached from a head before it: how far past the head it
/// is goes by its alignment, which only a reference to it says.
trait RcuHeadAt<H: HeadKind> {
    fn alloc<A: AllocGfp, E: From<AllocError>>(init: impl PinInit<Self, E>, gfp: Flags) -> Result<NonNull<Self>, E>
    where
        Self: Sized;

    /// The allocation a value laid out as @value is in, and its offset there.
    fn layout_of(value: Layout) -> Result<(Layout, usize), AllocError>;

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

    default fn layout_of(value: Layout) -> Result<(Layout, usize), AllocError> {
        head_before(Layout::new::<H>(), value)
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
        if A::KVFREE {
            let h = <T as RcuHeadAt<H>>::head(&f);
            return unsafe { crate::util::alloc::kvfree_rcu::<H>(h.cast(), Layout::new::<Headed<T, H>>()) };
        }
        Flavor::RCU.call(f, Freeing::free_mem);
    }
}

impl<T: HasRcuHead, H: HeadKind> RcuHeadAt<H> for T {
    fn alloc<A: AllocGfp, E: From<AllocError>>(init: impl PinInit<T, E>, gfp: Flags) -> Result<NonNull<T>, E> {
        const { assert!(!H::WORK || T::WORK, "an RcuDropWorkMember's head is an RcuWorkHead, with room for the work") };
        alloc_at::<T, A, E>(init, gfp)
    }

    fn layout_of(value: Layout) -> Result<(Layout, usize), AllocError> {
        Ok((value, 0))
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

// A DST's head is its own or before it, in the allocation its maker made at
// layout_of(): what reaches it from its head is from_thin(), and it's sized by
// its reference.
impl<T: ?Sized + SelfSized, H: HeadKind> RcuHeadAt<H> for T {
    default fn alloc<A: AllocGfp, E: From<AllocError>>(_: impl PinInit<T, E>, _: Flags) -> Result<NonNull<T>, E>
    where
        T: Sized,
    {
        unreachable!("SelfSized is never Sized")
    }

    default fn layout_of(value: Layout) -> Result<(Layout, usize), AllocError> {
        head_before(Layout::new::<H>(), value)
    }

    default fn head<A: Alloc>(f: &Freeing<T, H, A>) -> NonNull<H> {
        allocation::<T, H>(f.0).0.cast()
    }

    default unsafe fn of_head(_: NonNull<H>) -> NonNull<T> {
        let () = NoHead::<T>::ERR;
        unreachable!()
    }

    default fn free_mem<A: Alloc>(f: Freeing<T, H, A>) {
        let (base, layout) = allocation::<T, H>(f.0);
        unsafe { stateless::<A>().deallocate(base, layout) }
    }

    /// By kvfree_rcu() of the head before it, the allocation's start: no
    /// callback, which would have to reach it from its head.
    default fn free_mem_rcu<A: Alloc>(f: Freeing<T, H, A>) {
        const { assert!(A::KVFREE, "a DST freed a grace period on with no rcu_head of its own: by kvfree_rcu(), \
                                    and kvfree() doesn't free its allocator's memory") };
        let (base, layout) = allocation::<T, H>(f.0);
        unsafe { crate::util::alloc::kvfree_rcu::<H>(base, layout) }
    }
}

impl<T: ?Sized + SelfSized + HasRcuHead, H: HeadKind> RcuHeadAt<H> for T {
    fn layout_of(value: Layout) -> Result<(Layout, usize), AllocError> {
        Ok((value, 0))
    }

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
    const ERR: () = panic!("freed a grace period on with no room for a head before it - a DST dropped by the callback, \
                            or a box's T: it needs an rcu_head of its own");
}

/// A build error for a T freed a grace period on - !FreeAnytime - with no head
/// of its own: one in memory of its own layout, a box's, has no room for one
/// before it.
pub(crate) fn assert_head_in_place<T: ?Sized>() {
    <T as InPlace>::assert();
}

trait InPlace {
    fn assert();
}

impl<T: ?Sized> InPlace for T {
    default fn assert() {
        <T as OwnHead>::assert();
    }
}

impl<T: ?Sized + FreeAnytime> InPlace for T {
    fn assert() {}
}

trait OwnHead {
    fn assert();
}

impl<T: ?Sized> OwnHead for T {
    default fn assert() {
        let () = NoHead::<T>::ERR;
    }
}

impl<T: ?Sized + HasRcuHead> OwnHead for T {
    fn assert() {}
}

/// @head, then @value: a Headed's layout, and its value's offset.
fn head_before(head: Layout, value: Layout) -> Result<(Layout, usize), AllocError> {
    let (layout, offset) = head.extend(value).map_err(|_| AllocError)?;
    Ok((layout.pad_to_align(), offset))
}

/// The start of the allocation @p is in, and its layout.
fn allocation<T: ?Sized + RcuHeadAt<H>, H: HeadKind>(p: NonNull<T>) -> (NonNull<u8>, Layout) {
    // dropped, but its size is its pointer's metadata: nothing's read
    let (layout, offset) = T::layout_of(Layout::for_value(unsafe { p.as_ref() })).expect("its own allocation's layout");
    (unsafe { p.cast::<u8>().byte_sub(offset) }, layout)
}

/// free_mem_rcu() by T's own head: kvfree_rcu() of T, batched, if kvfree()
/// frees A's memory.
fn own_head_free_mem_rcu<T: ?Sized + HasRcuHead + RcuHeadAt<H>, H: HeadKind, A: Alloc>(f: Freeing<T, H, A>) {
    if A::KVFREE {
        let (base, layout) = allocation::<T, H>(f.0);
        return unsafe { crate::util::alloc::kvfree_rcu::<T>(base, layout) };
    }
    Flavor::RCU.call(f, Freeing::free_mem);
}

/// From the most a free has to wait for to the least: dropped after a grace
/// period on the free queue; dropped after one in the RCU callback; dropped
/// at once, its memory freed after one; freed at once.
pub(crate) trait RcuFreeing {
    /// A build error for a T that's freed before RCU readers are done with it:
    /// FreeAnytime. What its drop frees is its members' to say.
    fn assert_rcu_freed();

    fn alloc<A: AllocGfp, E: From<AllocError>>(init: impl PinInit<Self, E>, gfp: Flags) -> Result<NonNull<Self>, E>
    where
        Self: Sized;

    /// The allocation a value laid out as @value is in, and its offset there:
    /// what a DST's maker allocates, for free() to free.
    fn layout_of(value: Layout) -> Result<(Layout, usize), AllocError>;

    /// What's freed at once goes back to @a; what waits out readers, to
    /// stateless() or kvfree_rcu(), none of @a's state kept till then.
    ///
    /// # Safety
    /// @p is @a's: alloc()'s, or a DST's at layout_of() its value's layout.
    /// Nothing but RCU readers uses it after.
    unsafe fn free<A: Alloc>(p: NonNull<Self>, a: &A);

    /// Whether it's dropped at its free, nothing readers reach going with it,
    /// or after a grace period.
    fn drops_at_free() -> bool;

    /// A build error unless drops_at_free().
    fn assert_drops_at_free();
}

impl<T: ?Sized + RcuHeadAt<RcuHead> + RcuHeadAt<RcuWorkHead>> RcuFreeing for T {
    default fn assert_rcu_freed() {}

    default fn alloc<A: AllocGfp, E: From<AllocError>>(init: impl PinInit<T, E>, gfp: Flags) -> Result<NonNull<T>, E>
    where
        T: Sized,
    {
        <T as RcuHeadAt<RcuWorkHead>>::alloc::<A, E>(init, gfp)
    }

    default fn layout_of(value: Layout) -> Result<(Layout, usize), AllocError> {
        <T as RcuHeadAt<RcuWorkHead>>::layout_of(value)
    }

    default unsafe fn free<A: Alloc>(p: NonNull<T>, _: &A) {
        <T as Detach>::detach(unsafe { p.as_ref() });
        PENDING.fetch_add(1, Relaxed);
        Flavor::RCU.call(unsafe { Freeing::<T, RcuWorkHead, A>::new(p) }, Freeing::queue);
    }

    default fn drops_at_free() -> bool {
        false
    }

    default fn assert_drops_at_free() {
        let () = NotDroppedAtFree::<T>::ERR;
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

    default fn layout_of(value: Layout) -> Result<(Layout, usize), AllocError> {
        <T as RcuHeadAt<RcuHead>>::layout_of(value)
    }

    default unsafe fn free<A: Alloc>(p: NonNull<T>, _: &A) {
        <T as Detach>::detach(unsafe { p.as_ref() });
        Flavor::RCU.call(unsafe { Freeing::<T, RcuHead, A>::new(p) }, Freeing::drop_free);
    }

    default fn drops_at_free() -> bool {
        false
    }

    default fn assert_drops_at_free() {
        let () = NotDroppedAtFree::<T>::ERR;
    }
}

impl<T: ?Sized + RcuHeadAt<RcuHead> + RcuHeadAt<RcuWorkHead> + DropAtomic + DropAnytime> RcuFreeing for T {
    default fn assert_rcu_freed() {}

    default fn alloc<A: AllocGfp, E: From<AllocError>>(init: impl PinInit<T, E>, gfp: Flags) -> Result<NonNull<T>, E>
    where
        T: Sized,
    {
        <T as RcuHeadAt<RcuHead>>::alloc::<A, E>(init, gfp)
    }

    default fn layout_of(value: Layout) -> Result<(Layout, usize), AllocError> {
        <T as RcuHeadAt<RcuHead>>::layout_of(value)
    }

    /// No detach: its drop takes it out of readers' way.
    default unsafe fn free<A: Alloc>(p: NonNull<T>, _: &A) {
        unsafe { core::ptr::drop_in_place(p.as_ptr()) };
        <T as RcuHeadAt<RcuHead>>::free_mem_rcu(unsafe { Freeing::<T, RcuHead, A>::new(p) });
    }

    default fn drops_at_free() -> bool {
        true
    }

    default fn assert_drops_at_free() {}
}

impl<T: ?Sized + RcuHeadAt<RcuHead> + RcuHeadAt<RcuWorkHead> + DropAtomic + DropAnytime + FreeAnytime> RcuFreeing
    for T
{
    fn assert_rcu_freed() {
        let () = NotRcuFreed::<T>::ERR;
    }

    fn alloc<A: AllocGfp, E: From<AllocError>>(init: impl PinInit<T, E>, gfp: Flags) -> Result<NonNull<T>, E>
    where
        T: Sized,
    {
        alloc_at::<T, A, E>(init, gfp)
    }

    fn layout_of(value: Layout) -> Result<(Layout, usize), AllocError> {
        Ok((value, 0))
    }

    unsafe fn free<A: Alloc>(p: NonNull<T>, a: &A) {
        unsafe {
            let layout = Layout::for_value(p.as_ref());
            core::ptr::drop_in_place(p.as_ptr());
            a.deallocate(p.cast(), layout);
        }
    }
}

struct NotRcuFreed<T: ?Sized>(PhantomData<T>);

impl<T: ?Sized> NotRcuFreed<T> {
    const ERR: () = panic!("what's read under RCU must be freed a grace period on - !FreeAnytime: it needs an RcuMember");
}

struct NotDroppedAtFree<T: ?Sized>(PhantomData<T>);

impl<T: ?Sized> NotDroppedAtFree<T> {
    const ERR: () = panic!("dropped once readers are past it - not DropAnytime: what frees it can't drop it first");
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
        let work = <T as RcuHeadAt<RcuWorkHead>>::head(&self).cast::<RawWork>().as_ptr();
        let init = crate::new_work!(work_fn::<T, A>, "rcu_free");
        let Ok(()) = unsafe { init_at(init, work) };
        let wq = FREE_WQ.load(Acquire);
        assert!(!wq.is_null(), "an RcuDropWorkMember's freed outside module init and exit");
        unsafe { (*work).queue(Queue::from_raw(wq)) };
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
