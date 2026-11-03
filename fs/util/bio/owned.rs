// SPDX-License-Identifier: GPL-2.0

//! A bio's owners. Each owns a T that ends in its bio - HasBio: the bio is
//! the T's last field, and the T is found from it by that offset.
//!
//! - BioBox<T>: a ref on a bio from a BioSet<T>, a bioset whose front_pad is
//!   the rest of the T. bio_put() on drop.
//! - BioBuf<T>: a T and its inline bvecs, in one allocation of their own.
//!   BioBufPtr<T> is one as a thin pointer, for C's struct bio * fields.
//!
//! They deref to the T, and change it through Pin<&mut T>: the T holds its
//! bio, which is pinned.
//!
//! A T is made around its bio: the bioset, or BioBuf::new(), puts the bio
//! where it goes, then the caller's initializer does the rest of the T - its
//! bio field from BioInPlace::init(), which writes nothing.
//!
//! Submitting (RawBio::submit()) turns the owner into its bio for the block
//! layer; the endio turns it back, and hands it to Endio::endio().

use core::alloc::Layout;
use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicBool, Ordering::{Acquire, Release}};
use core::ops::Deref;
use core::pin::Pin;
use core::ptr::NonNull;

use pin_init::{pin_data, PinInit};

use super::{pages, Bio, BioOwner, BioRange};
use crate::c;
use crate::fs::Fs;
use crate::util::kernel::PAGE_SIZE;
use crate::errcode::{ret_to_result_void, BchError};
use crate::util::alloc::{init_at, kfree, kzalloc, KBox};
use crate::util::ffi::Opaque;

/// # Safety
/// BIO_OFFSET is the offset of a Bio field that ends the struct, #[repr(C)]. If the
/// struct projects its pinning - #[pin_data] - that field is #[pin]: so
/// BioBox::as_mut()'s Pin<&mut Self> never hands it out unpinned, to be
/// moved off its bvecs.
pub unsafe trait HasBio: Sized {
    const BIO_OFFSET: usize;

    fn bio(&self) -> &Bio {
        unsafe { Bio::from_raw(bio_of(core::ptr::from_ref(self).cast_mut())) }
    }

    fn bio_mut(self: Pin<&mut Self>) -> Pin<&mut Bio> {
        unsafe { Bio::from_raw_mut(bio_of(self.get_unchecked_mut())) }
    }
}

// SAFETY: a bio, at its own start
unsafe impl HasBio for Bio {
    const BIO_OFFSET: usize = 0;
}

/// A struct's bio field: a Bio, or a C-shared struct's c::bio - the same
/// memory, Bio being transparent - or a BioLend, its bio AT into it.
///
/// # Safety
/// A bio is AT bytes into the field, and ends it.
pub unsafe trait BioField {
    const AT: usize = 0;
}
unsafe impl BioField for Bio {}
unsafe impl BioField for c::bio {}

#[doc(hidden)]
pub const fn bio_field_at<B: BioField>(_: *const B) -> usize {
    B::AT
}

/// HasBio for $t, by its bio field $f: checked to be a BioField, and its bio
/// last - not that it's #[pin], which HasBio asks of a struct with
/// #[pin_data].
#[macro_export]
macro_rules! has_bio {
    ($t:ty, $($f:ident).+) => {
        // SAFETY: a bio field, below, its bio ending the struct
        unsafe impl $crate::util::bio::HasBio for $t {
            const BIO_OFFSET: usize = {
                let t = ::core::mem::MaybeUninit::<$t>::uninit();
                let field = unsafe { &raw const (*t.as_ptr()).$($f).+ };
                let off = ::core::mem::offset_of!($t, $($f).+) + $crate::util::bio::bio_field_at(field);
                assert!(off + ::core::mem::size_of::<$crate::util::bio::Bio>() == ::core::mem::size_of::<$t>(),
                        concat!(stringify!($t), ": its bio isn't last"));
                off
            };
        }
    };
}

fn bio_of<T: HasBio>(t: *mut T) -> *mut c::bio {
    t.cast::<u8>().wrapping_add(T::BIO_OFFSET).cast()
}

/// # Safety
/// @bio is a T's.
pub unsafe fn of_bio<T: HasBio>(bio: *mut c::bio) -> *mut T {
    // SAFETY: the caller's
    unsafe { bio.byte_sub(T::BIO_OFFSET).cast() }
}

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
        // SAFETY: a live bioset
        let bio = unsafe { c::bio_alloc_bioset(bdev, nr_vecs as _, opf as _, c::GFP_KERNEL, self.set.as_ptr()) };
        Self::init_new(bio, init)
    }

    /// alloc(), the bio a clone of @src's bio, at @src: its bvecs shared, so it
    /// borrows them as @src does.
    pub fn alloc_clone<'a, E, I>(&self, bdev: *mut c::block_device, src: BioRange<'a>,
                                 init: impl FnOnce(BioInPlace) -> I) -> Result<BioClone<'a, T>, E>
    where
        I: PinInit<T, E>,
        E: From<BchError>,
    {
        // SAFETY: a live bioset; @src's bio only read
        let bio = unsafe {
            c::bio_alloc_clone(bdev, core::ptr::from_ref(src.bio().raw()).cast_mut(), c::GFP_KERNEL,
                               self.set.as_ptr())
        };
        let mut b = Self::init_new(bio, init)?;
        b.bio_mut().raw_mut().bi_iter = src.pos();
        Ok(BioClone { b, _src: PhantomData })
    }

    /// alloc(), with @size bytes of data of its own, @gfp's - what that can't
    /// have from the page allocator from the bounce pool, so it doesn't fail
    /// for want of it.
    pub fn alloc_data<'f, E, I>(&self, fs: &'f Fs, size: usize, gfp: c::gfp_t,
                                init: impl FnOnce(BioInPlace) -> I) -> Result<BioBoxData<'f, T>, E>
    where
        I: PinInit<T, E>,
        E: From<BchError>,
    {
        let nr_vecs = size.div_ceil(PAGE_SIZE).try_into().expect("a bio's data of more than 64k pages");
        let mut b = self.alloc(core::ptr::null_mut(), nr_vecs, 0, init)?;
        pages::alloc_pages_pool(fs, b.bio_mut().raw_mut(), size, gfp);
        Ok(BioBoxData { b, fs })
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
            Ok(()) => Ok(BioBox(unsafe { NonNull::new_unchecked(t) })),
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

pub struct BioBox<T: HasBio>(NonNull<T>);

// SAFETY: a T, as a Box's: what has the box has the T
unsafe impl<T: HasBio + Send> Send for BioBox<T> {}
unsafe impl<T: HasBio + Sync> Sync for BioBox<T> {}

// SAFETY: the T's bio, and back - container_of()
unsafe impl<T: HasBio> RawBio for BioBox<T> {
    fn into_raw(self) -> *mut c::bio {
        let bio = bio_of(self.0.as_ptr());
        core::mem::forget(self);
        bio
    }

    unsafe fn from_raw(bio: *mut c::bio) -> Self {
        // SAFETY: the caller's
        Self(unsafe { NonNull::new_unchecked(of_bio::<T>(bio)) })
    }
}

impl<T: HasBio> Deref for BioBox<T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: the T, live while it's had
        unsafe { self.0.as_ref() }
    }
}

impl<T: HasBio> BioBox<T> {
    /// The T, for an owner that keeps the box and reaches it by pointer.
    pub fn as_ptr(&self) -> *mut T {
        self.0.as_ptr()
    }

    pub fn as_mut(&mut self) -> Pin<&mut T> {
        // SAFETY: the T, live while it's had, and the box's alone - a bio in
        // flight has given up its box; never moved from where its set made it
        unsafe { Pin::new_unchecked(self.0.as_mut()) }
    }

    /// The T dropped, and the bio's ref given up raw, for a put that may come
    /// later - bch2_bio_check_pages_dirty()'s, from a workqueue.
    #[cfg(kernel)]
    pub(super) fn into_bio(self) -> *mut c::bio {
        let t = core::mem::ManuallyDrop::new(self).0;
        unsafe { core::ptr::drop_in_place(t.as_ptr()) };
        bio_of(t.as_ptr())
    }
}

// SAFETY: the box's bio, its only ref
unsafe impl<T: HasBio> BioOwner for BioBox<T> {
    fn bio(&self) -> &Bio {
        HasBio::bio(&**self)
    }

    fn bio_mut(&mut self) -> Pin<&mut Bio> {
        HasBio::bio_mut(self.as_mut())
    }
}

impl<T: HasBio> Drop for BioBox<T> {
    fn drop(&mut self) {
        // SAFETY: the T and its ref, dropped then put - which frees it to its
        // set, if the ref's the last
        unsafe {
            core::ptr::drop_in_place(self.0.as_ptr());
            c::bio_put(bio_of(self.0.as_ptr()));
        }
    }
}

/// A BioBox of a clone: alloc_clone()'s, borrowing the bio it shares the
/// bvecs of.
pub struct BioClone<'a, T: HasBio> {
    b:    BioBox<T>,
    _src: PhantomData<BioRange<'a>>,
}

impl<T: HasBio> Deref for BioClone<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.b
    }
}

// SAFETY: its box's
unsafe impl<T: HasBio> BioOwner for BioClone<'_, T> {
    fn bio(&self) -> &Bio {
        self.b.bio()
    }

    fn bio_mut(&mut self) -> Pin<&mut Bio> {
        self.b.bio_mut()
    }
}

/// A BioBox and its data: alloc_data()'s, freed with it.
pub struct BioBoxData<'f, T: HasBio> {
    b:  BioBox<T>,
    fs: &'f Fs,
}

impl<T: HasBio> Deref for BioBoxData<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.b
    }
}

// SAFETY: its box's
unsafe impl<T: HasBio> BioOwner for BioBoxData<'_, T> {
    fn bio(&self) -> &Bio {
        self.b.bio()
    }

    fn bio_mut(&mut self) -> Pin<&mut Bio> {
        self.b.bio_mut()
    }
}

impl<T: HasBio> Drop for BioBoxData<'_, T> {
    fn drop(&mut self) {
        // what's in its bvecs is alloc_data()'s, or more of the same: nothing
        // safe puts anything else there for longer than a borrow of the box
        unsafe { pages::free_pages_pool(self.fs, self.b.bio_mut().raw_mut()) }
    }
}

// ── BioBuf ───────────────────────────────────────────────────────────────

/// A T, then its bio's bvecs.
#[repr(C)]
struct Inline<T> {
    t:    T,
    vecs: [c::bio_vec],
}

/// A DST, sized when made. A KBox, not a KVBox: an endio may drop it, and
/// kvfree() isn't safe in interrupt context.
pub struct BioBuf<T: HasBio>(KBox<Inline<T>>);

// SAFETY: as BioBox's
unsafe impl<T: HasBio + Send> Send for BioBuf<T> {}

impl<T: HasBio> BioBuf<T> {
    /// As BioSet::alloc(), the bvecs inline.
    pub fn new<E, I>(bdev: *mut c::block_device, nr_vecs: u16, opf: u32,
                     init: impl FnOnce(BioInPlace) -> I) -> Result<Self, E>
    where
        I: PinInit<T, E>,
        E: From<BchError>,
    {
        let layout = Layout::new::<T>()
            .extend(Layout::array::<c::bio_vec>(nr_vecs.into()).expect("a u16's worth of bvecs"))
            .expect("a T and its bvecs").0
            .pad_to_align();
        let p = kzalloc(layout).map_err(|_| BchError::from(c::ENOMEM))?;
        let inline = core::ptr::slice_from_raw_parts_mut(p.as_ptr().cast::<c::bio_vec>(), nr_vecs.into())
            as *mut Inline<T>;

        // SAFETY: the allocation, laid out as Inline<T> with nr_vecs bvecs:
        // its bio, given its own bvecs, then the rest of the T by @init -
        // and then a KBox's, as kzalloc() is
        unsafe {
            let t = &raw mut (*inline).t;
            let bio = bio_of(t);
            c::bio_init(bio, bdev, (&raw mut (*inline).vecs).cast(), nr_vecs, opf as _);
            match init_at(init(BioInPlace(bio)), t) {
                Ok(()) => Ok(Self(KBox::from_raw(inline))),
                Err(e) => {
                    c::bio_uninit(bio);
                    kfree(p, layout);
                    Err(e)
                }
            }
        }
    }
}

impl<T: HasBio> Deref for BioBuf<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0.t
    }
}

// SAFETY: the buffer's bio, its own
unsafe impl<T: HasBio> BioOwner for BioBuf<T> {
    fn bio(&self) -> &Bio {
        HasBio::bio(&self.0.t)
    }

    fn bio_mut(&mut self) -> Pin<&mut Bio> {
        HasBio::bio_mut(unsafe { Pin::new_unchecked(&mut self.0.t) })
    }
}

// SAFETY: the T's bio, and back: container_of(), and its bvecs' count -
// bi_max_vecs, which bio_init() set from it and bio_reset() keeps
unsafe impl<T: HasBio> RawBio for BioBuf<T> {
    fn into_raw(self) -> *mut c::bio {
        let this = core::mem::ManuallyDrop::new(self);
        // SAFETY: the box, moved out of what's never dropped
        let inline = KBox::into_raw(unsafe { core::ptr::read(&this.0) });
        // SAFETY: the box's
        bio_of(unsafe { &raw mut (*inline).t })
    }

    unsafe fn from_raw(bio: *mut c::bio) -> Self {
        // SAFETY: the caller's - a BioBuf<T>'s bio
        unsafe {
            let nr = (*bio).bi_max_vecs as usize;
            let inline = core::ptr::slice_from_raw_parts_mut(of_bio::<T>(bio).cast::<c::bio_vec>(), nr);
            Self(KBox::from_raw(inline as *mut Inline<T>))
        }
    }
}

impl<T: HasBio> Drop for BioBuf<T> {
    fn drop(&mut self) {
        // SAFETY: its bio, idle: the box then drops the T, and frees it all
        unsafe { c::bio_uninit(self.bio_mut().raw_mut()) }
    }
}

/// For a field C shares as a struct bio *: an Option of one is a nullable
/// pointer, zeroed None, where a BioBuf is a fat one. For as long as such
/// structs are C's.
#[repr(transparent)]
pub struct BioBufPtr<T: HasBio>(NonNull<T>);

// SAFETY: as BioBuf's
unsafe impl<T: HasBio + Send> Send for BioBufPtr<T> {}

// SAFETY: a NonNull, repr(transparent): an Option of one is a nullable
// pointer, all zeroes None
unsafe impl<T: HasBio> ::pin_init::ZeroableOption for BioBufPtr<T> {}

impl<T: HasBio> From<BioBuf<T>> for BioBufPtr<T> {
    fn from(b: BioBuf<T>) -> Self {
        // SAFETY: a bio's T: into_raw()'s
        BioBufPtr(unsafe { NonNull::new_unchecked(of_bio::<T>(b.into_raw())) })
    }
}

impl<T: HasBio> Deref for BioBufPtr<T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: its T, live while it's had
        unsafe { self.0.as_ref() }
    }
}

// SAFETY: the buffer's bio, its own
unsafe impl<T: HasBio> BioOwner for BioBufPtr<T> {
    fn bio(&self) -> &Bio {
        HasBio::bio(&**self)
    }

    fn bio_mut(&mut self) -> Pin<&mut Bio> {
        HasBio::bio_mut(unsafe { Pin::new_unchecked(self.0.as_mut()) })
    }
}

impl<T: HasBio> Drop for BioBufPtr<T> {
    fn drop(&mut self) {
        // SAFETY: a BioBuf's bio, from into_raw(), given back once
        drop(unsafe { BioBuf::<T>::from_raw(bio_of(self.0.as_ptr())) })
    }
}

// ── Submission ───────────────────────────────────────────────────────────

/// # Safety
/// from_raw() of into_raw()'s bio is the owner given up.
pub unsafe trait RawBio: BioOwner + Sized {
    fn into_raw(self) -> *mut c::bio;

    /// # Safety
    /// @bio is into_raw()'s, of an owner of this type, not since taken back.
    unsafe fn from_raw(bio: *mut c::bio) -> Self;

    /// 'static: the IO has it for as long as it takes.
    fn submit<E: Endio<Self>>(self)
    where
        Self: Send + 'static,
    {
        let bio = self.into_raw();
        // SAFETY: the bio, the owner's: endio::<Self, E>() takes it back
        unsafe {
            (*bio).bi_end_io = Some(endio::<Self, E>);
            c::submit_bio(bio);
        }
    }
}

/// The result is bio().status(). A type of its own, not O's: one struct's
/// bios may complete differently by the path that submitted them.
///
/// It runs where the IO completes - softirq, or an interrupt - so it may not
/// sleep, nor may T's Drop, if it drops the owner: what would goes to a
/// workqueue, with the owner.
pub trait Endio<O> {
    fn endio(bio: O);
}

unsafe extern "C" fn endio<O: RawBio, E: Endio<O>>(bio: *mut c::bio) {
    // SAFETY: armed by submit() alone, on an O's bio, given up with it
    E::endio(unsafe { O::from_raw(bio) })
}
