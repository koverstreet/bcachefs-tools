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

use pin_init::{pin_data, pin_init, PinInit};

use super::{bio_of, of_bio, Bio, BioBuffersMapped, BioBuffersUnmanaged, BioField, BioOwner, Buffers, HasBio,
            MapBuf};
use crate::util::gather::{Fold, GatherRef, Width};
use crate::util::task::{Exclusive, TaskSlot, Waiter};
use crate::c;
use crate::errcode::BchError;
use crate::os::kernel::PAGE_SIZE;
use crate::util::alloc::{init_at, kfree, kzalloc, Alloc, Kmalloc};
use crate::util::mempool::Mempool;

pub struct BioInPlace(*mut c::bio);

impl BioInPlace {
    /// The bio at @bio, bio_init()'s, in a struct no bio owner made - a data
    /// update's.
    ///
    /// # Safety
    /// @bio is initialized, and is the bio field init() is given.
    pub unsafe fn at(bio: *mut c::bio) -> Self {
        BioInPlace(bio)
    }

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

/// A set's bios, untyped: each a bio with what it's in before it and its
/// bvecs inline after, in one allocation from a pool of elements sized for
/// BIO_MAX_VECS - where it's less, the allocator's first, as any pool's
/// allocations. A bio's bi_pool is its BioPool, so it's put back knowing
/// nothing else of it.
#[pin_data]
pub struct BioPool {
    #[pin]
    pool:      Mempool<Kmalloc>,
    front_pad: usize,
}

/// A bioset of T's bios: a BioPool, its front_pad a T's.
#[pin_data]
#[repr(transparent)]
pub struct BioSet<T: FrontPad> {
    #[pin]
    set: BioPool,
    _t:  PhantomData<fn() -> T>,
}

/// A bio with @front_pad bytes before it and @nr_vecs bvecs after: its
/// allocation, and where the bvecs start.
fn bio_layout(front_pad: usize, nr_vecs: u16) -> (Layout, usize) {
    let bio = Layout::from_size_align(front_pad + size_of::<c::bio>(), align_of::<c::bio>())
        .expect("a bio and what it's in");
    let (layout, vecs) = bio.extend(inline_vecs(nr_vecs)).expect("a bio and its bvecs");
    (layout.pad_to_align(), vecs)
}

impl BioPool {
    fn new(pool_size: u32, front_pad: usize) -> impl PinInit<Self, BchError> {
        pin_init!(Self {
            pool <- Mempool::kmalloc(pool_size as usize, bio_layout(front_pad, c::BIO_MAX_VECS as u16).0.size()),
            front_pad,
        }? BchError)
    }

    /// bio_alloc_bioset(): a bio with room for @nr_vecs bvecs. Null past
    /// BIO_MAX_VECS, or where the pool can't wait - a NORECLAIM scope;
    /// otherwise it waits on the pool.
    pub fn alloc_raw(&self, bdev: *mut crate::os::blkdev::BlockDevice, nr_vecs: u16, opf: u32, gfp: c::gfp_t) -> *mut c::bio {
        let (layout, vecs) = bio_layout(self.front_pad, nr_vecs);
        let Ok(p) = self.pool.alloc_gfp(layout, gfp) else {
            return core::ptr::null_mut();
        };
        /*
         * Given a whole element, as bvec_alloc() falls back to its mempool's,
         * it has room for BIO_MAX_VECS: bio_put() frees what's recorded, so
         * the element goes back to the reserve.
         */
        let nr_vecs = if p.len() > layout.size() { c::BIO_MAX_VECS as u16 } else { nr_vecs };
        let p = p.cast::<u8>();
        unsafe {
            let bio = p.add(self.front_pad).cast::<c::bio>().as_ptr();
            crate::os::bio::init(bio, bdev, p.add(vecs).cast().as_ptr(), nr_vecs, opf);
            (*bio).bi_pool = core::ptr::from_ref(self).cast_mut().cast();
            bio
        }
    }

    /// bio_alloc_clone(): a bio of @src's bvecs from where it is, none of its
    /// own - for no device, bio_set_dev()'s after.
    ///
    /// # Safety
    /// @src is live, and its bvecs outlive the clone.
    pub unsafe fn alloc_clone(&self, src: *mut c::bio, gfp: c::gfp_t) -> *mut c::bio {
        let bio = self.alloc_raw(core::ptr::null_mut(), 0, unsafe { (*src).bi_opf }, gfp);
        if !bio.is_null() {
            unsafe { super::buffers::clone_from(&mut *bio, &*src, (*src).bi_iter) };
        }
        bio
    }

    /// bio_split(): @bio's first @sectors, a clone, and @bio advanced past
    /// them.
    ///
    /// # Safety
    /// As alloc_clone()'s.
    pub unsafe fn split(&self, bio: *mut c::bio, sectors: u32, gfp: c::gfp_t) -> *mut c::bio {
        let size = unsafe { (*bio).bi_iter.bi_size };
        assert!(sectors != 0 && sectors < size >> 9, "splitting {sectors} sectors off a {size} byte bio");

        let split = unsafe { self.alloc_clone(bio, gfp) };
        if !split.is_null() {
            unsafe {
                (*split).bi_iter.bi_size = sectors << 9;
                Bio::from_raw_mut(bio).advance(sectors << 9);
            }
        }
        split
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
    pub unsafe fn map_and_chain(&self, bdev: *mut crate::os::blkdev::BlockDevice, mut base: *mut u8, mut size: usize,
                                mut sector: u64, mut opf: u32, gfp: c::gfp_t) -> *mut c::bio {
        let mut tail: *mut c::bio = core::ptr::null_mut();
        let mut nr_bvecs = super::buf_nr_bvecs(base, size);
        let max_vecs = c::BIO_MAX_VECS as usize;

        while size != 0 {
            let (mut chunk_size, mut chunk_bvecs) = (size, nr_bvecs);

            if chunk_bvecs > max_vecs {
                let offset = base.addr() & (PAGE_SIZE - 1);

                chunk_size = max_vecs * PAGE_SIZE - offset;
                chunk_bvecs = max_vecs;
                assert!(chunk_size <= size);
            }

            let bio = self.alloc_raw(bdev, chunk_bvecs as u16, opf, gfp);
            opf &= !c::REQ_PREFLUSH; /* only on the first bio */

            unsafe {
                super::bch2_bio_map(bio, base.cast(), chunk_size);
                (*bio).bi_iter.bi_sector = sector;

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
            nr_bvecs  -= chunk_bvecs;
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
        crate::os::bio::inc_remaining(parent);
    }
}

unsafe extern "C" fn chain_endio(bio: *mut c::bio) {
    unsafe {
        let parent = (*bio).bi_private.cast::<c::bio>();
        if (*bio).bi_status != 0 && (*parent).bi_status == 0 {
            (*parent).bi_status = (*bio).bi_status;
        }
        bio_put(bio);
        crate::os::bio::endio(parent);
    }
}

/// bio_put(): a set's bio back to its pool. What it's in isn't dropped, as
/// C's isn't.
///
/// # Safety
/// @bio is a BioPool's, and nothing uses it after.
pub unsafe fn bio_put(bio: *mut c::bio) {
    unsafe {
        let set = (*bio).bi_pool.cast::<BioPool>().as_ref().expect("bio_put() of a bio no BioSet made");
        let (layout, _) = bio_layout(set.front_pad, (*bio).bi_max_vecs);
        crate::os::bio::uninit(bio);
        set.pool.free(NonNull::new_unchecked(bio.byte_sub(set.front_pad)).cast(), layout);
    }
}

impl<T: FrontPad> Deref for BioSet<T> {
    type Target = BioPool;

    fn deref(&self) -> &BioPool {
        &self.set
    }
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
    /// @pool_size bios, with BIO_MAX_VECS bvecs each, held in reserve.
    /// Nothing to undo if it fails.
    pub fn new(pool_size: u32) -> impl PinInit<Self, BchError> {
        pin_init!(Self {
            set <- BioPool::new(pool_size, T::FRONT_PAD),
            _t: PhantomData,
        }? BchError)
    }
}

impl<T: HasBio> BioSet<T> {
    /// A T with a bio for @bdev, @opf, of @nr_vecs bvecs - inline, or the
    /// set's - and its other fields from @init. GFP_KERNEL: NOIO or NOFS is
    /// the caller's memalloc scope's. Fails only if @init does, or past
    /// BIO_MAX_VECS or the set's bvec pool; otherwise it waits on the mempool.
    ///
    /// Which deadlocks, if what it waits for is a bio this task holds from
    /// the set, unsubmitted: one at a time, or a pool as big as how many are
    /// held at once.
    pub fn alloc<E, I>(&self, bdev: *mut crate::os::blkdev::BlockDevice, nr_vecs: u16, opf: u32,
                       init: impl FnOnce(BioInPlace) -> I) -> Result<BioBox<T>, E>
    where
        I: PinInit<T, E>,
        E: From<BchError>,
    {
        self.alloc_with(bdev, opf, nr_vecs, init)
    }

    /// alloc(), with its bvecs filled from @src: B's, released as the box is
    /// dropped.
    pub fn alloc_with<B: Buffers, E, I>(&self, bdev: *mut crate::os::blkdev::BlockDevice, opf: u32, src: B::Src<'_>,
                                        init: impl FnOnce(BioInPlace) -> I) -> Result<BioBox<T, B>, E>
    where
        I: PinInit<T, E>,
        E: From<BchError>,
    {
        let bio = self.alloc_raw(bdev, B::nr_vecs(&src), opf, c::GFP_KERNEL);
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
                unsafe { bio_put(bio) };
                Err(e)
            }
        }
    }
}

unsafe impl<T: FrontPad> ::pin_init::Zeroable for BioSet<T> {}

unsafe impl<T: FrontPad> crate::util::late::LateInit for BioSet<T> {
    fn is_made(&self) -> bool {
        self.set.pool.is_made()
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
            crate::os::blkdev::submit_bio(bio);
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
    pub fn new<E, I>(bdev: *mut crate::os::blkdev::BlockDevice, nr_vecs: u16, opf: u32,
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
pub unsafe fn inline_bio<T: HasBio, E, I>(bdev: *mut crate::os::blkdev::BlockDevice, vecs: NonNull<u8>, nr_vecs: u16, opf: u32,
                                          init: impl FnOnce(BioInPlace) -> I) -> impl PinInit<T, E>
where
    I: PinInit<T, E>,
{
    unsafe {
        ::pin_init::pin_init_from_closure(move |t: *mut T| {
            let bio = bio_of(t);
            crate::os::bio::init(bio, bdev, vecs.as_ptr().cast(), nr_vecs, opf);
            init_at(init(BioInPlace(bio)), t).inspect_err(|_| crate::os::bio::uninit(bio))
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
    pub fn exclusive_bio_in<E, I>(alloc: A, bdev: *mut crate::os::blkdev::BlockDevice, nr_vecs: u16, opf: u32,
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
    pub fn new_with<E, I>(bdev: *mut crate::os::blkdev::BlockDevice, opf: u32, src: B::Src<'_>,
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
                bio_put(bio);
            } else {
                crate::os::bio::uninit(bio);
                kfree(self.t.cast(), inline_layout::<T>(nr_vecs).0);
            }
        }
    }
}
