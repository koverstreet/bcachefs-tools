// SPDX-License-Identifier: GPL-2.0

//! struct bio, from Rust. The terms, used bare here and in iter.rs and
//! owned.rs:
//!
//! - A bio describes one block IO: device, op, sector, a position in its
//!   data, and its bvecs - (page, offset, len), in an array that may be
//!   inline, right after the bio in its allocation. Bio is a view of one; its
//!   c::bio is private, read through raw().
//! - Its data is what the bvecs point at. A position is a bvec_iter: a bvec,
//!   an offset into it, and the bytes left. The bio's own is bi_iter, which
//!   the block layer advances as the IO completes. A BioRange is a position
//!   and its bio; walking one gives chunks, virtually contiguous pieces of the
//!   data, or segments, the bvecs as they are, or folios (iter.rs).
//! - Its owner is what allocated it and holds it (owned.rs): a BioBox, a ref
//!   from a BioSet, or a BioBuf, a struct and its inline bvecs in one
//!   allocation. Only the owner builds a bio, submits it, and walks all of its
//!   bvecs wherever its position is - range_all(), C's _all walks. Anything
//!   with a &Bio may read it, and walk its data from a position.
//! - Mapping a buffer adds it to the bvecs, until the bio's next reset().
//!   Mapped ties that to a borrow of the buffer, resetting the bio on drop.
//! - In flight: submitted, until its endio. An async submit consumes the
//!   owner, and its endio is given it back (RawBio::submit()), so nothing
//!   reaches a bio in flight - but submit_gathered(), the superblock's,
//!   which is why it's unsafe. Those states are what make reading a bio's
//!   fields through &Bio safe.
//! - Pinned: inline bvecs make a bio self-referential, so it's never moved,
//!   and changed only through Pin<&mut Bio> - its own methods, given one by
//!   its owner's bio_mut().
//!
//! After Andreas Hindborg's kernel::block::bio, from his rnull series
//! (git.kernel.org a.hindborg/linux.git, rnull-v6.19-rc5, 41e04e1bb2e6): the
//! kernel crate vendored here hasn't got it, and couldn't serve userspace. So
//! it's over c::bio here, in both builds - the shim's bio_vec laid out as the
//! kernel's.

use core::marker::{PhantomData, PhantomPinned};
use core::pin::Pin;
use core::ptr::NonNull;

use crate::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::prt;
use crate::util::gather::GatherRef;
use crate::util::{Printbuf, ToText};
use crate::util::volatile::VolatileSlice;

mod iter;
mod owned;

#[cfg(kernel)]
pub use iter::{FolioVec, Folios};
pub use iter::{Chunk, Chunks, Segment, Segments, Walk};
#[cfg(kernel)]
pub(crate) use iter::nth_page;
pub use owned::{BioBox, BioBuf, BioBufPtr, BioInPlace, BioSet, Endio, HasBio, RawBio};

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

/// How many bvecs mapping @len bytes at @p takes: a page each for vmalloc
/// memory, one for the direct map.
pub fn buf_nr_bvecs(p: *const u8, len: usize) -> usize {
    use crate::util::kernel::PAGE_SIZE;
    // SAFETY: an address, only compared
    #[cfg(kernel)]
    if unsafe { c::is_vmalloc_addr(p.cast()) } {
        return (len + (p as usize & (PAGE_SIZE - 1))).div_ceil(PAGE_SIZE);
    }
    let _ = (p, len, PAGE_SIZE);
    1
}

#[repr(transparent)]
pub struct Bio {
    bio:  c::bio,
    _pin: PhantomPinned,
}

// SAFETY: a bio is no thread's - submitted on one, completed on another. Not
// Sync: two holders of a &Bio could write its data at once.
unsafe impl Send for Bio {}

impl Bio {
    /// # Safety
    /// @ptr is a bio, valid for 'a, and not in flight.
    pub unsafe fn from_raw<'a>(ptr: *const c::bio) -> &'a Self {
        // SAFETY: the caller's; Bio is a transparent c::bio
        unsafe { &*ptr.cast() }
    }

    /// # Safety
    /// As from_raw()'s, and nothing else has it for 'a.
    unsafe fn from_raw_mut<'a>(ptr: *mut c::bio) -> Pin<&'a mut Self> {
        // SAFETY: the caller's; a bio never moves
        unsafe { Pin::new_unchecked(&mut *ptr.cast()) }
    }

    pub fn raw(&self) -> &c::bio {
        &self.bio
    }

    /// For its own methods, and C's: never moved out of.
    fn raw_mut(self: Pin<&mut Self>) -> &mut c::bio {
        // SAFETY: not moved, as above
        unsafe { &mut self.get_unchecked_mut().bio }
    }

    pub fn range(&self) -> BioRange<'_> {
        BioRange { bio: self, iter: self.bio.bi_iter }
    }

    /// Its data from @pos: a position saved from it, its bi_iter before the IO
    /// advanced it or a range's pos().
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

    /// It, then the bios chained after it by bi_next.
    pub fn chain_iter(&self) -> BioIterator<'_> {
        BioIterator { bio: Some(NonNull::from(self)), _p: PhantomData }
    }

    pub fn has_data(&self) -> bool {
        self.bio.bi_iter.bi_size != 0 && !self.no_advance_iter()
    }

    /// A discard, or a write of zeroes: a size, and no bvecs.
    fn no_advance_iter(&self) -> bool {
        [c::req_op_REQ_OP_DISCARD, c::req_op_REQ_OP_SECURE_ERASE, c::req_op_REQ_OP_WRITE_ZEROES]
            .contains(&self.op())
    }

    fn op(&self) -> u32 {
        self.bio.bi_opf & ((1 << c::REQ_OP_BITS) - 1)
    }

    /// @len bytes of @buf mapped. Panics if they don't fit, where
    /// __bio_add_page() would write past the bvecs. For a buffer kept by other
    /// means than a borrow - a stripe buffer's blocks, kept by the IO's round;
    /// map_read() and map_write() borrow it.
    ///
    /// # Safety
    /// @buf is the allocators' - in the direct map, or vmalloc - and outlives
    /// its place in the bvecs; nothing has the bio in flight.
    pub(crate) unsafe fn map(self: Pin<&mut Self>, buf: *mut u8, len: usize) {
        let len = u32::try_from(len).expect("mapping a buffer of 4G or more into a bio");
        let bio = self.raw_mut();
        #[cfg(kernel)]
        // SAFETY: the caller's
        if unsafe { c::is_vmalloc_addr(buf.cast()) } {
            assert!(unsafe { c::bio_add_vmalloc(bio, buf.cast(), len) },
                    "mapping {len} bytes of vmalloc memory: more pages than the bio's bvecs");
            return;
        }
        assert!(bio.bi_vcnt < bio.bi_max_vecs, "mapping {len} bytes: the bio's bvecs are full");
        // SAFETY: the caller's, and a bvec free
        unsafe { c::bio_add_virt_nofail(bio, buf.cast(), len) };
    }

    pub fn reset(self: Pin<&mut Self>, bdev: *mut c::block_device, opf: u32) {
        // SAFETY: had by &mut, so not in flight
        unsafe { c::bio_reset(self.raw_mut(), bdev, opf as _) }
    }

    pub fn set_sector(self: Pin<&mut Self>, sector: u64) {
        self.raw_mut().bi_iter.bi_sector = sector;
    }

    /// Its size, for a bio with no data to size it by: a discard's.
    pub fn set_size(self: Pin<&mut Self>, bytes: u32) {
        self.raw_mut().bi_iter.bi_size = bytes;
    }

    /// Its completed IO's result: a BCH_ERR_BLK_STS_* errcode on failure.
    pub fn status(&self) -> Result<(), BchError> {
        match self.bio.bi_status {
            0  => Ok(()),
            // SAFETY: a status
            st => Err(unsafe { c::blk_status_to_bch_err(st) }.into()),
        }
    }

    /// A clone's bvecs are its parent's, and its bi_vcnt 0.
    pub fn cloned(&self) -> bool {
        self.bio.bi_flags & (1 << c::BIO_CLONED as u32) != 0
    }

    /// # Safety
    /// @idx is one of its bvecs - bi_vcnt of them, or its parent's, a clone's.
    unsafe fn bvec(&self, idx: u32) -> c::bio_vec {
        // SAFETY: the caller's
        unsafe { *self.bio.bi_io_vec.add(idx as usize) }
    }
}

impl ToText for Bio {
    fn to_text(&self, out: &mut Printbuf) {
        let iter = self.bio.bi_iter;
        prt!(out, "bio {:p}: vcnt {}, idx {}, size {:#x}, offset {:#x}",
             self, self.bio.bi_vcnt, iter.bi_idx, iter.bi_size, bi_offset!(iter));
    }
}

/// # Safety
/// bio() is the implementor's own, which nothing else has - in flight, or
/// to build on - while it's had.
pub unsafe trait BioOwner {
    fn bio(&self) -> &Bio;

    fn bio_mut(&mut self) -> Pin<&mut Bio>;

    /// Panics on a clone.
    fn range_all(&self) -> BioRange<'_> {
        let bio = self.bio();
        assert!(!bio.cloned(), "the _all walk of a cloned bio: its bvecs are its parent's");
        // SAFETY: its own bvecs, bi_vcnt of them
        let size = (0..u32::from(bio.bio.bi_vcnt)).map(|i| unsafe { bio.bvec(i) }.bv_len).sum();
        // INVARIANT: from its first bvec, all of them
        BioRange {
            bio,
            iter: c::bvec_iter { bi_size: size, ..Default::default() },
        }
    }

    fn submit_wait(&mut self) -> Result<(), BchError> {
        // SAFETY: the owner's bio, held for the IO
        ret_to_result_void(unsafe { c::submit_bio_wait(self.bio_mut().raw_mut()) })
    }

    /// Panics if the bio isn't a read, or hasn't the bvecs for @buf.
    fn map_read<'a>(&'a mut self, buf: &'a mut [u8]) -> Mapped<'a, Self, &'a mut [u8]>
    where
        Self: Sized,
    {
        assert_eq!(self.bio().op(), c::req_op_REQ_OP_READ, "map_read() of a bio for another op");
        // SAFETY: @buf's borrowed as long as the mapping is
        unsafe { self.bio_mut().map(buf.as_mut_ptr(), buf.len()) };
        Mapped { owner: self, _buf: PhantomData }
    }

    /// Panics if the bio isn't a write, or hasn't the bvecs for @buf.
    fn map_write<'a>(&'a mut self, buf: &'a [u8]) -> Mapped<'a, Self, &'a [u8]>
    where
        Self: Sized,
    {
        assert_eq!(self.bio().op(), c::req_op_REQ_OP_WRITE, "map_write() of a bio for another op");
        // SAFETY: as map_read()'s; a write only reads it
        unsafe { self.bio_mut().map(buf.as_ptr().cast_mut(), buf.len()) };
        Mapped { owner: self, _buf: PhantomData }
    }
}

/// An owner's bio mapped to a buffer it borrows: B is the borrow, &mut [u8]
/// for a read and & for a write. Dropping it resets the bio, so no bvec
/// outlives the borrow.
pub struct Mapped<'a, O: BioOwner, B> {
    owner: &'a mut O,
    _buf:  PhantomData<B>,
}

impl<O: BioOwner, B> Mapped<'_, O, B> {
    pub fn submit_wait(&mut self) -> Result<(), BchError> {
        self.owner.submit_wait()
    }

    /// Submitted, @r put when it completes. The mapping stays until the bio's
    /// next reset().
    ///
    /// sb/io's alone: the bio isn't given up, as an async submit's is, so
    /// it's reachable in flight. It goes once the superblock's IO consumes
    /// its bio.
    ///
    /// # Safety
    /// Nothing touches the bio, or the buffer it's mapped to, until @r's
    /// Gather has been waited on; nor walks or submits the bio until its next
    /// reset().
    pub unsafe fn submit_gathered(self, r: GatherRef<()>) {
        let mut this = core::mem::ManuallyDrop::new(self);
        let bio = this.owner.bio_mut().raw_mut();
        bio.bi_private = r.into_raw().cast_mut().cast();
        bio.bi_end_io = Some(gathered_endio);
        // SAFETY: the owner's, built for this IO; the caller keeps it, and
        // the buffer, from being touched until it's done
        unsafe { c::submit_bio(bio) };
    }
}

unsafe extern "C" fn gathered_endio(bio: *mut c::bio) {
    // SAFETY: submit_gathered()'s, whose ref is bi_private
    drop(unsafe { GatherRef::<()>::from_raw((*bio).bi_private.cast()) })
}

impl<O: BioOwner, B> Drop for Mapped<'_, O, B> {
    fn drop(&mut self) {
        let bio = self.owner.bio().raw();
        let (bdev, opf) = (bio.bi_bdev, bio.bi_opf);
        self.owner.bio_mut().reset(bdev, opf);
    }
}

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

    /// Where it starts, to save for range_at().
    pub fn pos(&self) -> c::bvec_iter {
        self.iter
    }

    /// Panics past its end.
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

    /// Its first @bytes. Panics past its end.
    pub fn take(mut self, bytes: u32) -> Self {
        assert!(bytes <= self.len(), "the first {bytes} bytes of a bio range of {}", self.len());
        self.iter.bi_size = bytes;
        self
    }

    pub fn segments(self) -> Segments<'a> {
        Walk::new(self, iter::segment)
    }

    #[cfg(kernel)]
    pub fn folios(self) -> Folios<'a> {
        Walk::new(self, iter::folio)
    }

    /// As much as both have, copied: how much.
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

    /// As copy_to().
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

    /// As much as both have, into @dst: as copy_to(), for memory that can't be
    /// a slice.
    pub fn copy_to_volatile(&self, dst: VolatileSlice<'_>) -> usize {
        let mut done = 0;
        for chunk in *self {
            if done == dst.len() {
                break;
            }
            let n = (chunk.len() as usize).min(dst.len() - done);
            // SAFETY: n of the chunk's bytes, mapped, into n of @dst
            chunk.with_mapped(|p, _| unsafe { core::ptr::copy_nonoverlapping(p, dst.as_ptr().add(done), n) });
            done += n;
        }
        done
    }

    /// As copy_to_volatile().
    pub fn copy_from_volatile(&self, src: VolatileSlice<'_>) -> usize {
        let mut done = 0;
        for chunk in *self {
            if done == src.len() {
                break;
            }
            let n = (chunk.len() as usize).min(src.len() - done);
            // SAFETY: n of @src into n of the chunk's bytes, mapped
            chunk.with_mapped(|p, _| unsafe { core::ptr::copy_nonoverlapping(src.as_ptr().add(done), p, n) });
            done += n;
        }
        done
    }

    pub fn zero(&self) {
        for chunk in *self {
            chunk.zero();
        }
    }

    // ── bvec.h's arithmetic, on the position: each as the function named ──

    /// __bvec_iter_bvec()
    fn bvec(&self) -> c::bio_vec {
        debug_assert!(!self.is_empty());
        // SAFETY: the invariant - not empty, so its start is in a bvec
        unsafe { self.bio.bvec(self.iter.bi_idx) }
    }

    /// mp_bvec_iter_offset(): past the bvec's first page, if it spans pages
    fn mp_offset(&self) -> u32 {
        self.bvec().bv_offset + bi_offset!(self.iter)
    }

    /// mp_bvec_iter_len()
    fn mp_len(&self) -> u32 {
        self.len().min(self.bvec().bv_len - bi_offset!(self.iter))
    }

    /// bio_advance_iter_single(): @bytes within the bvec
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
        // SAFETY: by the invariant
        let bio = unsafe { bio.as_ref() };
        // INVARIANT: the next is a bio of the chain too
        self.bio = NonNull::new(bio.bio.bi_next.cast());
        Some(bio)
    }
}

impl core::iter::FusedIterator for BioIterator<'_> {}
