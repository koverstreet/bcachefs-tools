// SPDX-License-Identifier: GPL-2.0

//! A bio's owners. Each owns a T with a bio in it - HasBio: the T is found
//! from its bio by its offset.
//!
//! - BioBox<T>: a T from a BioSet<T>, a bioset whose front_pad is the rest of
//!   the T - or, from BioBox::new_in(), a T from a pool, or kmalloc().
//!   Dropped, it's put to its set, or freed.
//! - TaskSlot::exclusive_bio_in(): a T that's a task's home.
//!
//! They deref to the T, and change it through Pin<&mut T>: a T may have
//! pinned fields of its own.
//!
//! A T is made around its bio: the bioset, or BioBox::new(), puts the bio
//! where it goes - bio_init() - then the caller's initializer does the rest of
//! the T - its bio field from BioInPlace::init(), or init_from(), which fills
//! its bvecs with what the bio's to own.
//!
//! A box submitted to a Gather (BioBox::submit_gathered()) is given up to its
//! IO, and its endio has it back from the bio, drops it, then puts what its T
//! makes of the IO's status (Gathered) to the gather.

use core::alloc::Layout;
use core::cell::UnsafeCell;
use core::future::Future;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicBool, Ordering::{Acquire, Release}};
use core::ops::Deref;
use core::pin::Pin;
use core::ptr::NonNull;

use pin_init::{pin_data, pin_init, PinInit};

use core::task::{ready, Context, Poll};

use super::{bio_of, of_bio, AtEnd, Bio, BioField, BioOwner, BioOwns, BioRange, BioSubmit, BioVecs, Data, HasBio,
            VecsAt};
use crate::util::gather::{Fold, GatherRef, Width};
use crate::util::task::{waiter, Completion, Exclusive, Resumable, TaskSlot, Waiter};
use crate::c;
use crate::data_structures::dst_vec::DstVec;
use crate::errcode::BchError;
use crate::locking::MemallocFlags;
use crate::os::blkdev::BlockDevice;
use crate::os::module::{Quiesced, ThisModule};
use crate::util::alloc::{Flags, init_at, stateless, Alloc, AllocError, Allocator, Kmalloc};
use crate::util::mempool::Mempool;

/// A bio bio_init() made, for the initializer of what it's in: 'a is as long
/// as that may borrow from what it's made of - a clone's source - and as long
/// as its box may live. Invariant, so an initializer can't shorten it.
pub struct BioInPlace<'a>(*mut c::bio, PhantomData<fn(&'a ()) -> &'a ()>);

impl BioInPlace<'static> {
    /// The bio at @bio, bio_init()'s, in a struct no bio owner made - a data
    /// update's.
    ///
    /// # Safety
    /// @bio is initialized, and is the bio field init() is given.
    pub unsafe fn at(bio: *mut c::bio) -> Self {
        BioInPlace(bio, PhantomData)
    }

    /// A Bio<B> in a struct no bio owner made - a data update's, the tools'
    /// write's - @init's, of what bio_init() makes: room for @nr_vecs bvecs of
    /// its own, past its inline few kmalloc()'s.
    pub fn own<B, E, I>(nr_vecs: u16, opf: u32, init: impl FnOnce(Self) -> I) -> impl PinInit<Bio<B>, E>
    where
        B: BioOwns,
        I: PinInit<Bio<B>, E>,
        E: From<BchError>,
    {
        bio_in(nr_vecs, opf, BvecAlloc::KMALLOC, init)
    }

    /// For what's built in it to borrow for 'a: a task's home, which its
    /// task holds.
    ///
    /// # Safety
    /// What it's built into doesn't outlive 'a.
    pub unsafe fn borrowing<'a>(self) -> BioInPlace<'a> {
        BioInPlace(self.0, PhantomData)
    }
}

impl<'a> BioInPlace<'a> {
    /// A Bio<B> of what bio_init() made: its bvecs empty, and B what owns
    /// none yet.
    pub fn init<E, B: BioOwns + Default>(self) -> impl PinInit<Bio<B>, E> {
        self.init_from(|_| Ok(B::default()))
    }

    /// A clone of @src's bio, at @src: its bvecs, and the data, the source's,
    /// where they are - borrowed for 'a, as long as its box lives.
    ///
    /// __bio_clone(), which isn't exported - but for its device's half, as a
    /// clone's made for no device and bio_set_dev() associates the blkg, and
    /// the crypt context and integrity payload a source has only once
    /// submitted.
    pub fn clone_of<E: 'a>(self, src: BioRange<'a>) -> impl PinInit<Bio, E> + 'a {
        self.init_from(move |b| {
            let owner = src.bio().vecs_owner();
            b.vecs_at = VecsAt::Borrowed(NonNull::from(owner));
            let table = owner.bvecs().as_ptr().cast_mut();
            let (b, s) = (&mut b.bio, &src.bio().bio);
            assert!(b.bi_bdev.is_null(), "a clone made for a device: it's its submit's");
            b.bi_flags |= 1 << c::BIO_CLONED;
            b.bi_ioprio = s.bi_ioprio;
            clone_hints(b, s);
            b.bi_iter = src.pos();
            bi_io_vec!(clone b, s);
            bi_io_vec!(b) = table;
            Ok(Data)
        })
    }

    /// A Bio<B> of what bio_init() made, and what it owns made by @owns from
    /// the bio: its bvecs filled - room for them made already - or, failing,
    /// nothing in them.
    pub fn init_from<E, B: BioOwns>(self, owns: impl FnOnce(&mut Bio) -> Result<B, E>) -> impl PinInit<Bio<B>, E> {
        let bio = self.0;
        // SAFETY: the bio's there - its slot, checked - but what it owns
        unsafe {
            ::pin_init::pin_init_from_closure(move |slot: *mut Bio<B>| {
                assert_eq!(slot.cast::<c::bio>(), bio, "BioInPlace: another bio's slot");
                let owns = owns(&mut *slot.cast::<Bio>())?;
                (&raw mut (*slot).owns).write(owns);
                Ok(())
            })
        }
    }
}

/// The rest of __bio_clone()'s copies, as each kernel's struct bio has them;
/// the shim's has none.
#[cfg(kernel)]
fn clone_hints(b: &mut c::bio, s: &c::bio) {
    b.bi_write_hint = s.bi_write_hint;
    #[cfg(linux_ge = "6.16")]
    { b.bi_write_stream = s.bi_write_stream; }
    #[cfg(linux_ge = "6.19")]
    { b.bi_bvec_gap_bit = s.bi_bvec_gap_bit; }
}

#[cfg(not(kernel))]
fn clone_hints(_: &mut c::bio, _: &c::bio) {}

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
// A &mut Bio is made through get_mut(), by an owner that has the BioLend
// alone, or through a Lent, and only one Lent is out at a time: lend() takes it
// by an atomic swap of `lent`, Acquire, and the Lent's drop gives it back,
// Release - so one holder's writes happen before the next's, whichever CPU
// each runs on. It's atomic, not a RefCell, because the holders are tasks,
// which run on any CPU: a read's bio is lent, if the read isn't split, to its
// one fragment, whose task holds it through the IO; a promote's, to the
// fragment reading into it. So the flag never actually sees two at once; it
// says so if that's ever broken.
//
// What's read through & while the bio's lent isn't ordered against the
// holder, so a & reader reads only what the holder doesn't change: the bvecs
// as mapped, and the data through a BioRange - which borrows BioData, a ZST,
// none of the bio's fields - or it's the holder itself, reading its own.
unsafe impl Sync for BioLend {}

impl BioLend {
    pub fn new<E>(bio: impl PinInit<Bio, E>) -> impl PinInit<Self, E> {
        let bio = unsafe { ::pin_init::cast_pin_init::<Bio, UnsafeCell<Bio>, E>(bio) };
        ::pin_init::pin_init!(BioLend { lent: AtomicBool::new(false), bio <- bio }? E)
    }

    pub fn lend(&self) -> Lent<'_> {
        assert!(!self.lent.swap(true, Acquire), "a bio lent while it's lent already");
        Lent(self)
    }

    pub fn get_mut(&mut self) -> &mut Bio {
        self.bio.get_mut()
    }

    /// For C to make the bio in place: bio_init(), before anything's lent it.
    pub fn as_ptr(&self) -> *mut c::bio {
        self.bio.get().cast()
    }
}

unsafe impl BioField for BioLend {
    type Owns = Data;
    const AT: usize = core::mem::offset_of!(BioLend, bio);
}

/// A BioLend's bio, lent: given back as it's dropped.
#[must_use = "dropped, the bio's given back"]
pub struct Lent<'a>(&'a BioLend);

impl Lent<'_> {
    pub fn bio_mut(&mut self) -> &mut Bio {
        unsafe { &mut *self.0.bio.get() }
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

/// A pool of bvecs, BIO_MAX_VECS each: a bioset's bvec_pool.
#[pin_data]
#[repr(transparent)]
#[derive(::pin_init::Zeroable)]
pub struct BvecMempool {
    #[pin]
    pool: Mempool<Kmalloc>,
}

impl BvecMempool {
    /// @min_nr held in reserve: as many as the bios it serves.
    pub fn new(min_nr: usize) -> impl PinInit<Self, BchError> {
        pin_init!(Self { pool <- Mempool::kmalloc(min_nr, Self::element_size()) }? BchError)
    }

    pub fn init_static(this: &'static Self, min_nr: usize, m: &ThisModule) -> Result<(), BchError> {
        Mempool::init_static(&this.pool, Mempool::kmalloc(min_nr, Self::element_size()), m)
    }

    fn element_size() -> usize {
        DstVec::<c::bio_vec, u16, BvecAlloc>::alloc_size(c::BIO_MAX_VECS as usize)
    }

    pub fn exit(&self, q: &Quiesced<'_>) {
        self.pool.exit(q)
    }

    /// For its bios to take bvecs from: a static's, which outlives them all.
    pub fn alloc(&'static self) -> BvecAlloc {
        BvecAlloc(Bvecs::Pool(NonNull::from(self)))
    }

    /// alloc(), of a pool that isn't a static's: a set's.
    ///
    /// # Safety
    /// The pool outlives every bio that takes bvecs from it.
    unsafe fn alloc_unbound(&self) -> BvecAlloc {
        BvecAlloc(Bvecs::Pool(NonNull::from(self)))
    }
}

/// Where a bio's bvecs past its inline few come from. By pointer, as bi_pool
/// is: what made the bio outlives it - so made only by a pool's alloc(), or
/// here.
#[derive(Clone, Copy)]
pub struct BvecAlloc(Bvecs);

#[derive(Clone, Copy)]
enum Bvecs {
    Pool(NonNull<BvecMempool>),
    Kmalloc,
    None,
}

impl BvecAlloc {
    pub(super) const KMALLOC: Self = BvecAlloc(Bvecs::Kmalloc);
    pub(super) const NONE: Self = BvecAlloc(Bvecs::None);
}

unsafe impl Allocator for BvecAlloc {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        match self.0 {
            Bvecs::Pool(p) => unsafe { p.as_ref() }.pool.allocate(layout),
            Bvecs::Kmalloc => Kmalloc.allocate(layout),
            Bvecs::None    => Err(AllocError),
        }
    }

    unsafe fn deallocate(&self, p: NonNull<u8>, layout: Layout) {
        match self.0 {
            Bvecs::Pool(pool) => unsafe { pool.as_ref().pool.deallocate(p, layout) },
            Bvecs::Kmalloc    => unsafe { Kmalloc.deallocate(p, layout) },
            Bvecs::None       => unreachable!("freeing bvecs a bio with no BvecAlloc never had"),
        }
    }
}

unsafe impl Alloc for BvecAlloc {}

/// Fails as @bvecs does, in the task's memalloc scope, or past BIO_MAX_VECS,
/// as bvec_alloc() does: so a buffer whose bvecs it was made with room for
/// fits.
///
/// # Safety
/// @bio is room for a Bio, not in flight.
unsafe fn bio_init(bio: *mut Bio, nr_vecs: u16, opf: u32, bvecs: BvecAlloc) -> Result<(), BchError> {
    let mut vecs = BioVecs::<Data>::new_in(bvecs);
    super::reserve_bvecs(&mut vecs, nr_vecs.into())?;
    unsafe {
        (&raw mut (*bio).vecs).write(vecs);
        (&raw mut (*bio).vecs_at).write(VecsAt::Own);
        let vecs = &mut *(&raw mut (*bio).vecs);
        let max = vecs.capacity().min(c::BIO_MAX_VECS as usize) as u16;
        crate::os::bio::sys::init(bio.cast(), core::ptr::null_mut(), vecs.as_mut_ptr(), max, opf);
    }
    Ok(())
}

// ── BioSet, BioBox ───────────────────────────────────────────────────────

/// A set's bios, untyped: each a bio with what it's in before it, in one
/// allocation from a pool, its bvecs past its inline few from the set's
/// BvecMempool. A bio's bi_pool is its BioPool, so it's put back knowing
/// nothing else of it.
#[pin_data]
pub struct BioPool {
    #[pin]
    pool:      Mempool<Kmalloc>,
    #[pin]
    bvecs:     BvecMempool,
    /* where in an element its bio is, and the element */
    front_pad: usize,
    layout:    Layout,
}

/// A bioset of T's bios: a BioPool, its elements T's.
#[pin_data]
#[repr(transparent)]
pub struct BioSet<T: HasBio> {
    #[pin]
    set: BioPool,
    _t:  PhantomData<fn() -> T>,
}

impl BioPool {
    fn new(pool_size: u32, front_pad: usize, layout: Layout) -> impl PinInit<Self, BchError> {
        pin_init!(Self {
            pool  <- Mempool::kmalloc(pool_size as usize, layout.size()),
            bvecs <- BvecMempool::new(pool_size as usize),
            front_pad,
            layout,
        }? BchError)
    }

    pub fn reserve_drawn(&self) -> bool {
        self.pool.reserve_drawn()
    }

    /// bio_alloc_bioset(): a bio with room for @nr_vecs bvecs. Null past
    /// BIO_MAX_VECS, or where the pool can't wait - a NORECLAIM scope;
    /// otherwise it waits on the pool.
    pub fn alloc_raw(&self, nr_vecs: u16, opf: u32, gfp: Flags) -> *mut c::bio {
        let layout = self.layout;
        let Ok(p) = self.pool.alloc_gfp(layout, gfp) else {
            return core::ptr::null_mut();
        };
        let p = p.cast::<u8>();
        unsafe {
            let bio = p.add(self.front_pad).cast::<Bio>().as_ptr();
            let made = {
                let _gfp = MemallocFlags::for_gfp(gfp);
                bio_init(bio, nr_vecs, opf, self.bvecs.alloc_unbound())
            };
            if made.is_err() {
                self.pool.deallocate(p, layout);
                return core::ptr::null_mut();
            }
            let bio = bio.cast::<c::bio>();
            (*bio).bi_pool = core::ptr::from_ref(self).cast_mut().cast();
            bio
        }
    }

    /// @bio's memory back to the pool, what's in it dropped already.
    ///
    /// # Safety
    /// @bio is this pool's, and nothing uses it after.
    unsafe fn free(&self, bio: *mut c::bio) {
        let p = unsafe { NonNull::new_unchecked(bio.byte_sub(self.front_pad)) };
        unsafe { self.pool.deallocate(p.cast(), self.layout) };
    }

    /*
     * Allocate a bio (possibly a chain of bios via chain()) covering
     * [base, base+size) on @bdev starting at sector @sector. Each bio in the
     * chain is capped at BIO_MAX_VECS bvecs; physically contiguous buffers fit
     * in one bvec and the chain typically has length 1.
     *
     * The returned bio is the tail of the chain (the last chunk in sector
     * order), left unsubmitted for the caller to configure (bi_end_io,
     * bi_private, any wrapper state) and submit. Earlier chunks are chained
     * to the bio that immediately follows them and submitted here in sector
     * order, so the block layer sees requests in forward order; callers
     * should wrap their submission in blk_start_plug()/blk_finish_plug() to
     * give the driver a chance to merge.
     *
     * When the caller's tail bio completes, the chain's bi_status reflects
     * any error propagated from the earlier chunks.
     *
     * REQ_PREFLUSH, if set in @opf, applies only to the first (sector-lowest)
     * bio in the chain.
     */
    /// # Safety
    /// [base, base+size) lives till the chain's IO is done.
    pub unsafe fn map_and_chain(&self, bdev: &BlockDevice, mut base: *mut u8, mut size: usize,
                                mut sector: u64, mut opf: u32, gfp: Flags) -> *mut c::bio {
        let mut tail: *mut c::bio = core::ptr::null_mut();

        while size != 0 {
            let chunk_size = super::buf_bio_bytes(base, size);
            let chunk_bvecs = super::buf_nr_bvecs(base, chunk_size);

            let bio = self.alloc_raw(chunk_bvecs as u16, opf, gfp);
            opf &= !c::REQ_PREFLUSH; /* only on the first bio */

            unsafe {
                let b = Bio::from_raw_mut(bio);
                b.map(base, chunk_size).expect("a chunk of buf_bio_bytes(), into a bio with room for its bvecs");
                b.aim(bdev, sector);

                if !tail.is_null() {
                    /*
                     * Chain the previous (earlier-sector) bio to this one
                     * and submit it now: submission order is sector-forward
                     * so the elevator can merge with adjacent writes.
                     */
                    chain(tail, bio);
                    crate::os::blkdev::submit_bio(tail);
                }
            }
            tail = bio;

            base       = base.wrapping_add(chunk_size);
            sector    += (chunk_size >> 9) as u64;
            size      -= chunk_size;
        }

        tail
    }
}

/// bio_chain(), for our bios: @bio's completion is one of @parent's - its
/// status passed on, then it's put back to its set.
///
/// # Safety
/// Both a BioPool's, @bio not yet submitted.
unsafe fn chain(bio: *mut c::bio, parent: *mut c::bio) {
    unsafe {
        (*bio).bi_private = parent.cast();
        (*bio).bi_end_io  = Some(chain_endio);
        crate::os::bio::sys::inc_remaining(parent);
    }
}

unsafe extern "C" fn chain_endio(bio: *mut c::bio) {
    unsafe {
        let parent = (*bio).bi_private.cast::<c::bio>();
        if (*bio).bi_status != 0 && (*parent).bi_status == 0 {
            (*parent).bi_status = (*bio).bi_status;
        }
        bio_put(bio);
        crate::os::bio::sys::endio(parent);
    }
}

/// # Safety
/// @bio is a bio, and its pool outlives 'a.
unsafe fn pool_of<'a>(bio: *const c::bio) -> &'a BioPool {
    unsafe { (*bio).bi_pool.cast::<BioPool>().as_ref() }.expect("a set's bio, of no BioSet")
}

/// bio_put(): a set's bio - its bvecs with it - back to its pool. What it's
/// in isn't dropped, as C's isn't.
///
/// # Safety
/// @bio is a BioPool's, and nothing uses it after.
pub unsafe fn bio_put(bio: *mut c::bio) {
    unsafe {
        let set = pool_of(bio);
        core::ptr::drop_in_place(bio.cast::<Bio>());
        set.free(bio);
    }
}

impl<T: HasBio> Deref for BioSet<T> {
    type Target = BioPool;

    fn deref(&self) -> &BioPool {
        &self.set
    }
}

impl<T: HasBio> BioSet<T> {
    /// @pool_size bios, with BIO_MAX_VECS bvecs each, held in reserve.
    /// Nothing to undo if it fails.
    pub fn new(pool_size: u32) -> impl PinInit<Self, BchError> {
        pin_init!(Self {
            set <- BioPool::new(pool_size, T::BIO_OFFSET, Layout::new::<T>()),
            _t: PhantomData,
        }? BchError)
    }

    /// A T with a bio for @opf, of @nr_vecs bvecs - inline, or the set's - and
    /// its other fields from @init. GFP_KERNEL: NOIO or NOFS is the caller's
    /// memalloc scope's. Fails only if @init does, or past BIO_MAX_VECS or the
    /// set's bvec pool; otherwise it waits on the mempool.
    ///
    /// Which deadlocks, if what it waits for is a bio this task holds from
    /// the set, unsubmitted: one at a time, or a pool as big as how many are
    /// held at once.
    pub fn alloc<'a, E, I>(&self, nr_vecs: u16, opf: u32, init: impl FnOnce(BioInPlace<'a>) -> I)
        -> Result<BioBox<'a, T>, E>
    where
        I: PinInit<T, E>,
        E: From<BchError>,
    {
        let bio = self.alloc_raw(nr_vecs, opf, Flags::empty());
        if bio.is_null() {
            return Err(BchError::from(c::ENOMEM).into());
        }
        // SAFETY: front_pad bytes before it, a T's
        let t = unsafe { of_bio::<T>(bio) };
        // SAFETY: the T, its bio there, the rest for @init
        match unsafe { init_at(init(BioInPlace(bio, PhantomData)), t) } {
            // SAFETY: a bio's T
            Ok(()) => Ok(BioBox {
                t:     unsafe { NonNull::new_unchecked(t) },
                alloc: Kmalloc,
                _init: PhantomData,
            }),
            Err(e) => {
                unsafe { bio_put(bio) };
                Err(e)
            }
        }
    }
}

unsafe impl<T: HasBio> ::pin_init::Zeroable for BioSet<T> {}

impl<T: HasBio> crate::util::late::LateInit for BioSet<T> {
    fn is_made(&self) -> bool {
        self.set.pool.is_made()
    }
}

impl<T: HasBio> crate::util::late::LateStop for BioSet<T> {}

/// @A: what a box with its bvecs inline is freed to; a bioset's is put to its
/// set. 'a: as long as what it was made of may be borrowed - a clone's
/// source - for most, 'static.
pub struct BioBox<'a, T: HasBio, A: Alloc = Kmalloc> {
    t:     NonNull<T>,
    alloc: A,
    _init: PhantomData<&'a ()>,
}

// SAFETY: a T, as a Box's: what has the box has the T
unsafe impl<T: HasBio + Send, A: Alloc + Send> Send for BioBox<'_, T, A> {}
unsafe impl<T: HasBio + Sync, A: Alloc + Sync> Sync for BioBox<'_, T, A> {}

/// A bio submitted to a Gather: what its T makes of its IO's status, to put
/// to the gather. It runs where the IO completes - softirq, or an interrupt -
/// so it may not sleep, nor may T's Drop: what would goes to a workqueue.
pub trait Gathered<F: Fold>: HasBio {
    fn done(self: Pin<&mut Self>, status: Result<(), BchError>) -> F::Item;
}

/// A T that keeps its bio's device open through the IO by the ref it's given
/// at submit: for a bio given up to its IO, which a borrow wouldn't outlast.
pub trait HoldsDev: HasBio {
    type Dev;

    fn hold(self: Pin<&mut Self>, dev: Self::Dev);

    /// The device of the ref it holds: hold()'s, first.
    fn bdev(&self) -> &BlockDevice;
}

/// @t's bio aimed at @dev's device, @dev held by @t: the device read from
/// where it's held, which stays put till the IO's done.
fn hold_dev<T: HoldsDev>(mut t: Pin<&mut T>, dev: T::Dev, sector: u64) -> *mut c::bio {
    t.as_mut().hold(dev);
    let bdev = core::ptr::from_ref(t.bdev());
    t.bio_mut().aim(bdev, sector)
}

// A has no state, so the endio has the box back from the bio alone
impl<T: HasBio, A: Alloc> BioBox<'_, T, A> {
    /// Submitted at @sector of @dev's device, which it holds, given up to the
    /// IO with its ref to @r's Gather: once it's done, what its T makes of
    /// the IO's status (Gathered) is put - after the box is dropped, so its
    /// bio's freed before the gather's waiter can wake. 'static: the IO has
    /// it for as long as it takes.
    pub fn submit_gathered<F: Fold, Wt: Waiter, W: Width>(self, dev: T::Dev, sector: u64, r: GatherRef<F, Wt, W>)
    where
        T: Gathered<F> + HoldsDev,
        Self: Send + 'static,
    {
        unsafe { self.submit_gathered_unchecked(dev, sector, r) }
    }

    /// submit_gathered(), of a box that borrows: a clone of another's bvecs.
    ///
    /// # Safety
    /// What the box borrows outlives @r's Gather's wait.
    pub unsafe fn submit_gathered_unchecked<F: Fold, Wt: Waiter, W: Width>(self, dev: T::Dev, sector: u64,
                                                                           r: GatherRef<F, Wt, W>)
    where
        T: Gathered<F> + HoldsDev,
    {
        let bio = hold_dev(core::mem::ManuallyDrop::new(self).as_mut(), dev, sector);
        unsafe {
            (*bio).bi_private = r.into_raw().cast_mut().cast();
            (*bio).bi_end_io = Some(gathered_endio::<T, A, F, Wt, W>);
            crate::os::blkdev::submit_bio(bio);
        }
    }
}

unsafe extern "C" fn gathered_endio<T, A, F, Wt, W>(bio: *mut c::bio)
where
    T: Gathered<F>,
    A: Alloc,
    F: Fold,
    Wt: Waiter,
    W: Width,
{
    let (r, mut b) = unsafe {
        (GatherRef::<F, Wt, W>::from_raw((*bio).bi_private.cast()),
         BioBox::<T, A> { t: NonNull::new_unchecked(of_bio::<T>(bio)), alloc: stateless(), _init: PhantomData })
    };
    let status = b.bio().status();
    let v = b.as_mut().done(status);
    drop(b);
    r.put(v);
}

/// @t's bio submitted to @r's Gather, as a box's is by submit_gathered(), but
/// not given up: what its T makes of the IO's status is put, and @t is the
/// caller's again once the gather's waited on.
///
/// # Safety
/// @t isn't moved, used or dropped till @r's Gather's wait has returned.
pub unsafe fn submit_gathered_borrowed<T, F, Wt, W>(t: Pin<&mut T>, dev: T::Dev, sector: u64,
                                                    r: GatherRef<F, Wt, W>)
where
    T: Gathered<F> + HoldsDev,
    F: Fold,
    Wt: Waiter,
    W: Width,
{
    let bio = hold_dev(t, dev, sector);
    unsafe {
        (*bio).bi_private = r.into_raw().cast_mut().cast();
        (*bio).bi_end_io = Some(borrowed_endio::<T, F, Wt, W>);
        crate::os::blkdev::submit_bio(bio);
    }
}

unsafe extern "C" fn borrowed_endio<T, F, Wt, W>(bio: *mut c::bio)
where
    T: Gathered<F>,
    F: Fold,
    Wt: Waiter,
    W: Width,
{
    let r = unsafe { GatherRef::<F, Wt, W>::from_raw((*bio).bi_private.cast()) };
    let t = unsafe { Pin::new_unchecked(&mut *of_bio::<T>(bio)) };
    let status = t.bio().status();
    r.put(t.done(status));
}

/// A box submitted: given up to its IO till it's done, and then, awaited,
/// given back - its bio's status its own. Dropped before that, it asserts.
#[must_use = "a bio's IO is waited on, or awaited, to its end"]
pub struct Submitted<'a, T: AtEnd, A: Alloc, W: Waiter> {
    /* first: its Done is the box's bi_private, so it's done before the box goes */
    c: Completion<'static, W>,
    b: Option<BioBox<'a, T, A>>,
}

// moving it moves neither the box's allocation nor the Done in it
impl<T: AtEnd, A: Alloc, W: Waiter> Unpin for Submitted<'_, T, A, W> {}

impl<'a, T: AtEnd + HoldsDev, A: Alloc> BioBox<'a, T, A> {
    /// Submitted at @sector of @dev's device, which it holds, its T's
    /// at_end() run in the endio: Submitted's till it's awaited.
    pub fn submit_owned<W: Waiter>(mut self, dev: T::Dev, sector: u64, waiter: W) -> Submitted<'a, T, A, W> {
        let bio = hold_dev(self.as_mut(), dev, sector);
        let c = unsafe {
            let c = Completion::init_at((&raw mut (*bio).bi_private).cast(), waiter);
            (*bio).bi_end_io = Some(super::at_end_endio::<T, W>);
            crate::os::blkdev::submit_bio(bio);
            c
        };
        Submitted { c, b: Some(self) }
    }
}

impl<'a, T: AtEnd, A: Alloc> Resumable for Submitted<'a, T, A, waiter::Async> {
    type Output = BioBox<'a, T, A>;

    fn poll_resume(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        ready!(Pin::new(&mut this.c).poll_resume(cx));
        Poll::Ready(this.b.take().expect("a submitted bio, given back twice"))
    }
}

impl<T: HasBio, A: Alloc> Deref for BioBox<'_, T, A> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: the T, live while it's had
        unsafe { self.t.as_ref() }
    }
}

impl<T: HasBio, A: Alloc> BioBox<'_, T, A> {
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

impl<'a, T: HasBio> BioBox<'a, T> {
    /// As BioSet::alloc(), from kmalloc() - its bvecs past its inline few
    /// too. kzalloc(), not kvzalloc(): an endio may drop it, and kvfree()
    /// isn't safe in interrupt context.
    pub fn new<E, I>(nr_vecs: u16, opf: u32, init: impl FnOnce(BioInPlace<'a>) -> I) -> Result<Self, E>
    where
        I: PinInit<T, E>,
        E: From<BchError>,
    {
        Self::new_in(Kmalloc, BvecAlloc::KMALLOC, nr_vecs, opf, init)
    }
}

impl<'a, T: HasBio, A: Alloc> BioBox<'a, T, A> {
    /// new(), from @alloc - a pool, whose elements are its reserve - and its
    /// bvecs past its inline few from @bvecs.
    pub fn new_in<E, I>(alloc: A, bvecs: BvecAlloc, nr_vecs: u16, opf: u32,
                        init: impl FnOnce(BioInPlace<'a>) -> I) -> Result<Self, E>
    where
        I: PinInit<T, E>,
        E: From<BchError>,
    {
        let layout = Layout::new::<T>();
        let p = alloc.allocate_zeroed(layout).map_err(|_| BchError::from(c::ENOMEM))?.cast::<T>();
        unsafe {
            match init_at(bio_in(nr_vecs, opf, bvecs, init), p.as_ptr()) {
                Ok(()) => Ok(BioBox { t: p, alloc, _init: PhantomData }),
                Err(e) => {
                    alloc.deallocate(p.cast(), layout);
                    Err(e)
                }
            }
        }
    }
}

fn bio_in<'a, T: HasBio, E, I>(nr_vecs: u16, opf: u32, bvecs: BvecAlloc, init: impl FnOnce(BioInPlace<'a>) -> I)
    -> impl PinInit<T, E>
where
    I: PinInit<T, E>,
    E: From<BchError>,
{
    unsafe {
        ::pin_init::pin_init_from_closure(move |t: *mut T| {
            let bio = bio_of(t);
            bio_init(bio.cast(), nr_vecs, opf, bvecs)?;
            init_at(init(BioInPlace(bio, PhantomData)), t).inspect_err(|_| core::ptr::drop_in_place(bio.cast::<Bio>()))
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
    /// An Exclusive task's TaskSlot from @alloc, its home a T made around its
    /// bio, as BioBox::new_in()'s is.
    pub fn exclusive_bio_in<E, I>(alloc: A, bvecs: &'static BvecMempool, nr_vecs: u16, opf: u32,
                                  init: impl FnOnce(BioInPlace<'static>) -> I) -> Result<Self, BchError>
    where
        I: PinInit<T, E>,
        E: From<BchError>,
        BchError: From<E>,
    {
        Self::exclusive_in(alloc, bio_in(nr_vecs, opf, bvecs.alloc(), init))
    }
}

// An Option of one is None as its pointer is null, all zeroes
unsafe impl<T: HasBio> ::pin_init::ZeroableOption for BioBox<'_, T> {}

impl<T: HasBio, A: Alloc> BioSubmit for BioBox<'_, T, A> {
    fn bio(&self) -> &Bio {
        HasBio::bio(&**self)
    }

    fn bio_mut(&mut self) -> &mut Bio {
        HasBio::bio_mut(self.as_mut())
    }
}

// SAFETY: has_bio!() checks T's Owns is its bio's
unsafe impl<T: HasBio, A: Alloc> BioOwner for BioBox<'_, T, A> {
    type Owns = T::Owns;
}

impl<T: HasBio, A: Alloc> Drop for BioBox<'_, T, A> {
    fn drop(&mut self) {
        let t = self.t.as_ptr();
        let bio = bio_of(t);
        unsafe {
            let set = (*bio).bi_pool.cast::<BioPool>().as_ref();
            core::ptr::drop_in_place(t);
            match set {
                Some(set) => set.free(bio),
                None      => self.alloc.deallocate(self.t.cast(), Layout::new::<T>()),
            }
        }
    }
}
