// SPDX-License-Identifier: GPL-2.0

//! A bio's owners. Each owns a T that ends in its bio - HasBio: the bio is
//! the T's last field, and the T is found from it by that offset.
//!
//! - BioBox<T>: a T from a BioSet<T>, a bioset whose front_pad is the rest of
//!   the T - or, from BioBox::new(), a T and its inline bvecs, in one
//!   allocation of their own. Dropped, it's put to its set, or freed.
//! - TaskSlot::exclusive_bio_in(): a T that's a task's home, its bvecs inline
//!   after the task's box; the T's drop uninit()s its bio.
//!
//! They deref to the T, and change it through Pin<&mut T>: the T holds its
//! bio, which is pinned.
//!
//! A T is made around its bio: the bioset, or BioBox::new(), puts the bio
//! where it goes, then the caller's initializer does the rest of the T - its
//! bio field from BioInPlace::init(), which writes nothing.
//!
//! A box submitted to a Gather (BioBox::submit_gathered()) is given up to its
//! IO, and its endio has it back from the bio, drops it, then puts what its T
//! makes of the IO's status (Gathered) to the gather.

use core::alloc::Layout;
use core::cell::UnsafeCell;
use core::future::Future;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicBool, Ordering::{Acquire, Release}};
use core::ops::{Deref, DerefMut};
use core::pin::Pin;
use core::ptr::NonNull;

use pin_init::{pin_data, PinInit};

use super::{bio_of, of_bio, Bio, BioBuffersMapped, BioBuffersUnmanaged, BioField, BioOwner, Buffers, HasBio,
            MapBuf};
use crate::util::gather::{Fold, GatherRef, Width};
use crate::util::task::{Exclusive, TaskSlot, Waiter};
use crate::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::util::alloc::{current_gfp_context_raw, init_at, kfree, kzalloc, Alloc};
use crate::util::ffi::Opaque;

pub struct BioInPlace(*mut c::bio);

impl BioInPlace {
    pub fn init<E>(self) -> impl PinInit<Bio, E> {
        let bio = self.0;
        // SAFETY: writes nothing, the bio being there - its slot, checked
        unsafe {
            ::pin_init::pin_init_from_closure(move |slot: *mut Bio| {
                assert_eq!(slot.cast::<c::bio>(), bio, "BioInPlace: another bio's slot");
                Ok(())
            })
        }
    }
}

/// A bio in something shared by & - a read bio, which each fragment of its
/// read holds - lent &mut to one holder at a time: lend() hands it out, and
/// its Lent's drop gives it back. Lending twice at once is a bug, and
/// panics.
#[pin_data]
#[repr(C)]
pub struct BioLend {
    lent: AtomicBool,
    #[pin]
    bio:  UnsafeCell<Bio>,
}

// SAFETY: this is what makes a &BioLend's bio writable, so here's the whole
// of it.
//
// A &mut Bio is made only through a Lent, and only one Lent is out at a time:
// lend() takes it by an atomic swap of `lent`, Acquire, and the Lent's drop
// gives it back, Release - so one holder's writes happen before the next's,
// whichever CPU each runs on. It's atomic, not a RefCell, because the holders
// are tasks, which run on any CPU, and not one task: a read's walk lends its
// bio to each step and each page cache hook - to advance it, grow it by a
// folio, set its folios' state, trim it - and then, if the read isn't split,
// lends it to its one fragment, whose task holds it through the IO. The walk
// is done with the bio before that: a fragment that's lent the read's bio is
// the walk's last. A data update's read bio is lent by the update before its
// read - to set its position and priority - and then to the fragment reading
// into it. So the flag never actually sees two at once; it says so if that's
// ever broken.
//
// What's read through & while the bio's lent isn't ordered against the
// holder, so a & reader reads only what the holder doesn't change: the bvecs
// as mapped, and the data through a BioRange - which borrows BioData, a ZST,
// none of the bio's fields - or it's the holder itself, reading its own.
//
// The bio is where its bioset placed it, pinned, and the cell never moves.
unsafe impl Sync for BioLend {}

impl BioLend {
    pub fn new<E>(bio: BioInPlace) -> impl PinInit<Self, E> {
        let bio = unsafe { ::pin_init::cast_pin_init::<Bio, UnsafeCell<Bio>, E>(bio.init()) };
        ::pin_init::pin_init!(BioLend { lent: AtomicBool::new(false), bio <- bio }? E)
    }

    pub fn lend(&self) -> Lent<'_> {
        assert!(!self.lent.swap(true, Acquire), "a bio lent while it's lent already");
        Lent(self)
    }

    /// For C to make the bio in place: bio_init(), before anything's lent it.
    pub fn as_ptr(&self) -> *mut c::bio {
        self.bio.get().cast()
    }
}

unsafe impl BioField for BioLend {
    const AT: usize = core::mem::offset_of!(BioLend, bio);
}

/// A BioLend's bio, lent: given back as it's dropped.
#[must_use = "dropped, the bio's given back"]
pub struct Lent<'a>(&'a BioLend);

impl Lent<'_> {
    pub fn bio_mut(&mut self) -> Pin<&mut Bio> {
        unsafe { Pin::new_unchecked(&mut *self.0.bio.get()) }
    }

    /// For C to change it, through the lend.
    pub fn as_ptr(&mut self) -> *mut c::bio {
        self.0.as_ptr()
    }
}

impl Deref for Lent<'_> {
    type Target = Bio;

    fn deref(&self) -> &Bio {
        unsafe { &*self.0.bio.get() }
    }
}

impl Drop for Lent<'_> {
    fn drop(&mut self) {
        self.0.lent.store(false, Release);
    }
}

// ── BioSet, BioBox ───────────────────────────────────────────────────────

/// Rust's, made by new() and destroyed on drop - in a struct C may share, as
/// its struct bio_set - or C's, borrowed by from_c().
#[repr(transparent)]
pub struct BioSet<T: FrontPad> {
    set: Opaque<c::bio_set>,
    _t:  PhantomData<fn() -> T>,
}

/// What a bioset puts in front of its bios: a HasBio type's, or the larger of
/// two, Either - a bioset C allocates either container from.
pub trait FrontPad {
    const FRONT_PAD: usize;
}

impl<T: HasBio> FrontPad for T {
    const FRONT_PAD: usize = T::BIO_OFFSET;
}

pub struct Either<A, B>(PhantomData<(A, B)>);

impl<A: HasBio, B: HasBio> FrontPad for Either<A, B> {
    const FRONT_PAD: usize = if A::BIO_OFFSET > B::BIO_OFFSET { A::BIO_OFFSET } else { B::BIO_OFFSET };
}

impl<T: FrontPad> BioSet<T> {
    /// For C's bio_alloc_bioset() and bio_split(), allocating a T's bio.
    pub fn as_ptr(&self) -> *mut c::bio_set {
        self.set.as_ptr()
    }

    /// @pool_size bios held in reserve, and bvecs with them if @need_bvecs:
    /// for bios allocated with more than fit inline. Nothing to undo if it
    /// fails.
    pub fn new(pool_size: u32, need_bvecs: bool) -> impl PinInit<Self, BchError> {
        let flags = if need_bvecs { c::BIOSET_NEED_BVECS as i32 } else { 0 };
        // SAFETY: initialized in place, where it stays - pinned
        unsafe {
            ::pin_init::pin_init_from_closure(move |slot: *mut Self| {
                ret_to_result_void(c::bioset_init(slot.cast(), pool_size, T::FRONT_PAD as u32, flags))
            })
        }
    }
}

impl<T: HasBio> BioSet<T> {
    /// Panics if its front_pad isn't a T's.
    pub fn from_c(set: &c::bio_set) -> &Self {
        assert_eq!(set.front_pad as usize, T::BIO_OFFSET, "a bioset of another type's bios");
        // SAFETY: a transparent bio_set
        unsafe { &*core::ptr::from_ref(set).cast() }
    }

    /// A T with a bio for @bdev, @opf, of @nr_vecs bvecs - inline, or the
    /// set's - and its other fields from @init. GFP_KERNEL: NOIO or NOFS is
    /// the caller's memalloc scope's. Fails only if @init does, or past
    /// BIO_MAX_VECS or the set's bvec pool; otherwise it waits on the mempool.
    ///
    /// Which deadlocks, if what it waits for is a bio this task holds from
    /// the set, unsubmitted: one at a time, or a pool as big as how many are
    /// held at once.
    pub fn alloc<E, I>(&self, bdev: *mut c::block_device, nr_vecs: u16, opf: u32,
                       init: impl FnOnce(BioInPlace) -> I) -> Result<BioBox<T>, E>
    where
        I: PinInit<T, E>,
        E: From<BchError>,
    {
        self.alloc_with(bdev, opf, nr_vecs, init)
    }

    /// alloc(), with its bvecs filled from @src: B's, released as the box is
    /// dropped.
    pub fn alloc_with<B: Buffers, E, I>(&self, bdev: *mut c::block_device, opf: u32, src: B::Src<'_>,
                                        init: impl FnOnce(BioInPlace) -> I) -> Result<BioBox<T, B>, E>
    where
        I: PinInit<T, E>,
        E: From<BchError>,
    {
        // SAFETY: a live bioset
        let gfp = current_gfp_context_raw(c::GFP_KERNEL);
        let bio = unsafe { c::bio_alloc_bioset(bdev, B::nr_vecs(&src) as _, opf as _, gfp, self.set.as_ptr()) };
        let mut b = Self::init_new(bio, init)?;
        let buffers = B::fill(b.bio_mut(), src)?;
        let t = core::mem::ManuallyDrop::new(b).t;
        Ok(BioBox { t, buffers })
    }

    fn init_new<E, I>(bio: *mut c::bio, init: impl FnOnce(BioInPlace) -> I) -> Result<BioBox<T>, E>
    where
        I: PinInit<T, E>,
        E: From<BchError>,
    {
        if bio.is_null() {
            return Err(BchError::from(c::ENOMEM).into());
        }
        // SAFETY: front_pad bytes before it, a T's - by from_c()'s check
        let t = unsafe { of_bio::<T>(bio) };
        // SAFETY: the T, its bio there, the rest for @init
        match unsafe { init_at(init(BioInPlace(bio)), t) } {
            // SAFETY: a bio's T
            Ok(()) => Ok(BioBox { t: unsafe { NonNull::new_unchecked(t) }, buffers: BioBuffersUnmanaged }),
            Err(e) => {
                // SAFETY: the bio just allocated, its one ref
                unsafe { c::bio_put(bio) };
                Err(e)
            }
        }
    }
}

unsafe impl<T: FrontPad> ::pin_init::Zeroable for BioSet<T> {}

unsafe impl<T: FrontPad> crate::util::late::LateInit for BioSet<T> {
    /// bioset_initialized()
    #[cfg(kernel)]
    fn is_made(&self) -> bool {
        unsafe { !(*self.set.as_ptr()).bio_slab.is_null() }
    }

    /// Userspace's has no slab: its bio pool's
    #[cfg(not(kernel))]
    fn is_made(&self) -> bool {
        unsafe { !(*self.set.as_ptr()).bio_pool.elements.is_null() }
    }
}

impl<T: FrontPad> Drop for BioSet<T> {
    /// Its bios are all back: its owner's teardown waits for them.
    fn drop(&mut self) {
        // SAFETY: new()'s, never moved: a C bioset is only ever borrowed
        unsafe { c::bioset_exit(self.set.as_ptr()) }
    }
}

pub struct BioBox<T: HasBio, B: Buffers = BioBuffersUnmanaged> {
    t:       NonNull<T>,
    buffers: B,
}

// SAFETY: a T, as a Box's: what has the box has the T
unsafe impl<T: HasBio + Send, B: Buffers + Send> Send for BioBox<T, B> {}
unsafe impl<T: HasBio + Sync, B: Buffers + Sync> Sync for BioBox<T, B> {}

/// A bio submitted to a Gather: what its T makes of its IO's status, to put
/// to the gather. It runs where the IO completes - softirq, or an interrupt -
/// so it may not sleep, nor may T's Drop: what would goes to a workqueue.
pub trait Gathered<F: Fold>: HasBio {
    fn done(self: Pin<&mut Self>, status: Result<(), BchError>) -> F::Item;
}

// B is Default: it has nothing the bio doesn't, so the box is its bio
impl<T: HasBio, B: Buffers + Default> BioBox<T, B> {
    /// Submitted, given up to the IO with its ref to @r's Gather: once it's
    /// done, what its T makes of the IO's status (Gathered) is put - after
    /// the box is dropped, so its bio's freed before the gather's waiter can
    /// wake. 'static: the IO has it for as long as it takes.
    pub fn submit_gathered<F: Fold, Wt: Waiter, W: Width>(self, r: GatherRef<F, Wt, W>)
    where
        T: Gathered<F>,
        Self: Send + 'static,
    {
        let bio = bio_of(self.t.as_ptr());
        core::mem::forget(self);
        unsafe {
            (*bio).bi_private = r.into_raw().cast_mut().cast();
            (*bio).bi_end_io = Some(gathered_endio::<T, B, F, Wt, W>);
            c::submit_bio(bio);
        }
    }
}

unsafe extern "C" fn gathered_endio<T, B, F, Wt, W>(bio: *mut c::bio)
where
    T: Gathered<F>,
    B: Buffers + Default,
    F: Fold,
    Wt: Waiter,
    W: Width,
{
    let (r, mut b) = unsafe {
        (GatherRef::<F, Wt, W>::from_raw((*bio).bi_private.cast()),
         BioBox::<T, B> { t: NonNull::new_unchecked(of_bio::<T>(bio)), buffers: B::default() })
    };
    let status = b.bio().status();
    let v = b.as_mut().done(status);
    drop(b);
    r.put(v);
}

impl<T: HasBio, B: Buffers> Deref for BioBox<T, B> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: the T, live while it's had
        unsafe { self.t.as_ref() }
    }
}

impl<T: HasBio, B: Buffers> BioBox<T, B> {
    /// The T, for an owner that keeps the box and reaches it by pointer.
    pub fn as_ptr(&self) -> *mut T {
        self.t.as_ptr()
    }

    pub fn as_mut(&mut self) -> Pin<&mut T> {
        // SAFETY: the T, live while it's had, and the box's alone - a bio in
        // flight has given up its box; never moved from where its set made it
        unsafe { Pin::new_unchecked(self.t.as_mut()) }
    }
}

impl<T: HasBio> BioBox<T> {
    /// As BioSet::alloc(), the bvecs inline. kzalloc(), not kvzalloc(): an
    /// endio may drop it, and kvfree() isn't safe in interrupt context.
    pub fn new<E, I>(bdev: *mut c::block_device, nr_vecs: u16, opf: u32,
                     init: impl FnOnce(BioInPlace) -> I) -> Result<Self, E>
    where
        I: PinInit<T, E>,
        E: From<BchError>,
    {
        let (layout, vecs) = inline_layout::<T>(nr_vecs);
        let p = kzalloc(layout).map_err(|_| BchError::from(c::ENOMEM))?;
        let t = p.as_ptr().cast::<T>();

        unsafe {
            match init_at(inline_bio(bdev, p.add(vecs), nr_vecs, opf, init), t) {
                Ok(()) => Ok(BioBox { t: p.cast(), buffers: BioBuffersUnmanaged }),
                Err(e) => {
                    kfree(p, layout);
                    Err(e)
                }
            }
        }
    }
}

/// The room for @nr_vecs bvecs inline: a bio's, after what it's in.
pub fn inline_vecs(nr_vecs: u16) -> Layout {
    Layout::array::<c::bio_vec>(nr_vecs.into()).expect("a u16's worth of bvecs")
}

/// A T, its bio's bvecs the @nr_vecs at @vecs - inline, after what it's in,
/// inline_vecs()'s room: its bio bio_init()'d, then the rest made by @init.
/// Its drop is to uninit() the bio, as a BioBox's is.
///
/// # Safety
/// @vecs is room for @nr_vecs bvecs, that lasts as long as the T.
pub unsafe fn inline_bio<T: HasBio, E, I>(bdev: *mut c::block_device, vecs: NonNull<u8>, nr_vecs: u16, opf: u32,
                                          init: impl FnOnce(BioInPlace) -> I) -> impl PinInit<T, E>
where
    I: PinInit<T, E>,
{
    unsafe {
        ::pin_init::pin_init_from_closure(move |t: *mut T| {
            let bio = bio_of(t);
            c::bio_init(bio, bdev, vecs.as_ptr().cast(), nr_vecs, opf as _);
            init_at(init(BioInPlace(bio)), t).inspect_err(|_| c::bio_uninit(bio))
        })
    }
}

impl<'q, T, M, A> TaskSlot<'q, T, M, A, Exclusive>
where
    T: HasBio + Send + 'static,
    A: Alloc + Copy,
    M: FnOnce<(Pin<&'static mut T>,)>,
    <M as FnOnce<(Pin<&'static mut T>,)>>::Output: Future<Output = ()> + Send,
{
    /// An Exclusive task's TaskSlot from @alloc, its home a T whose bio's
    /// @nr_vecs bvecs are inline, after the box: inline_bio()'s.
    pub fn exclusive_bio_in<E, I>(alloc: A, bdev: *mut c::block_device, nr_vecs: u16, opf: u32,
                                  init: impl FnOnce(BioInPlace) -> I) -> Result<Self, BchError>
    where
        I: PinInit<T, E>,
        BchError: From<E>,
    {
        Self::exclusive_tail_in(alloc, inline_vecs(nr_vecs), |vecs| unsafe { inline_bio(bdev, vecs, nr_vecs, opf, init) })
    }
}

impl<T: HasBio, B: Buffers> BioBox<T, B> {
    /// As BioSet::alloc_with(), the bvecs inline.
    pub fn new_with<E, I>(bdev: *mut c::block_device, opf: u32, src: B::Src<'_>,
                          init: impl FnOnce(BioInPlace) -> I) -> Result<Self, E>
    where
        I: PinInit<T, E>,
        E: From<BchError>,
    {
        let mut b = BioBox::<T>::new(bdev, B::nr_vecs(&src), opf, init)?;
        let buffers = B::fill(b.bio_mut(), src)?;
        let t = core::mem::ManuallyDrop::new(b).t;
        Ok(BioBox { t, buffers })
    }
}

impl<T: HasBio, B: MapBuf> BioBox<T, BioBuffersMapped<B>> {
    pub fn buf(&self) -> &B::Target {
        &self.buffers.0
    }

    pub fn buf_mut(&mut self) -> &mut B::Target
    where
        B: DerefMut,
    {
        &mut self.buffers.0
    }
}

fn inline_layout<T>(nr_vecs: u16) -> (Layout, usize) {
    let (layout, vecs) = Layout::new::<T>().extend(inline_vecs(nr_vecs)).expect("a T and its bvecs");
    (layout.pad_to_align(), vecs)
}

// An Option of one is None as its pointer is null, all zeroes; Unmanaged
// buffers are nothing
unsafe impl<T: HasBio> ::pin_init::ZeroableOption for BioBox<T> {}

// SAFETY: the box's bio, its only ref
unsafe impl<T: HasBio, B: Buffers> BioOwner for BioBox<T, B> {
    type Buffers = B;

    fn bio(&self) -> &Bio {
        HasBio::bio(&**self)
    }

    fn bio_mut(&mut self) -> Pin<&mut Bio> {
        HasBio::bio_mut(self.as_mut())
    }
}

impl<T: HasBio, B: Buffers> Drop for BioBox<T, B> {
    /// A bioset's bio is put to its set - bio_put() - and one with its bvecs
    /// inline, which has no set, freed as new() laid it out: its bvecs'
    /// count is bi_max_vecs, which bio_init() set and bio_reset() keeps.
    fn drop(&mut self) {
        let t = self.t.as_ptr();
        let bio = bio_of(t);
        self.buffers.release(unsafe { Bio::from_raw_mut(bio) });
        unsafe {
            let (pool, nr_vecs) = ((*bio).bi_pool, (*bio).bi_max_vecs);
            core::ptr::drop_in_place(t);
            if !pool.is_null() {
                c::bio_put(bio);
            } else {
                c::bio_uninit(bio);
                kfree(self.t.cast(), inline_layout::<T>(nr_vecs).0);
            }
        }
    }
}
