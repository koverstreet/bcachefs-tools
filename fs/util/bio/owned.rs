// SPDX-License-Identifier: GPL-2.0

//! What owns a bio: BioBox, a ref on one from a BioSet - a bioset of bios at
//! the end of a T - and BioBuf, a T and its inline bvecs in memory of their
//! own. Both deref to the T, shared only: its bio is never had by &mut, so
//! never moved off the bvecs that may follow it.
//!
//! A T is made around its bio: the bioset, or BioBuf::new(), puts the bio
//! where it goes, then the caller's initializer does the rest of the T -
//! its bio field from BioInPlace::init(), which writes nothing, the bio
//! already being there.

use core::alloc::Layout;
use core::marker::PhantomData;
use core::ops::Deref;
use core::ptr::NonNull;

use pin_init::PinInit;

use super::{Bio, BioOwner};
use crate::c;
use crate::errcode::BchError;
use crate::util::alloc::{init_at, kfree, kzalloc, KBox};
use crate::util::ffi::Opaque;

/// A #[repr(C)] struct whose last field is a Bio: a bioset's front_pad is
/// where that is, and the struct is found from its bio - container_of().
///
/// # Safety
/// BIO_OFFSET is the offset of a Bio field that ends the struct.
pub unsafe trait HasBio: Sized {
    const BIO_OFFSET: usize;
}

// SAFETY: a bio, at its own start
unsafe impl HasBio for Bio {
    const BIO_OFFSET: usize = 0;
}

/// HasBio for $t, by its Bio field $f: checked to be a Bio, and last.
#[macro_export]
macro_rules! has_bio {
    ($t:ty, $f:ident) => {
        // SAFETY: a Bio field, below, that ends the struct
        unsafe impl $crate::util::bio::HasBio for $t {
            const BIO_OFFSET: usize = {
                let off = ::core::mem::offset_of!($t, $f);
                assert!(off + ::core::mem::size_of::<$crate::util::bio::Bio>() == ::core::mem::size_of::<$t>(),
                        concat!(stringify!($t), ": its bio isn't last"));
                off
            };
        }
        const _: fn(&$t) -> &$crate::util::bio::Bio = |t| &t.$f;
    };
}

/// @t's bio.
fn bio_of<T: HasBio>(t: *mut T) -> *mut c::bio {
    t.cast::<u8>().wrapping_add(T::BIO_OFFSET).cast()
}

/// The T @bio is the bio of.
///
/// # Safety
/// @bio is a T's.
unsafe fn of_bio<T: HasBio>(bio: *mut c::bio) -> *mut T {
    // SAFETY: the caller's
    unsafe { bio.byte_sub(T::BIO_OFFSET).cast() }
}

/// A T's bio, made where it goes - by its bioset, or BioBuf::new() - for the
/// T's initializer to initialize its bio field with.
pub struct BioInPlace(*mut c::bio);

impl BioInPlace {
    /// The initializer of a T's bio field: writes nothing - the bio's there.
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

// ── BioSet, BioBox ───────────────────────────────────────────────────────

/// A bioset of T's: their bios at front_pad, where a T has its bio.
#[repr(transparent)]
pub struct BioSet<T: HasBio> {
    set: Opaque<c::bio_set>,
    _t:  PhantomData<fn() -> T>,
}

impl<T: HasBio> BioSet<T> {
    /// C's bioset, as the bioset of T's it is: its front_pad is checked.
    pub fn from_c(set: &c::bio_set) -> &Self {
        assert_eq!(set.front_pad as usize, T::BIO_OFFSET, "a bioset of another type's bios");
        // SAFETY: a transparent bio_set
        unsafe { &*core::ptr::from_ref(set).cast() }
    }

    /// A T with a bio for @bdev, @opf, of @nr_vecs bvecs - inline, or the
    /// set's - and its other fields from @init, given its bio:
    /// bio_alloc_bioset(), GFP_KERNEL - NOIO or NOFS are the caller's
    /// memalloc scope's. Fails only if @init does, or past BIO_MAX_VECS or
    /// the set's bvec pool: the bio is a mempool's, so it waits for one.
    ///
    /// Which deadlocks, if what it waits for is a bio this task already has
    /// from the set and hasn't submitted: one at a time, or a pool big enough
    /// for as many as are held at once.
    pub fn alloc<E, I>(&self, bdev: *mut c::block_device, nr_vecs: u16, opf: u32,
                       init: impl FnOnce(BioInPlace) -> I) -> Result<BioBox<T>, E>
    where
        I: PinInit<T, E>,
        E: From<BchError>,
    {
        // SAFETY: a live bioset
        let bio = unsafe { c::bio_alloc_bioset(bdev, nr_vecs as _, opf as _, c::GFP_KERNEL, self.set.as_ptr()) };
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

/// A ref on a bio from a BioSet<T>, and the T around it: bio_put() on drop.
pub struct BioBox<T: HasBio>(NonNull<T>);

// SAFETY: a T, as a Box's: what has the box has the T
unsafe impl<T: HasBio + Send> Send for BioBox<T> {}

impl<T: HasBio> Deref for BioBox<T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: the T, live while it's had
        unsafe { self.0.as_ref() }
    }
}

// SAFETY: the box's bio, its only ref
unsafe impl<T: HasBio> BioOwner for BioBox<T> {
    fn bio(&self) -> &Bio {
        // SAFETY: the T's bio
        unsafe { Bio::from_raw(bio_of(self.0.as_ptr())) }
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

// ── BioBuf ───────────────────────────────────────────────────────────────

/// A T, then its bio's bvecs.
#[repr(C)]
struct Inline<T> {
    t:    T,
    vecs: [c::bio_vec],
}

/// A T and its bio's bvecs, inline, in a box of their own - bio_kmalloc()
/// and bio_init(), in Rust: a DST, sized when made, bio_uninit() on drop.
/// A KBox, not a KVBox: an endio may drop it, and kvfree() isn't safe in
/// interrupt context.
pub struct BioBuf<T: HasBio>(KBox<Inline<T>>);

// SAFETY: as BioBox's
unsafe impl<T: HasBio + Send> Send for BioBuf<T> {}

impl<T: HasBio> BioBuf<T> {
    /// A T with a bio for @bdev, @opf, of @nr_vecs inline bvecs, and its
    /// other fields from @init, given its bio.
    pub fn new<E, I>(nr_vecs: u16, bdev: *mut c::block_device, opf: u32,
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
        // SAFETY: the T's bio
        unsafe { Bio::from_raw(bio_of(core::ptr::from_ref(&self.0.t).cast_mut())) }
    }
}

impl<T: HasBio> Drop for BioBuf<T> {
    fn drop(&mut self) {
        // SAFETY: its bio, idle: the box then drops the T, and frees it all
        unsafe { c::bio_uninit(self.bio().as_ptr()) }
    }
}
