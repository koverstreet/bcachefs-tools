// SPDX-License-Identifier: GPL-2.0

//! ReffedWork, the same in both builds, over each one's workqueue - but its
//! WorkClass, in-kernel a lockdep class per field.

use super::Queue;
use crate::c;
use crate::util::place::{Contained, Enclosing, OnePath, Path, Place, Within};
use crate::util::refcount::{RefGuard, TryRefcount};
use core::cell::UnsafeCell;
use core::marker::{PhantomData, PhantomPinned};

/// A field's work's class: in-kernel its lockdep class - @key, named @name.
#[cfg(kernel)]
pub(crate) type WorkClass = (&'static core::ffi::CStr, core::pin::Pin<&'static kernel::sync::LockClassKey>);
#[cfg(not(kernel))]
pub(crate) type WorkClass = ();

/// A ReffedWork's function as a type, and the field it runs from: for a
/// field `w: ReffedWork<f, R>`, the parent's #[derive(Init)] makes f - a
/// braced struct, a type only, beside the fn f - and this. At is that
/// field's, and only its initializer makes a ReffedWork<Self, _>.
pub(crate) trait WorkFunc: Sized + 'static {
    #[doc(hidden)]
    type At: Place<Base: Enclosing<Visible = <Self::At as Place>::Base>>;

    fn run(p: &<Self::At as Place>::Base);
}

/// The struct F's ReffedWork is the field of.
pub(crate) type Parent<F> = <<F as WorkFunc>::At as Place>::Base;

/// struct work_struct, queued and run holding refs; its field's type says
/// what: F the function, R the refs, by their paths from the struct it's in -
///
/// ```text
/// wait_for_pagecache_and_delete_work: ReffedWork<delete_unlinked, field!(super.writes)>,
/// ```
///
/// runs delete_unlinked(&bch_fs_snapshots) holding a write ref of their Fs.
///
/// The parent's #[derive(Init)] makes it in place, holding the field's At:
/// so C's pointer to it is had back as the ReffedWork, and its container()
/// as the parent. Queued, it holds R's refs until the run's done - and the
/// first keeps the parent: see WorkRefs.
///
/// C has it by address - on the workqueue's lists while it's pending, no
/// borrow held - so it's never copied, cloned or moved: it's not Copy or
/// Clone, and !Unpin, checked below. A mem::swap() of two through &mut
/// parents would still move one: place.rs's known gap, Fs never had by &mut.
#[repr(C)]
pub(crate) struct ReffedWork<F: WorkFunc, R> {
    work:       UnsafeCell<c::work_struct>,
    _placed_at: F::At,
    _r:         PhantomData<fn() -> R>,
    /// !Unpin - a PhantomData of it, as At's, for improper_ctypes
    _pin:       PhantomData<PhantomPinned>,
}

// Not Copy, Clone or Unpin: an impl of either trait makes check()'s call
// resolve to two impls of NotImpl, and fail to compile.
const _: () = {
    trait NotImpl<A> {
        fn check() {}
    }
    impl<T: ?Sized> NotImpl<()> for T {}
    struct IsClone;
    struct IsUnpin;
    impl<T: ?Sized + Clone> NotImpl<IsClone> for T {}
    impl<T: ?Sized + Unpin> NotImpl<IsUnpin> for T {}

    #[allow(dead_code)]
    fn check<F: WorkFunc, R>() {
        let _ = <ReffedWork<F, R> as NotImpl<_>>::check;
    }
};

// SAFETY: the workqueue's to synchronize, from any thread
unsafe impl<F: WorkFunc, R> Send for ReffedWork<F, R> {}
unsafe impl<F: WorkFunc, R> Sync for ReffedWork<F, R> {}

impl<F: WorkFunc, R> Contained for ReffedWork<F, R>
where
    F::At: Place<Type = Self>,
{
    type Parent = Parent<F>;
    type At = F::At;
    const AT_OFFSET: usize = core::mem::offset_of!(Self, _placed_at);
}

/// The refs a ReffedWork holds, by their paths from its parent P: one, or a
/// pair. The first's path is Within what the parent is in, so its ref keeps
/// the parent for the run; a second's count need only outlive the parent -
/// its path may go Through a BackRef. Taken in order; put in reverse, the
/// parent's keeper last - Held's drop order. Held is guards on the refs
/// tryget_live() takes, and adopt() makes.
pub(crate) trait WorkRefs<P> {
    /// The refs, held: put when it's dropped.
    type Held;

    fn tryget_live(p: &P) -> Option<Self::Held>;

    /// # Safety
    /// The refs are held, from a tryget_live() whose Held was forgotten, and
    /// given up to this.
    unsafe fn adopt(p: &P) -> Self::Held;
}

impl<P, R: Path<P> + OnePath + Within> WorkRefs<P> for R
where
    R::To: TryRefcount,
{
    type Held = RefGuard<R::To>;

    fn tryget_live(p: &P) -> Option<Self::Held> {
        R::get(p).tryget_live()
    }

    unsafe fn adopt(p: &P) -> Self::Held {
        unsafe { RefGuard::adopt(R::get(p)) }
    }
}

// B's guard first, so it's put first
impl<P, A: Path<P> + Within, B: Path<P>> WorkRefs<P> for (A, B)
where
    A::To: TryRefcount,
    B::To: TryRefcount,
{
    type Held = (RefGuard<B::To>, RefGuard<A::To>);

    fn tryget_live(p: &P) -> Option<Self::Held> {
        let a = A::get(p).tryget_live()?;
        Some((B::get(p).tryget_live()?, a))
    }

    unsafe fn adopt(p: &P) -> Self::Held {
        unsafe { (RefGuard::adopt(B::get(p)), RefGuard::adopt(A::get(p))) }
    }
}

impl<F: WorkFunc, R: WorkRefs<Parent<F>>> ReffedWork<F, R>
where
    F::At: Place<Type = Self>,
{
    /// Run F on @wq, holding R's refs: false if they can't all be had -
    /// tryget_live(). Queued already, the pending run's refs are the ones
    /// there'll be.
    pub(crate) fn queue(&self, wq: &Queue) -> bool {
        // taken first: the run can start, and put them, as soon as it's queued
        let Some(r) = R::tryget_live(self.container()) else { return false };
        // SAFETY: made in place, by new(); the refs keep it, for the run
        if unsafe { wq.queue_work(self.work.get()) } {
            // the run's, which adopts them
            core::mem::forget(r);
        }
        // else put: the pending run has its own
        true
    }

    /// # Safety
    /// @work is a ReffedWork<F, R>'s, queued by queue(), holding its refs.
    unsafe extern "C" fn work_fn(work: *mut c::work_struct) {
        // SAFETY: only a ReffedWork<F, R> runs its work_fn - its work_struct
        // first - and the refs it holds keep its parent
        let p = unsafe { (*work.cast::<Self>()).container() };
        // SAFETY: the refs queue() took for this run: put as it ends, their
        // last touch of the parent
        let _r = unsafe { R::adopt(p) };
        F::run(p);
    }

    /// INIT_WORK(), of the field's class - in-kernel its lockdep class, a
    /// static per field in the parent's #[derive(Init)].
    #[doc(hidden)]
    pub(crate) fn new<E>(at: F::At, class: WorkClass) -> impl ::pin_init::PinInit<Self, E> {
        // SAFETY: the work_struct and the At, all of it, written in place
        unsafe {
            ::pin_init::pin_init_from_closure(move |slot: *mut Self| {
                #[cfg(kernel)]
                c::init_work_with_key(slot.cast(), Some(Self::work_fn), false, class.0.as_ptr().cast(),
                                      class.1.as_ptr());
                #[cfg(not(kernel))]
                {
                    let () = class;
                    super::queues::init_work(slot.cast(), Self::work_fn);
                }
                (&raw mut (*slot)._placed_at).write(at);
                Ok(())
            })
        }
    }
}
