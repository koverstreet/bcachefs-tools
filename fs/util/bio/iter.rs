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
use core::ptr::NonNull;

use super::BioRange;
use crate::c;
use crate::util::kernel::PAGE_SIZE;

/// Page @n of the run starting at @page: in the kernel, one struct page after
/// another, as bvec_iter_page(); in userspace, a page is its address.
///
/// # Safety
/// @page is a bvec's, which has at least n + 1 pages.
unsafe fn nth_page(page: *mut c::page, n: usize) -> *mut c::page {
    // SAFETY: the caller's
    #[cfg(kernel)]
    return unsafe { page.add(n) };
    // SAFETY: the caller's
    #[cfg(not(kernel))]
    return unsafe { page.byte_add(n * PAGE_SIZE) };
}

/// @page's address in the direct map: page_address().
///
/// # Safety
/// @page is a page, not highmem.
unsafe fn page_address(page: NonNull<c::page>) -> NonNull<u8> {
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

impl BioRange<'_> {
    /// Whether a walk has anything to yield: a discard, or a write of zeroes,
    /// has a size and no bvecs.
    fn walkable(&self) -> bool {
        !self.is_empty() && !self.bio.no_advance_iter()
    }

    /// Where it starts - the page, and where in it - and how much of that
    /// bvec it has; None if a walk is done.
    fn head(&self) -> Option<(NonNull<c::page>, u32, u32)> {
        if !self.walkable() {
            return None;
        }
        let mp_offset = self.mp_offset() as usize;
        // SAFETY: a page of the bvec it starts in
        let page = unsafe { nth_page(self.bvec().bv_page, mp_offset / PAGE_SIZE) };
        let page = NonNull::new(page).expect("a bvec's page");
        Some((page, (mp_offset % PAGE_SIZE) as u32, self.mp_len()))
    }
}

/// A walk over a range: what's left of it, and how a step of it is taken.
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

    /// What's left of the range: where the walk's got to.
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

/// Part of a bio's data that's virtually contiguous, borrowed from it: copied
/// out of, copied into, zeroed, or its bytes handed over mapped.
///
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

    /// @f on its bytes, mapped: a pointer to them, and how many - which may
    /// change under it, as said above.
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

    /// Its first bytes copied to @dst, as many as both have: how many.
    pub fn copy_to(&self, dst: &mut [u8]) -> usize {
        // SAFETY: n of the chunk's bytes, mapped, into n of @dst
        self.with_mapped(|p, len| {
            let n = len.min(dst.len());
            unsafe { core::ptr::copy_nonoverlapping(p, dst.as_mut_ptr(), n) };
            n
        })
    }

    /// Its first bytes from @src, as many as both have: how many.
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

/// A range's next chunk, the walk past it.
pub(super) fn chunk<'a>(rest: &mut BioRange<'a>) -> Option<Chunk<'a>> {
    let (page, offset, len) = rest.head()?;
    // SAFETY: the bvec the walk is in, from where it's at
    let (at, len) = unsafe { chunk_at(page, offset, len) };
    rest.advance_in_bvec(len);

    // INVARIANT: chunk_at()'s
    Some(Chunk { at, len, _bio: PhantomData })
}

// ── Segments ─────────────────────────────────────────────────────────────

/// Part of a bvec of a bio's: __bio_for_each_bvec()'s - from @offset in
/// @page, possibly running on into the pages after it.
pub struct Segment<'a> {
    pub page:   NonNull<c::page>,
    /// Where in @page
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

/// Part of a folio of a bio's: a folio_vec.
#[cfg(kernel)]
pub struct FolioVec<'a> {
    pub folio:  NonNull<c::folio>,
    /// Where in the folio
    pub offset: usize,
    pub len:    usize,
    _bio:       PhantomData<&'a ()>,
}

/// As much of the folio the range starts in as its bvec has.
#[cfg(kernel)]
pub(super) fn folio<'a>(rest: &mut BioRange<'a>) -> Option<FolioVec<'a>> {
    let (page, offset, len) = rest.head()?;
    // SAFETY: a bvec's page, and the folio it's of
    let (folio, offset, len) = unsafe {
        let folio = c::rust_page_folio(page.as_ptr());
        let offset = c::rust_folio_page_idx(folio, page.as_ptr()) as usize * PAGE_SIZE + offset as usize;
        (folio, offset, (c::rust_folio_size(folio) - offset).min(len as usize))
    };
    rest.advance_in_bvec(len as u32);

    Some(FolioVec {
        folio: NonNull::new(folio).expect("a page's folio"),
        offset,
        len,
        _bio: PhantomData,
    })
}

#[cfg(all(test, not(kernel)))]
mod tests {
    use super::super::{Bio, BioBuf, BioOwner, BioRange};
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
        let buf = BioBuf::<Bio>::new::<BchError, _>(3, core::ptr::null_mut(), 0, |bio| bio.init()).unwrap();
        for x in [&mut a[..], &mut b[..], &mut c_[..]] {
            // SAFETY: a buffer that outlives the bio, into one of its 3 bvecs
            unsafe { c::bio_add_virt_nofail(buf.bio().as_ptr(), x.as_mut_ptr().cast(), x.len() as _) };
        }
        let lens = |r: BioRange<'_>| r.into_iter().map(|s| s.len()).collect::<Vec<_>>();
        assert_eq!(lens(buf.range_all()), [8, 4, 6]);
        assert_eq!(buf.range_all().segments().map(|s| s.len).collect::<Vec<_>>(), [8, 4, 6]);

        let mut r = buf.bio().range();
        assert_eq!(r.len(), 18);
        r.advance(10);
        assert_eq!(lens(r), [2, 6]);

        let pos = r.pos();
        let mut out = [0u8; 8];
        assert_eq!(r.copy_to(&mut out), 8);
        assert_eq!(out, [2, 2, 3, 3, 3, 3, 3, 3]);
        // SAFETY: a position of this bio, just taken
        assert_eq!(unsafe { buf.bio().range_at(pos) }.copy_from(&[9; 3]), 3);

        let mut chunks = buf.range_all().into_iter();
        chunks.next();
        assert_eq!(chunks.remaining().len(), 10);

        let mut tail = buf.bio().range();
        tail.advance(16);
        tail.zero();

        // SAFETY: the bio's, not walked meanwhile
        unsafe { (*buf.bio().as_ptr()).bi_iter.bi_size = 0 };
        assert_eq!((lens(buf.bio().range()), lens(buf.range_all())), (vec![], vec![8, 4, 6]));

        // SAFETY: as above
        unsafe { (*buf.bio().as_ptr()).bi_opf = c::req_op_REQ_OP_DISCARD };
        assert_eq!(lens(buf.range_all()), []);

        drop(buf);
        assert_eq!((a, b, c_), ([1; 8], [2, 2, 9, 9], [9, 3, 3, 3, 0, 0]));
    }

    /// A mapping's bvecs are its own: dropped, the bio has none, so none
    /// outlives the buffer's borrow.
    #[test]
    fn mapping_ends_with_its_borrow() {
        let buf = [5u8; 512];
        let mut bio = BioBuf::<Bio>::new::<BchError, _>(1, core::ptr::null_mut(),
                                                        c::req_op_REQ_OP_WRITE, |b| b.init()).unwrap();
        let m = bio.map_write(&buf);
        assert_eq!(m.owner.range_all().len(), 512);
        drop(m);
        assert_eq!(bio.range_all().len(), 0);
    }
}
