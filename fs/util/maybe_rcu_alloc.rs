// SPDX-License-Identifier: GPL-2.0

//! MaybeRcuAlloc<T, A>: the owner of a typed allocation, pinned, and freed as
//! T says (util/rcu.rs's Freeing).
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
use crate::util::alloc::{init_at, AllocError, Flags};
use crate::util::futex::{futex_wait, futex_wake};
use crate::util::rcu::{Detach, DropAnytime, DropAtomic, FreeAnytime, HeadKind, NothingToDetach, RcuHead, RcuWorkHead};
use crate::util::sleep::Sleep;
use crate::util::workqueue::{Queue, Workqueue, WqFlags};

#[cfg(not(kernel))]
unsafe extern "C" {
    fn urcu_memb_call_rcu(rcu: *mut c::rcu_head, func: unsafe extern "C" fn(*mut c::rcu_head));
    fn urcu_memb_barrier();
}

/// # Safety
/// alloc() gives @layout's bytes, zeroed and aligned to it; free() and,
/// in-kernel, kvfree() free them, from any context.
pub unsafe trait Alloc {
    fn alloc(layout: Layout, gfp: Flags) -> Result<NonNull<u8>, AllocError>;

    /// # Safety
    /// @p is alloc()'s, of @layout, and nothing uses it after.
    unsafe fn free(p: NonNull<u8>, layout: Layout);
}

macro_rules! allocators {
    ($($a:ident),*) => {$(
        pub struct $a;

        unsafe impl Alloc for $a {
            fn alloc(layout: Layout, gfp: Flags) -> Result<NonNull<u8>, AllocError> {
                #[cfg(kernel)]
                {
                    use kernel::alloc::{flags::__GFP_ZERO, Allocator, NumaNode};

                    Ok(<kernel::alloc::allocator::$a as Allocator>::alloc(layout, gfp | __GFP_ZERO, NumaNode::NO_NODE)?.cast())
                }
                #[cfg(not(kernel))]
                {
                    let _ = gfp;
                    NonNull::new(unsafe { std::alloc::alloc_zeroed(layout) }).ok_or(AllocError)
                }
            }

            unsafe fn free(p: NonNull<u8>, layout: Layout) {
                #[cfg(kernel)]
                unsafe { <kernel::alloc::allocator::$a as kernel::alloc::Allocator>::free(p, layout) };
                #[cfg(not(kernel))]
                unsafe { std::alloc::dealloc(p.as_ptr(), layout) };
            }
        }
    )*};
}

allocators!(Kmalloc, KVmalloc, Vmalloc);

pub(crate) fn alloc_at<T, A: Alloc, E: From<AllocError>>(init: impl PinInit<T, E>, gfp: Flags)
    -> Result<NonNull<T>, E>
{
    const { assert!(size_of::<T>() != 0, "an allocation of a zero sized type") };

    let layout = Layout::new::<T>();
    let p = A::alloc(layout, gfp)?.cast::<T>();
    match unsafe { init_at(init, p.as_ptr()) } {
        Ok(()) => Ok(p),
        Err(e) => {
            unsafe { A::free(p.cast(), layout) };
            Err(e)
        }
    }
}

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
    /// @init's T, in memory that starts zeroed - padding too - as @gfp says.
    /// Fails with @init's error, or the allocation's.
    pub fn new<E: From<AllocError>>(init: impl PinInit<T, E>, gfp: Flags) -> Result<Self, E> {
        Ok(Self { p: <T as RcuFreeing>::alloc::<A, E>(init, gfp)?, _p: PhantomData })
    }

    pub fn as_pin_mut(&mut self) -> Pin<&mut T> {
        unsafe { Pin::new_unchecked(self.p.as_mut()) }
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
        unsafe { <T as RcuFreeing>::free::<A>(self.p) }
    }
}

// ── Freeing ──────────────────────────────────────────────────────────────

/// The head a free uses: a Headed<T, H>'s, before T, which is then the
/// allocation.
trait RcuHeadAt<H: HeadKind>: Sized {
    fn alloc<A: Alloc, E: From<AllocError>>(init: impl PinInit<Self, E>, gfp: Flags) -> Result<NonNull<Self>, E>;

    /// # Safety
    /// @p is alloc()'s.
    unsafe fn head(p: NonNull<Self>) -> *mut H;

    /// # Safety
    /// @h is head()'s.
    unsafe fn of_head(h: *mut H) -> NonNull<Self>;

    /// The memory alone, its T dropped already.
    ///
    /// # Safety
    /// @p is alloc::<A>()'s, and nothing uses it after.
    unsafe fn free_mem<A: Alloc>(p: NonNull<Self>);

    /// free_mem(), a grace period on, by kvfree_rcu(): batched.
    ///
    /// # Safety
    /// @p is alloc()'s, its T dropped already, and nothing but RCU readers
    /// uses it after.
    #[cfg(kernel)]
    unsafe fn kvfree_rcu_mem(p: NonNull<Self>);
}

#[repr(C)]
struct Headed<T, H> {
    head:  H,
    value: T,
}

impl<T, H: HeadKind> RcuHeadAt<H> for T {
    fn alloc<A: Alloc, E: From<AllocError>>(init: impl PinInit<T, E>, gfp: Flags) -> Result<NonNull<T>, E> {
        let init = unsafe { pin_init_from_closure(move |slot: *mut Headed<T, H>| init_at(init, &raw mut (*slot).value)) };
        let p = alloc_at::<Headed<T, H>, A, E>(init, gfp)?;
        Ok(unsafe { NonNull::new_unchecked(&raw mut (*p.as_ptr()).value) })
    }

    unsafe fn head(p: NonNull<T>) -> *mut H {
        unsafe { p.as_ptr().byte_sub(offset_of!(Headed<T, H>, value)).cast() }
    }

    unsafe fn of_head(h: *mut H) -> NonNull<T> {
        unsafe { NonNull::new_unchecked(h.byte_add(offset_of!(Headed<T, H>, value)).cast()) }
    }

    unsafe fn free_mem<A: Alloc>(p: NonNull<T>) {
        unsafe { A::free(NonNull::new_unchecked(<T as RcuHeadAt<H>>::head(p).cast()), Layout::new::<Headed<T, H>>()) }
    }

    #[cfg(kernel)]
    unsafe fn kvfree_rcu_mem(p: NonNull<T>) {
        unsafe {
            let h = <T as RcuHeadAt<H>>::head(p);
            c::rust_kvfree_rcu(h.cast(), h.cast());
        }
    }
}

/// From the most a free has to wait for to the least: dropped after a grace
/// period on the free queue; dropped after one in the RCU callback; dropped
/// at once, its memory freed after one; freed at once.
pub(crate) trait RcuFreeing: Sized {
    /// A build error for a T that's dropped before RCU readers are done with
    /// it: what an owning RcuPtr publishes is read whole.
    fn assert_rcu_freed();

    fn alloc<A: Alloc, E: From<AllocError>>(init: impl PinInit<Self, E>, gfp: Flags) -> Result<NonNull<Self>, E>;

    /// # Safety
    /// @p is alloc::<A>()'s, and nothing but RCU readers uses it after.
    unsafe fn free<A: Alloc>(p: NonNull<Self>);
}

impl<T> RcuFreeing for T {
    default fn assert_rcu_freed() {}

    default fn alloc<A: Alloc, E: From<AllocError>>(init: impl PinInit<T, E>, gfp: Flags) -> Result<NonNull<T>, E> {
        <T as RcuHeadAt<RcuWorkHead>>::alloc::<A, E>(init, gfp)
    }

    default unsafe fn free<A: Alloc>(p: NonNull<T>) {
        <T as Detach>::detach(unsafe { p.as_ref() });
        PENDING.fetch_add(1, Relaxed);
        unsafe { call_rcu(<T as RcuHeadAt<RcuWorkHead>>::head(p).cast(), queue_fn::<T, A>) };
    }
}

impl<T: DropAtomic> RcuFreeing for T {
    default fn assert_rcu_freed() {}

    default fn alloc<A: Alloc, E: From<AllocError>>(init: impl PinInit<T, E>, gfp: Flags) -> Result<NonNull<T>, E> {
        <T as RcuHeadAt<RcuHead>>::alloc::<A, E>(init, gfp)
    }

    default unsafe fn free<A: Alloc>(p: NonNull<T>) {
        <T as Detach>::detach(unsafe { p.as_ref() });
        unsafe { call_rcu(<T as RcuHeadAt<RcuHead>>::head(p).cast(), drop_free_fn::<T, A>) };
    }
}

impl<T: DropAtomic + DropAnytime> RcuFreeing for T {
    default fn assert_rcu_freed() {
        let () = NotRcuFreed::<T>::ERR;
    }

    default fn alloc<A: Alloc, E: From<AllocError>>(init: impl PinInit<T, E>, gfp: Flags) -> Result<NonNull<T>, E> {
        <T as RcuHeadAt<RcuHead>>::alloc::<A, E>(init, gfp)
    }

    /// No detach: its drop takes it out of readers' way.
    default unsafe fn free<A: Alloc>(p: NonNull<T>) {
        unsafe {
            core::ptr::drop_in_place(p.as_ptr());
            #[cfg(kernel)]
            <T as RcuHeadAt<RcuHead>>::kvfree_rcu_mem(p);
            #[cfg(not(kernel))]
            call_rcu(<T as RcuHeadAt<RcuHead>>::head(p).cast(), free_fn::<T, A>);
        }
    }
}

impl<T: DropAtomic + DropAnytime + FreeAnytime> RcuFreeing for T {
    fn alloc<A: Alloc, E: From<AllocError>>(init: impl PinInit<T, E>, gfp: Flags) -> Result<NonNull<T>, E> {
        alloc_at::<T, A, E>(init, gfp)
    }

    unsafe fn free<A: Alloc>(p: NonNull<T>) {
        unsafe {
            core::ptr::drop_in_place(p.as_ptr());
            A::free(p.cast(), Layout::new::<T>());
        }
    }
}

struct NotRcuFreed<T>(PhantomData<T>);

impl<T> NotRcuFreed<T> {
    const ERR: () = panic!("what's read whole under RCU must be dropped a grace period on: it needs an RcuDropMember");
}

/// The callback of what was dropped at once: its memory alone - userspace's,
/// whose kvfree_rcu() allocates.
#[cfg(not(kernel))]
unsafe extern "C" fn free_fn<T, A: Alloc>(rcu: *mut c::rcu_head) {
    unsafe { <T as RcuHeadAt<RcuHead>>::free_mem::<A>(<T as RcuHeadAt<RcuHead>>::of_head(rcu.cast())) }
}

/// # Safety
/// @h is RcuHeadAt<H>::head()'s, of an alloc::<A>(), and nothing uses it after.
unsafe fn drop_free<T, H: HeadKind, A: Alloc>(h: *mut H) {
    unsafe {
        let p = <T as RcuHeadAt<H>>::of_head(h);
        core::ptr::drop_in_place(p.as_ptr());
        <T as RcuHeadAt<H>>::free_mem::<A>(p);
    }
}

unsafe extern "C" fn drop_free_fn<T, A: Alloc>(rcu: *mut c::rcu_head) {
    unsafe { drop_free::<T, RcuHead, A>(rcu.cast()) }
}

/// RcuDropWorkMembers whose drop is still to come - the word their last write
/// goes to, so barrier() waits on it.
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

unsafe extern "C" fn queue_fn<T, A: Alloc>(rcu: *mut c::rcu_head) {
    let work = rcu.cast::<c::work_struct>();
    let init = crate::new_work!(work_fn::<T, A>, "rcu_free");
    let Ok(()) = unsafe { init_at(init, work) };
    let wq = FREE_WQ.load(Acquire);
    assert!(!wq.is_null(), "an RcuDropWorkMember's freed outside module init and exit");
    unsafe { Queue::from_raw(wq).queue_work(work) };
}

unsafe extern "C" fn work_fn<T, A: Alloc>(work: *mut c::work_struct) {
    unsafe { drop_free::<T, RcuWorkHead, A>(work.cast()) };

    if PENDING.fetch_sub(1, Release) == 1 {
        futex_wake(core::ptr::from_ref(&PENDING).cast(), usize::MAX);
    }
}

unsafe fn call_rcu(rcu: *mut c::rcu_head, func: unsafe extern "C" fn(*mut c::rcu_head)) {
    #[cfg(kernel)]
    unsafe { kernel::bindings::call_rcu(rcu, Some(func)) };
    #[cfg(not(kernel))]
    unsafe { urcu_memb_call_rcu(rcu, func) };
}

/// Before what pending frees' drops need goes, and at module exit.
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
