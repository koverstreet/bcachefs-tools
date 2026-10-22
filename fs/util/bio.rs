// SPDX-License-Identifier: GPL-2.0

//! struct bio, from Rust.
//!
//! Bio is a view of one, owned or not: what any holder of a bio may do. Its
//! data is reached as a BioRange - a position and a length, C's bvec_iter,
//! with its bio - from where the bio's at, bi_iter. Iterating a range walks
//! its data, virtually contiguous chunks; segments() walks its bvecs as they
//! are, folios() its folios (iter.rs).
//!
//! A bio is self-referential: its bvecs may be inline, right after it in its
//! allocation. So nothing hands one out by value or by &mut - Bio's methods
//! take &self, its C state an Opaque - and moving one can't break that.
//!
//! What allocated a bio owns it (owned.rs): BioBox, from a BioSet - a bioset
//! of bios at the end of a T, HasBio - and BioBuf, a T and its inline bvecs in
//! memory of their own. An owner may also walk all of a bio's bvecs,
//! regardless of its position: BioOwner::range_all(), C's _all walks.
//!
//! After Andreas Hindborg's kernel::block::bio, from his rnull series
//! (git.kernel.org a.hindborg/linux.git, rnull-v6.19-rc5, 41e04e1bb2e6): the
//! kernel crate vendored here hasn't got it, and couldn't serve userspace. So
//! it's over c::bio here, in both builds - the shim's bio_vec laid out as the
//! kernel's.

use core::marker::PhantomData;
use core::ptr::NonNull;

use crate::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::prt;
use crate::util::ffi::Opaque;
use crate::util::{Printbuf, ToText};

mod iter;
mod owned;

#[cfg(kernel)]
pub use iter::{FolioVec, Folios};
pub use iter::{Chunk, Chunks, Segment, Segments, Walk};
pub use owned::{BioBox, BioBuf, BioInPlace, BioSet, Endio, HasBio, RawBio};

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

    /// Its data, from its position: what a holder that isn't its owner may
    /// walk.
    pub fn range(&self) -> BioRange<'_> {
        BioRange { bio: self, iter: self.raw_iter() }
    }

    /// Its data from @pos: a position saved from it - its bi_iter before it
    /// was advanced, or a range's pos().
    ///
    /// # Safety
    /// @pos is a position of this bio: within its bvecs, as many bytes of
    /// them after it as bi_size says.
    // XXX: unsafe, as a position carries nothing saying which bio it's of. A
    // lifetime tying them together may make this safe, once bcachefs walks its
    // bios this way throughout.
    pub unsafe fn range_at(&self, pos: c::bvec_iter) -> BioRange<'_> {
        BioRange { bio: self, iter: pos }
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

    /// # Safety
    /// @idx is one of its bvecs - bi_vcnt of them, or its parent's, a clone's.
    unsafe fn bvec(&self, idx: u32) -> c::bio_vec {
        // SAFETY: the caller's
        unsafe { *(*self.as_ptr()).bi_io_vec.add(idx as usize) }
    }
}

impl ToText for Bio {
    fn to_text(&self, out: &mut Printbuf) {
        let iter = self.raw_iter();
        prt!(out, "bio {:p}: vcnt {}, idx {}, size {:#x}, offset {:#x}",
             self.as_ptr(), self.io_vec_count(), iter.bi_idx, iter.bi_size, bi_offset!(iter));
    }
}

/// What allocated a bio, and holds it: it may walk all of the bio's bvecs,
/// wherever its position is, and submit it.
///
/// # Safety
/// bio() is the implementor's own, which nothing else has - in flight, or
/// to build on - while it's had.
pub unsafe trait BioOwner {
    fn bio(&self) -> &Bio;

    /// All its data: every bvec, from the first - C's _all walks. Not a
    /// clone's, whose bvecs are its parent's.
    fn range_all(&self) -> BioRange<'_> {
        let bio = self.bio();
        assert!(!bio.cloned(), "the _all walk of a cloned bio: its bvecs are its parent's");
        // SAFETY: its own bvecs, bi_vcnt of them
        let size = (0..u32::from(bio.io_vec_count())).map(|i| unsafe { bio.bvec(i) }.bv_len).sum();
        // INVARIANT: from its first bvec, all of them
        BioRange {
            bio,
            iter: c::bvec_iter { bi_size: size, ..Default::default() },
        }
    }

    /// Submitted, and waited on: submit_bio_wait(). Borrowed for as long as
    /// the IO takes.
    fn submit_wait(&mut self) -> Result<(), BchError> {
        // SAFETY: the owner's bio, held for the IO
        ret_to_result_void(unsafe { c::submit_bio_wait(self.bio().as_ptr()) })
    }
}

/// A range of a bio's data: where it starts, and how much - C's bvec_iter,
/// with the bio it's of. Copy: a cursor.
///
/// # Invariants
/// @iter is a position of @bio: within its bvecs, bi_size bytes of them
/// after it.
#[derive(Clone, Copy)]
pub struct BioRange<'a> {
    bio:  &'a Bio,
    iter: c::bvec_iter,
}

impl<'a> BioRange<'a> {
    pub fn bio(&self) -> &'a Bio {
        self.bio
    }

    pub fn len(&self) -> u32 {
        self.iter.bi_size
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Where it starts: to save, for range_at().
    pub fn pos(&self) -> c::bvec_iter {
        self.iter
    }

    /// Past its first @bytes: bio_advance_iter(). Panics past its end.
    pub fn advance(&mut self, mut bytes: u32) {
        assert!(bytes <= self.len(), "advancing a bio range of {} bytes by {bytes}", self.len());
        if self.bio.no_advance_iter() {
            self.iter.bi_sector += u64::from(bytes >> 9);
            self.iter.bi_size -= bytes;
            return;
        }
        while bytes != 0 {
            let n = bytes.min(self.mp_len());
            self.advance_in_bvec(n);
            bytes -= n;
        }
    }

    /// Its bvecs as they are: for what's done to the bvecs, not the data.
    pub fn segments(self) -> Segments<'a> {
        Walk::new(self, iter::segment)
    }

    /// Its folios: for the vfs glue, what's done to folios.
    #[cfg(kernel)]
    pub fn folios(self) -> Folios<'a> {
        Walk::new(self, iter::folio)
    }

    /// Its data copied to @dst, as much as both have: how much - memcpy_from_bio().
    pub fn copy_to(&self, dst: &mut [u8]) -> usize {
        let mut done = 0;
        for chunk in *self {
            if done == dst.len() {
                break;
            }
            done += chunk.copy_to(&mut dst[done..]);
        }
        done
    }

    /// @src copied into its data, as much as both have: how much - memcpy_to_bio().
    pub fn copy_from(&self, src: &[u8]) -> usize {
        let mut done = 0;
        for chunk in *self {
            if done == src.len() {
                break;
            }
            done += chunk.copy_from(&src[done..]);
        }
        done
    }

    /// Its data zeroed: zero_fill_bio_iter().
    pub fn zero(&self) {
        for chunk in *self {
            chunk.zero();
        }
    }

    // ── bvec.h's arithmetic, on the position ─────────────────────────────

    /// The bvec it starts in: __bvec_iter_bvec().
    fn bvec(&self) -> c::bio_vec {
        debug_assert!(!self.is_empty());
        // SAFETY: the invariant - not empty, so its start is in a bvec
        unsafe { self.bio.bvec(self.iter.bi_idx) }
    }

    /// Where in that bvec it starts, past its first page if it spans pages:
    /// mp_bvec_iter_offset().
    fn mp_offset(&self) -> u32 {
        self.bvec().bv_offset + bi_offset!(self.iter)
    }

    /// How much of that bvec it has: mp_bvec_iter_len().
    fn mp_len(&self) -> u32 {
        self.len().min(self.bvec().bv_len - bi_offset!(self.iter))
    }

    /// Past @bytes, which that bvec has: bio_advance_iter_single().
    fn advance_in_bvec(&mut self, bytes: u32) {
        debug_assert!(bytes <= self.mp_len());
        let bv_len = self.bvec().bv_len;
        self.iter.bi_sector += u64::from(bytes >> 9);
        self.iter.bi_size -= bytes;
        bi_offset!(self.iter) += bytes;
        if bi_offset!(self.iter) == bv_len {
            // INVARIANT: the next bvec, if there's anything left
            self.iter.bi_idx += 1;
            bi_offset!(self.iter) = 0;
        }
    }
}

impl<'a> IntoIterator for BioRange<'a> {
    type Item = Chunk<'a>;
    type IntoIter = Chunks<'a>;

    fn into_iter(self) -> Chunks<'a> {
        Walk::new(self, iter::chunk)
    }
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
