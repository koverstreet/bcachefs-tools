// SPDX-License-Identifier: GPL-2.0

//! A bio and the folios it holds: Bio<Holds<H>>, its bvecs Bvec<H>s, each a
//! range of a folio and what it holds of it - a writeback count, a lock,
//! pins - released as it's dropped.
//!
//! A bvec's page is its folio's head, its offset into the folio, as
//! bvec_set_folio() makes them: every folio is a page, so a bvec of ours
//! holds a folio.

use super::{Bio, BioOwns};
use crate::c;
use crate::os::folio::Folio;
use crate::util::alloc::AllocError;
use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::ops::{Deref, DerefMut};
use core::ptr::{self, NonNull};

/// What a bvec holds of its folio, given up as the folio and released with
/// the bvec's range.
///
/// # Safety
/// folio() is held while the H is, and until release() of a bvec made of it.
pub unsafe trait FolioHold: Sized {
    /// The folio's address space's, as Folio is typed.
    type A;

    fn folio(&self) -> NonNull<c::folio>;

    fn into_folio(self) -> NonNull<c::folio> {
        ManuallyDrop::new(self).folio()
    }

    /// # Safety
    /// @len bytes from @offset of @folio are a bvec's, made of into_folio()'s,
    /// released once.
    unsafe fn release(folio: NonNull<c::folio>, offset: usize, len: usize);
}

/// @len bytes of a folio from @offset, and what it holds of it: H.
#[repr(transparent)]
pub struct Bvec<H: FolioHold> {
    bv: c::bio_vec,
    _h: PhantomData<H>,
}

impl<H: FolioHold> Bvec<H> {
    /// Panics if its folio hasn't @len bytes from @offset.
    pub fn new(h: H, offset: usize, len: usize) -> Self {
        let size = unsafe { c::folio_size(h.folio().as_ptr()) };
        assert!(offset + len <= size, "a bvec of {len} bytes at {offset} of a folio of {size}");
        Bvec {
            bv: c::bio_vec { bv_page: h.into_folio().as_ptr().cast(), bv_len: len as u32, bv_offset: offset as u32 },
            _h: PhantomData,
        }
    }

    fn raw_folio(&self) -> NonNull<c::folio> {
        unsafe { NonNull::new_unchecked(self.bv.bv_page.cast()) }
    }

    pub fn folio(&self) -> &Folio<H::A> {
        unsafe { Folio::from_raw(self.raw_folio()) }
    }

    pub fn offset(&self) -> usize {
        self.bv.bv_offset as usize
    }

    pub fn len(&self) -> u32 {
        self.bv.bv_len
    }
}

impl<H: FolioHold> Drop for Bvec<H> {
    fn drop(&mut self) {
        unsafe { H::release(self.raw_folio(), self.offset(), self.len() as usize) }
    }
}

/// What a bio of folios owns: each bvec its hold, H, on its folio - released
/// as the bvec's dropped.
pub struct Holds<H: FolioHold>(PhantomData<H>);

impl<H: FolioHold> BioOwns for Holds<H> {
    type Bvec = Bvec<H>;
}

impl<H: FolioHold> Bio<Holds<H>> {
    /// @v, last: grown into, from its BvecAlloc, past what it has room for.
    pub fn push(&mut self, v: Bvec<H>) -> Result<(), AllocError> {
        let len = v.len();
        self.vecs.push(v)?;
        self.sync();
        self.raw_mut().bi_iter.bi_size += len;
        Ok(())
    }

    pub fn vecs(&self) -> &[Bvec<H>] {
        &self.vecs
    }

    /// Its bvecs dropped - what they own given back - and its position
    /// emptied with them.
    pub fn release(&mut self) {
        self.vecs.clear();
        self.sync();
        self.raw_mut().bi_iter.bi_size = 0;
    }
}

/// What it's lent to - the write path, submit - has the plain Bio, which
/// can't add or free them: the same bytes, a Bvec<H> being a bio_vec.
impl<H: FolioHold> Deref for Bio<Holds<H>> {
    type Target = Bio;

    fn deref(&self) -> &Bio {
        unsafe { &*ptr::from_ref(self).cast() }
    }
}

// XXX: through this, its bvecs can be swapped with another bio's - mem::swap()
// of two &mut Bio - and its drop then releases what H never held. And Bio
// still has map(), add_folio(), truncate_vecs(), clear() and pin_user_pages(),
// for the owners that aren't typed yet - each goes with its last; till then a
// Bio<Holds<H>> could gain bvecs that aren't H's.
impl<H: FolioHold> DerefMut for Bio<Holds<H>> {
    fn deref_mut(&mut self) -> &mut Bio {
        unsafe { &mut *ptr::from_mut(self).cast() }
    }
}
