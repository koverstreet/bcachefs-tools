// SPDX-License-Identifier: GPL-2.0

//! struct bio, from Rust.
//!
//! Bio is a view of one, owned or not: what any holder of a bio may do.
//!
//! A bio is self-referential: its bvecs may be inline, right after it in its
//! allocation. So nothing hands one out by value or by &mut - Bio's methods
//! take &self, its C state an Opaque - and moving one can't break that.
//!
//! What allocated a bio owns it (owned.rs): BioBox, from a BioSet - a bioset
//! of bios at the end of a T, HasBio - and BioBuf, a T and its inline bvecs in
//! memory of their own.
//!
//! After Andreas Hindborg's kernel::block::bio, from his rnull series
//! (git.kernel.org a.hindborg/linux.git, rnull-v6.19-rc5, 41e04e1bb2e6): the
//! kernel crate vendored here hasn't got it, and couldn't serve userspace. So
//! it's over c::bio here, in both builds - the shim's bio_vec laid out as the
//! kernel's.

use core::marker::PhantomData;
use core::ptr::NonNull;

use crate::c;
use crate::prt;
use crate::util::ffi::Opaque;
use crate::util::{Printbuf, ToText};

mod owned;

pub use owned::{BioBox, BioBuf, BioInPlace, BioSet, HasBio};

// A bvec_iter's offset into its bvec, as a place: bi_offset - bi_bvec_done
// before 7.3, and in the shim.
#[cfg(kernel)]
#[macro_use]
mod compat {
    #[cfg(linux_ge = "7.3")]
    macro_rules! bi_offset { ($i:expr) => { $i.bi_offset } }
    #[cfg(not(linux_ge = "7.3"))]
    macro_rules! bi_offset { ($i:expr) => { $i.bi_bvec_done } }
}
#[cfg(not(kernel))]
macro_rules! bi_offset { ($i:expr) => { $i.bi_bvec_done } }

/// A block IO (struct bio): what to read or write, where, and the memory.
#[repr(transparent)]
pub struct Bio(Opaque<c::bio>);

// SAFETY: a bio is no thread's - submitted on one, completed on another. Not
// Sync: two holders of a &Bio could write its data at once, or one read its
// position while the block layer advances it.
unsafe impl Send for Bio {}

impl Bio {
    /// # Safety
    /// @ptr is a bio, valid for 'a.
    pub unsafe fn from_raw<'a>(ptr: *mut c::bio) -> &'a Self {
        // SAFETY: the caller's; Bio is a transparent c::bio
        unsafe { &*ptr.cast() }
    }

    pub fn as_ptr(&self) -> *mut c::bio {
        self.0.as_ptr()
    }

    /// It, then each bio chained after it, by bi_next.
    pub fn chain_iter(&self) -> BioIterator<'_> {
        BioIterator { bio: Some(NonNull::from(self)), _p: PhantomData }
    }

    /// Whether it has data left: bio_has_data().
    pub fn has_data(&self) -> bool {
        self.raw_iter().bi_size != 0 && !self.no_advance_iter()
    }

    /// Whether it's a discard, or a write of zeroes, which have a size and no
    /// bvecs: bio_no_advance_iter().
    fn no_advance_iter(&self) -> bool {
        [c::req_op_REQ_OP_DISCARD, c::req_op_REQ_OP_SECURE_ERASE, c::req_op_REQ_OP_WRITE_ZEROES]
            .contains(&self.op())
    }

    fn op(&self) -> u32 {
        // SAFETY: a bio, for as long as &self is
        let opf = unsafe { (*self.as_ptr()).bi_opf };
        opf & ((1 << c::REQ_OP_BITS) - 1)
    }

    /// Whether it's a clone: its bvecs are another bio's, bi_vcnt 0.
    pub fn cloned(&self) -> bool {
        // SAFETY: as has_data()'s
        let flags = unsafe { (*self.as_ptr()).bi_flags };
        flags & (1 << c::BIO_CLONED as u32) != 0
    }

    fn raw_iter(&self) -> c::bvec_iter {
        // SAFETY: as has_data()'s
        unsafe { (*self.as_ptr()).bi_iter }
    }

    fn io_vec_count(&self) -> u16 {
        // SAFETY: as has_data()'s
        unsafe { (*self.as_ptr()).bi_vcnt }
    }
}

impl ToText for Bio {
    fn to_text(&self, out: &mut Printbuf) {
        let iter = self.raw_iter();
        prt!(out, "bio {:p}: vcnt {}, idx {}, size {:#x}, offset {:#x}",
             self.as_ptr(), self.io_vec_count(), iter.bi_idx, iter.bi_size, bi_offset!(iter));
    }
}

/// What allocated a bio, and holds it.
///
/// # Safety
/// bio() is the implementor's own, which nothing else has - in flight, or
/// to build on - while it's had.
pub unsafe trait BioOwner {
    fn bio(&self) -> &Bio;
}

/// The bios of a chain, from one, by bi_next.
///
/// # Invariants
/// @bio is None, or a bio valid for 'a.
pub struct BioIterator<'a> {
    bio: Option<NonNull<Bio>>,
    _p:  PhantomData<&'a Bio>,
}

impl<'a> Iterator for BioIterator<'a> {
    type Item = &'a Bio;

    fn next(&mut self) -> Option<&'a Bio> {
        let bio = self.bio.take()?;
        // SAFETY: by the invariant; the next is a bio of the chain too
        unsafe {
            let bio = bio.as_ref();
            self.bio = NonNull::new((*bio.as_ptr()).bi_next.cast());
            Some(bio)
        }
    }
}

impl core::iter::FusedIterator for BioIterator<'_> {}
