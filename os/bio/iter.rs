// SPDX-License-Identifier: GPL-2.0

//! The walks over a BioRange:
//!
//! - Its chunks, by iterating it: virtually contiguous, the walk for its
//!   data. As much of a bvec as is in the direct map - all of it, in
//!   userspace or without HIGHMEM - at its page_address(); a highmem page is
//!   a chunk of its own, kmapped while it's used. Data access - copies,
//!   zeroing, the mapped bytes - is the chunk's, so highmem goes no further
//!   than here.
//! - Its segments: the bvecs as they are, for what's done to the bvecs
//!   themselves; their pages may not all be mapped, so no data access.
//! - Its folios, in the kernel: for the vfs glue, what's done to folios.
//!   Pages, in userspace, are 4K of an address, and nothing there walks them.
//!   Each folio of a walk is as much of it as the bvec has - not a page at a
//!   time, as C's bio_for_each_folio() (vfs/io.h), over single-page bvecs, was.

use core::iter::FusedIterator;
use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::ptr::NonNull;

use super::{Bio, BioData};
use crate::util::alloc::{Alloc, Box};
use crate::util::place::Contained;
use crate::util::volatile::VolatileSlice;
use crate::c;
use crate::os::kernel::PAGE_SIZE;

/// Page @n of the run starting at @page: in the kernel, one struct page after
/// another, as bvec_iter_page(); in userspace, a page is its address. Only
/// arithmetic: what uses the page is what needs it to be one.
pub(crate) fn nth_page(page: *mut c::page, n: usize) -> *mut c::page {
    #[cfg(kernel)]
    return page.wrapping_add(n);
    #[cfg(not(kernel))]
    return page.wrapping_byte_add(n * PAGE_SIZE);
}

/// # Safety
/// @page is a page, not highmem.
pub(super) unsafe fn page_address(page: NonNull<c::page>) -> NonNull<u8> {
    // SAFETY: the caller's
    #[cfg(kernel)]
    return NonNull::new(unsafe { c::rust_page_address(page.as_ptr()) }.cast())
        .expect("a lowmem page's address");
    #[cfg(not(kernel))]
    return page.cast();
}

/// # Safety
/// @page is a page.
#[cfg(all(kernel, CONFIG_HIGHMEM))]
unsafe fn highmem(page: *mut c::page) -> bool {
    // SAFETY: the caller's
    unsafe { c::rust_page_highmem(page) }
}

/// Where a chunk's bytes are, and how many of a bvec's @len from @offset in
/// @page that is: in the direct map, as many as are; or a highmem page's, to
/// its end.
///
/// # Safety
/// @page is a bvec's, with @len bytes of it from @offset.
unsafe fn chunk_at(page: NonNull<c::page>, offset: u32, len: u32) -> (At, u32) {
    #[cfg(all(kernel, CONFIG_HIGHMEM))]
    // SAFETY: the caller's
    if unsafe { highmem(page.as_ptr()) } {
        return (At::Highmem { page, offset }, len.min(PAGE_SIZE as u32 - offset));
    }
    // and the lowmem pages after it, up to a highmem one
    #[cfg(all(kernel, CONFIG_HIGHMEM))]
    let len = {
        let (mut n, mut i) = (PAGE_SIZE as u32 - offset, 1);
        // SAFETY: the bvec's pages - @len runs on past n
        while n < len && !unsafe { highmem(nth_page(page.as_ptr(), i)) } {
            n += PAGE_SIZE as u32;
            i += 1;
        }
        n.min(len)
    };
    // SAFETY: lowmem, and @offset into it
    (At::Direct(unsafe { page_address(page).add(offset as usize) }), len)
}

pub fn reset_bvec_offset(iter: &mut c::bvec_iter) {
    bi_offset!(iter) = 0;
}

/// # Invariants
/// @iter is a position of @bio: within its bvecs, bi_size bytes of them
/// after it.
#[derive(Clone, Copy)]
pub struct BioRange<'a> {
    pub(super) data: &'a BioData,
    pub(super) iter: c::bvec_iter,
}

impl<'a> BioRange<'a> {
    pub(super) fn bio(&self) -> &Bio {
        self.data.container()
    }

    pub fn is_of(&self, bio: &Bio) -> bool {
        core::ptr::eq(self.bio(), bio)
    }

    pub fn len(&self) -> u32 {
        self.iter.bi_size
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Where it starts.
    pub fn pos(&self) -> c::bvec_iter {
        self.iter
    }

    /// Panics past its end.
    pub fn advance(&mut self, mut bytes: u32) {
        assert!(bytes <= self.len(), "advancing a bio range of {} bytes by {bytes}", self.len());
        if self.bio().no_advance_iter() {
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

    /// @bytes from where it starts, shorter or longer: the bytes a bio's
    /// buffer has past its data, a decoded extent's. Panics past its bio's
    /// bvecs, or on a clone's, whose aren't its own.
    pub fn resize(mut self, bytes: u32) -> Self {
        let bio = self.bio();
        assert!(!bio.cloned(), "resizing a range of a clone: its bvecs are its parent's");
        let room = (self.iter.bi_idx..u32::from(bio.nr_vecs())).map(|i| bio.bvec(i).bv_len).sum::<u32>()
            - bi_offset!(self.iter);
        assert!(bytes <= room, "resizing a bio range to {bytes} bytes: its bio has {room} from it");
        self.iter.bi_size = bytes;
        self
    }

    pub fn segments(self) -> Segments<'a> {
        Walk::new(self, segment)
    }

    #[cfg(kernel)]
    pub fn folios(self) -> Folios<'a> {
        Walk::new(self, folio)
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

    /// All of it, into @b, its length, not yet written: as Box::write(), b
    /// written. Panics if they differ.
    pub fn copy_into<A: Alloc>(&self, mut b: Box<[MaybeUninit<u8>], A>) -> Box<[u8], A> {
        assert_eq!(self.len() as usize, b.len(), "copying a bio range into a buffer of another length");
        let dst = b.as_mut_ptr().cast::<u8>();
        let mut done = 0;
        for chunk in *self {
            let n = chunk.len() as usize;
            // SAFETY: the chunk's bytes, mapped, into n of @b - within it, as
            // the range is its length
            chunk.with_mapped(|p, _| unsafe { core::ptr::copy_nonoverlapping(p, dst.add(done), n) });
            done += n;
        }
        // SAFETY: every byte, the range's, copied
        unsafe { b.assume_init() }
    }

    /// As copy_to(), for memory that can't be a slice.
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

    /// As much as both have, from @src - another bio's - copied: how much. The
    /// kernel's old bio_copy_data_iter(), which 7.2 removed.
    pub fn copy_from_range(&self, src: BioRange<'_>) -> u32 {
        let mut dst = *self;
        for chunk in src {
            if dst.is_empty() {
                break;
            }
            // SAFETY: the chunk's bytes, mapped while they're copied
            let n = chunk.with_mapped(|p, len| dst.copy_from_volatile(unsafe {
                VolatileSlice::from_raw(NonNull::new_unchecked(p), len)
            }));
            dst.advance(n as u32);
        }
        self.len() - dst.len()
    }

    pub fn zero(&self) {
        for chunk in *self {
            chunk.zero();
        }
    }

    /// A random u64 of it overwritten: the debug corrupt ratios'.
    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    pub fn corrupt(&self) {
        use crate::os::kernel::{random_u64, random_u64_below};
        let u64s = self.len() / 8;
        if u64s == 0 {
            return;
        }
        let mut r = *self;
        r.advance(random_u64_below(u64s.into()) as u32 * 8);
        r.take(8).copy_from(&random_u64().to_ne_bytes());
    }

    // ── bvec.h's arithmetic, on the position: each as the function named ──

    /// __bvec_iter_bvec()
    fn bvec(&self) -> c::bio_vec {
        debug_assert!(!self.is_empty());
        self.bio().bvec(self.iter.bi_idx)
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
        Walk::new(self, chunk)
    }
}

impl BioRange<'_> {
    fn walkable(&self) -> bool {
        !self.is_empty() && !self.bio().no_advance_iter()
    }

    /// Where it starts - the page, and where in it - and how much of that
    /// bvec it has; None if a walk is done.
    fn head(&self) -> Option<(NonNull<c::page>, u32, u32)> {
        if !self.walkable() {
            return None;
        }
        let mp_offset = self.mp_offset() as usize;
        let page = nth_page(self.bvec().bv_page, mp_offset / PAGE_SIZE);
        let page = NonNull::new(page).expect("a bvec's page");
        Some((page, (mp_offset % PAGE_SIZE) as u32, self.mp_len()))
    }
}

pub struct Walk<'a, T> {
    rest: BioRange<'a>,
    step: fn(&mut BioRange<'a>) -> Option<T>,
}

pub type Chunks<'a>   = Walk<'a, Chunk<'a>>;
pub type Segments<'a> = Walk<'a, Segment<'a>>;
#[cfg(kernel)]
pub type Folios<'a>   = Walk<'a, FolioVec<'a>>;

impl<'a, T> Walk<'a, T> {
    pub(super) fn new(rest: BioRange<'a>, step: fn(&mut BioRange<'a>) -> Option<T>) -> Self {
        Self { rest, step }
    }

    pub fn remaining(&self) -> BioRange<'a> {
        self.rest
    }
}

impl<T> Iterator for Walk<'_, T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        (self.step)(&mut self.rest)
    }

    /// Each step is at least a byte.
    fn size_hint(&self) -> (usize, Option<usize>) {
        if self.rest.walkable() { (1, Some(self.rest.len() as usize)) } else { (0, Some(0)) }
    }
}

impl<T> FusedIterator for Walk<'_, T> {}

// ── Chunks ───────────────────────────────────────────────────────────────

/// Its memory may be written while it's read - a user's page under direct
/// IO, a page another IO maps too - so it's never a &[u8]: copied, or handed
/// over as a pointer.
///
/// # Invariants
/// @at is @len bytes of the bio's, mapped as one.
pub struct Chunk<'a> {
    at:   At,
    len:  u32,
    _bio: PhantomData<&'a ()>,
}

enum At {
    Direct(NonNull<u8>),
    /// A highmem page, mapped when it's used
    #[cfg(all(kernel, CONFIG_HIGHMEM))]
    Highmem { page: NonNull<c::page>, offset: u32 },
}

impl Chunk<'_> {
    pub fn len(&self) -> u32 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The bytes may change under @f, as above.
    pub fn with_mapped<R>(&self, f: impl FnOnce(*mut u8, usize) -> R) -> R {
        match self.at {
            At::Direct(p) => f(p.as_ptr(), self.len as usize),
            #[cfg(all(kernel, CONFIG_HIGHMEM))]
            // SAFETY: the chunk's page - mapped, then unmapped before anything
            // else is
            At::Highmem { page, offset } => unsafe {
                let p = c::kmap_local_page(page.as_ptr()).cast::<u8>();
                let r = f(p.add(offset as usize), self.len as usize);
                c::kunmap_local(p.cast());
                r
            },
        }
    }

    /// As much as both have, copied: how much.
    pub fn copy_to(&self, dst: &mut [u8]) -> usize {
        // SAFETY: n of the chunk's bytes, mapped, into n of @dst
        self.with_mapped(|p, len| {
            let n = len.min(dst.len());
            unsafe { core::ptr::copy_nonoverlapping(p, dst.as_mut_ptr(), n) };
            n
        })
    }

    /// As copy_to().
    pub fn copy_from(&self, src: &[u8]) -> usize {
        // SAFETY: n of @src into n of the chunk's bytes, mapped
        self.with_mapped(|p, len| {
            let n = len.min(src.len());
            unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), p, n) };
            n
        })
    }

    pub fn zero(&self) {
        // SAFETY: the chunk's bytes, mapped
        self.with_mapped(|p, len| unsafe { core::ptr::write_bytes(p, 0, len) })
    }
}

pub(super) fn chunk<'a>(rest: &mut BioRange<'a>) -> Option<Chunk<'a>> {
    let (page, offset, len) = rest.head()?;
    // SAFETY: the bvec the walk is in, from where it's at
    let (at, len) = unsafe { chunk_at(page, offset, len) };
    rest.advance_in_bvec(len);

    // INVARIANT: chunk_at()'s
    Some(Chunk { at, len, _bio: PhantomData })
}

// ── Segments ─────────────────────────────────────────────────────────────

/// May run on from @page into the pages after it.
pub struct Segment<'a> {
    pub page:   NonNull<c::page>,
    pub offset: u32,
    pub len:    u32,
    _bio:       PhantomData<&'a ()>,
}

pub(super) fn segment<'a>(rest: &mut BioRange<'a>) -> Option<Segment<'a>> {
    let (page, offset, len) = rest.head()?;
    rest.advance_in_bvec(len);
    Some(Segment { page, offset, len, _bio: PhantomData })
}

// ── Folios ───────────────────────────────────────────────────────────────

#[cfg(kernel)]
pub struct FolioVec<'a> {
    pub folio:  NonNull<c::folio>,
    pub offset: usize,
    pub len:    usize,
    _bio:       PhantomData<&'a ()>,
}

#[cfg(kernel)]
pub(super) fn folio<'a>(rest: &mut BioRange<'a>) -> Option<FolioVec<'a>> {
    let (page, offset, len) = rest.head()?;
    // SAFETY: a bvec's page, and the folio it's of
    let (folio, offset, len) = unsafe {
        let folio = c::rust_page_folio(page.as_ptr());
        let offset = c::rust_folio_page_idx(folio, page.as_ptr()) as usize * PAGE_SIZE + offset as usize;
        (folio, offset, (c::folio_size(folio) - offset).min(len as usize))
    };
    rest.advance_in_bvec(len as u32);

    Some(FolioVec {
        folio: NonNull::new(folio).expect("a page's folio"),
        offset,
        len,
        _bio: PhantomData,
    })
}

// ── Mapped as one ────────────────────────────────────────────────────────

/// A range's memory as one buffer, without a copy: in place, its chunks one
/// after another in the direct map; or in the kernel its pages vmapped,
/// unmapped when it's dropped.
pub enum Mapped<'a> {
    InPlace(VolatileSlice<'a>),
    #[cfg(kernel)]
    Vmap { vmap: crate::os::vmap::Vmap, offset: usize, len: usize },
}

impl Mapped<'_> {
    pub fn data(&self) -> VolatileSlice<'_> {
        match self {
            Mapped::InPlace(data) => *data,
            #[cfg(kernel)]
            Mapped::Vmap { vmap, offset, len } => vmap.data().sub(*offset..offset + len),
        }
    }
}

impl<'a> BioRange<'a> {
    /// None if it can't be: not virtually contiguous, nor whole pages but
    /// where it starts and ends.
    pub fn map(self) -> Option<Mapped<'a>> {
        #[cfg(not(CONFIG_HIGHMEM))]
        if let Some(data) = self.in_place() {
            return Some(Mapped::InPlace(data));
        }
        #[cfg(kernel)]
        return self.vmap();
        #[cfg(not(kernel))]
        None
    }

    /// Where it is, if it's virtually contiguous: no HIGHMEM, so every
    /// chunk's in the direct map.
    #[cfg(not(CONFIG_HIGHMEM))]
    fn in_place(self) -> Option<VolatileSlice<'a>> {
        let mut chunks = self.into_iter();
        let first = chunks.next()?;
        let At::Direct(start) = first.at;
        let mut end = start.as_ptr().wrapping_add(first.len as usize);
        for c in chunks {
            let At::Direct(p) = c.at;
            if p.as_ptr() != end {
                return None;
            }
            end = p.as_ptr().wrapping_add(c.len as usize);
        }
        // SAFETY: its bytes, one chunk after another, the bio's for 'a
        Some(unsafe { VolatileSlice::from_raw(start, self.len() as usize) })
    }

    #[cfg(kernel)]
    fn vmap(self) -> Option<Mapped<'a>> {
        use crate::os::vmap::Vmap;
        use crate::util::alloc::KVVec;
        use core::ptr;

        let pages_of = |s: &Segment<'_>| ((s.offset + s.len) as usize).div_ceil(PAGE_SIZE);
        let offset = self.segments().next()?.offset as usize;
        let mut seen = 0;
        for s in self.segments() {
            if (seen != 0 && s.offset != 0) ||
               (seen + s.len != self.len() && (s.offset + s.len) as usize % PAGE_SIZE != 0) {
                return None;
            }
            seen += s.len;
        }
        let nr = (offset + self.len() as usize).div_ceil(PAGE_SIZE);

        let mut stack = [ptr::null_mut::<c::page>(); 16];
        let mut heap;
        let pages: &mut [*mut c::page] = if nr <= stack.len() {
            &mut stack[..nr]
        } else {
            heap = KVVec::from_elem(ptr::null_mut(), nr).ok()?;
            &mut heap
        };

        let mut i = 0;
        for s in self.segments() {
            for n in 0..pages_of(&s) {
                pages[i] = nth_page(s.page.as_ptr(), n);
                i += 1;
            }
        }

        // SAFETY: the bio's pages, which it holds for 'a
        let vmap = unsafe { Vmap::new(pages) }?;
        Some(Mapped::Vmap { vmap, offset, len: self.len() as usize })
    }
}

#[cfg(all(test, not(kernel)))]
mod tests {
    use super::super::{Bio, BioBox, BioOwner, BioRange, BioSubmit};
    use crate::c;
    use crate::errcode::BchError;

    /// A bio of three bvecs over three buffers - not virtually adjacent, so
    /// three chunks: walked whole, from its position advanced into the
    /// second, and from a position saved; copied out of, into and zeroed; a
    /// walk stopped part way says what's left; a completed bio's _all walk
    /// is all of it; a discard has no chunks.
    #[test]
    fn ranges_and_chunks() {
        let (mut a, mut b, mut c_) = ([1u8; 8], [2u8; 4], [3u8; 6]);
        let mut buf = BioBox::<Bio>::new::<BchError, _>(3, 0, |bio| bio.init()).unwrap();
        for x in [&mut a[..], &mut b[..], &mut c_[..]] {
            // SAFETY: a buffer that outlives the bio, into one of its 3 bvecs
            unsafe { buf.bio_mut().map(x.as_mut_ptr(), x.len()).expect("a bvec of the three it has room for") };
        }
        let lens = |r: BioRange<'_>| r.into_iter().map(|s| s.len()).collect::<Vec<_>>();
        assert_eq!(lens(buf.range_all()), [8, 4, 6]);
        assert_eq!(buf.range_all().segments().map(|s| s.len).collect::<Vec<_>>(), [8, 4, 6]);

        let mut r = buf.bio().range();
        assert_eq!(r.len(), 18);
        r.advance(10);
        assert_eq!(lens(r), [2, 6]);

        let saved = r;
        let mut out = [0u8; 8];
        assert_eq!(r.copy_to(&mut out), 8);
        assert_eq!(out, [2, 2, 3, 3, 3, 3, 3, 3]);
        assert_eq!(saved.copy_from(&[9; 3]), 3);

        let mut chunks = buf.range_all().into_iter();
        chunks.next();
        assert_eq!(chunks.remaining().len(), 10);

        let mut tail = buf.bio().range();
        tail.advance(16);
        tail.zero();

        // what only the block layer does: the IO completed, then the bio a
        // discard
        buf.bio_mut().raw_mut().bi_iter.bi_size = 0;
        assert_eq!((lens(buf.bio().range()), lens(buf.range_all())), (vec![], vec![8, 4, 6]));

        buf.bio_mut().raw_mut().bi_opf = c::req_op_REQ_OP_DISCARD;
        assert_eq!(lens(buf.range_all()), []);

        drop(buf);
        assert_eq!((a, b, c_), ([1; 8], [2, 2, 9, 9], [9, 3, 3, 3, 0, 0]));
    }

    /// A remapping's bvecs are its own: dropped, the bio has none, so none
    /// outlives the buffer's borrow.
    #[test]
    fn remapping_ends_with_its_borrow() {
        let buf = [5u8; 512];
        let mut bio = BioBox::<Bio>::new::<BchError, _>(1, c::req_op_REQ_OP_WRITE, |b| b.init()).unwrap();
        let m = bio.remap_write(&buf).expect("one bvec, into a bio with room for one");
        assert_eq!(m.owner.range_all().len(), 512);
        drop(m);
        assert_eq!(bio.range_all().len(), 0);
    }
}
