// SPDX-License-Identifier: GPL-2.0

//! What a bio owns of what its bvecs point at, as its B: pages of its own, or
//! a mapped buffer.

use core::ops::Deref;
use core::ptr::NonNull;

use pin_init::PinInit;
use yoke::Yoke;

use super::pages::{self, BouncePool};
use super::{buf_nr_bvecs, Bio, BioBox, BioInPlace, BioOwns, BioVecs, HasBio, Mapped};
use crate::c;
use crate::errcode::BchError;
use crate::os::kernel::PAGE_SIZE;
use crate::util::alloc::{flags, AlignedBox, AllocError, Flags};

/// Pages of the bio's own, from the page allocator - or what it can't give
/// from the bounce pool, which waits rather than fails, but in a NORECLAIM
/// scope.
pub struct Pages<'p> {
    pool:      &'p BouncePool,
    used_pool: bool,
}

impl<'p> Pages<'p> {
    /// @size bytes of them, in the bvecs of @bio - fresh, with room for
    /// pages::nr_vecs(@size): each allocation a multiple of @bs, with @gfp,
    /// and the pool making up all of it, or for a bio that can do with less,
    /// a write's, @at_least. Failing, it has none.
    pub fn fill(bio: &mut Bio, pool: &'p BouncePool, bs: u32, size: usize, at_least: usize, gfp: Flags)
        -> Result<Self, BchError>
    {
        assert!(bio.vecs.is_empty(), "bouncing into a bio with {} bvecs already: freed as pages, after", bio.vecs.len());
        let ret = pages::alloc_pages_pool(pool, &mut bio.vecs, bs, size, at_least, gfp);
        if ret.is_err() {
            unsafe { pages::free_pages_pool(Some(pool), &mut bio.vecs) };
        }
        /* the pool's whole elements may come to more */
        let covers = pages::vecs_len(&bio.vecs).min(if matches!(ret, Ok(true)) { at_least } else { size });
        bio.sync(covers as u32);
        Ok(Self { pool, used_pool: ret? })
    }

    /// What it holds freed, from @bio, which they were put on beside it, not
    /// owned by it: a fragment task's home's.
    pub fn free_from(self, bio: &mut Bio) {
        unsafe { pages::free_pages_pool(Some(self.pool), &mut bio.vecs) };
        bio.sync(0);
    }

    /// Whether the pool made some of it up: its elements may be its reserve,
    /// and a bio holding those mustn't ask for more till they're freed.
    pub fn used_pool(&self) -> bool {
        self.used_pool
    }
}

impl BioOwns for Pages<'_> {
    type Bvec = c::bio_vec;

    fn release(&mut self, vecs: &mut BioVecs) {
        unsafe { pages::free_pages_pool(Some(self.pool), vecs) }
    }
}

impl<'p> Bio<Pages<'p>> {
    /// A Bio owning @size bytes of pages, from the page allocator alone, each
    /// allocation a multiple of @bs - failing as it does, with nothing made: a
    /// data update's, whose write is of it.
    pub fn new(pool: &'p BouncePool, bs: u32, size: usize, gfp: Flags) -> impl PinInit<Self, BchError> + 'p {
        BioInPlace::own(pages::nr_vecs(size), 0, move |at| at.init_from(move |b| {
            if let Err(e) = pages::alloc_pages(&mut b.vecs, bs, size, gfp) {
                unsafe { pages::free_pages_pool(Some(pool), &mut b.vecs) };
                return Err(e);
            }
            b.sync(size as u32);
            Ok(Pages { pool, used_pool: false })
        }))
    }

    /// Its pages freed now, before it's dropped: a data update's, once its
    /// write's data is written, before the index update.
    pub fn free_pages(&mut self) {
        unsafe { pages::free_pages_pool(Some(self.owns.pool), &mut self.vecs) };
        self.sync(0);
    }
}

/// A bio's own pages, from the page allocator - in high order allocations,
/// so a few bvecs cover a lot - and the same pages as one buffer for the CPU:
/// vmapped, where they aren't one already. For a buffer the CPU fills and a
/// bio's clones write - a journal entry's: written through &mut, so never
/// while a clone of the bio has it.
pub struct MappedPages(Yoke<Mapped<'static>, BioBox<'static, Bio<AllocPages>>>);

// its mapping is of its own pages, the vmap or where they are: they go where
// it goes
unsafe impl Send for MappedPages {}

impl crate::data_structures::dst_room::Storage for MappedPages {
    type Elem = u64;

    fn elems(&self) -> &[u64] {
        self.as_slice()
    }

    fn elems_mut(&mut self) -> &mut [u64] {
        self.as_mut_slice()
    }
}

/// Pages of the bio's own from alloc_pages() alone, none the bounce pool's:
/// each freed as it was allocated.
pub struct AllocPages;

impl BioOwns for AllocPages {
    type Bvec = c::bio_vec;

    fn release(&mut self, vecs: &mut BioVecs) {
        unsafe { pages::free_pages_pool(None, vecs) }
    }
}

impl MappedPages {
    /// @size bytes of them, zeroed - failing as the page allocator does, or
    /// vmap() does.
    pub fn new(size: usize, gfp: Flags) -> Result<Self, BchError> {
        let gfp = gfp | flags::ZERO;
        let bio = BioBox::<Bio<AllocPages>>::new(pages::nr_vecs(size), 0, |at: BioInPlace<'static>| {
            at.init_from(move |b| {
                if let Err(e) = pages::alloc_pages(&mut b.vecs, PAGE_SIZE as u32, size, gfp) {
                    unsafe { pages::free_pages_pool(None, &mut b.vecs) };
                    return Err(e);
                }
                b.sync(size as u32);
                Ok(AllocPages)
            })
        })?;

        let m = Yoke::<Mapped<'static>, _>::try_attach_to_cart(bio, |b: &Bio<AllocPages>| {
            HasBio::bio(b).range().map().ok_or(BchError::from(c::Errcode::ENOMEM_bio_buffers_map))
        })?;
        #[cfg(kernel)]
        if let Mapped::Vmap { offset, .. } = m.get() {
            assert!(*offset == 0, "alloc_pages()'s pages, vmapped {offset} bytes into the first");
        }
        Ok(MappedPages(m))
    }

    /// Where it's mapped as one: the vmap, or where its pages already are.
    fn data(&self) -> NonNull<u8> {
        NonNull::new(self.0.get().data().as_ptr()).expect("a mapping at NULL")
    }

    pub fn len(&self) -> usize {
        self.0.get().data().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// As u64s: whole pages, page aligned.
    pub fn as_slice(&self) -> &[u64] {
        // SAFETY: our pages, mapped at data() while we live; what IO has them
        // - a clone of the bio, borrowing it - writes them out, reading them
        unsafe { core::slice::from_raw_parts(self.data().as_ptr().cast(), self.len() / size_of::<u64>()) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u64] {
        // SAFETY: as as_slice()'s, borrowed mutably: no clone of the bio has
        // them
        unsafe { core::slice::from_raw_parts_mut(self.data().as_ptr().cast(), self.len() / size_of::<u64>()) }
    }

    /// Its pages, by pointer: not a borrow of them, which what's had through
    /// it may be parts of, each its own holder's.
    pub fn as_non_null(&self) -> NonNull<[u8]> {
        NonNull::slice_from_raw_parts(self.data(), self.len())
    }

    /// Its bio, over all of it: to write some of it from, by clones that
    /// borrow it - a write's, which only read the pages.
    pub(crate) fn bio(&self) -> &Bio {
        HasBio::bio(&**self.0.backing_cart())
    }
}

/// A buffer the bio's IO goes to or from, which the bio holds as long as it
/// lives: borrowed - &mut [u8], for reads, which the device writes, and
/// writes; & for writes alone - or owned, and freed with it. The bio's owner
/// reaches it as its Target.
///
/// # Safety
/// The memory as_raw() names is the buffer's while it lives: nothing done
/// through its Target - &mut, if it's DerefMut - moves or frees it.
pub unsafe trait MapBuf: Deref {
    fn allows_op(op: u32) -> bool {
        op == c::req_op_REQ_OP_READ || op == c::req_op_REQ_OP_WRITE
    }

    fn as_raw(&self) -> (*mut u8, usize);
}

unsafe impl MapBuf for &mut [u8] {
    fn as_raw(&self) -> (*mut u8, usize) {
        (self.as_ptr().cast_mut(), self.len())
    }
}

unsafe impl MapBuf for &[u8] {
    fn allows_op(op: u32) -> bool {
        op == c::req_op_REQ_OP_WRITE
    }

    fn as_raw(&self) -> (*mut u8, usize) {
        (self.as_ptr().cast_mut(), self.len())
    }
}

unsafe impl<const A: usize> MapBuf for AlignedBox<[u8], A> {
    fn as_raw(&self) -> (*mut u8, usize) {
        (self.as_ptr().cast_mut(), self.len())
    }
}

/// What a buffer IO is done to is aligned to: a page, as userspace reads into
/// it O_DIRECT.
pub const IO_ALIGN: usize = PAGE_SIZE;

/// A buffer for IO: zeroed until a read, or a writer, fills it - so what
/// isn't, a read that failed, is bytes all the same.
pub type IoBuf = AlignedBox<[u8], IO_ALIGN>;

/// An IoBuf of @bytes, with @gfp, zeroed.
pub fn io_buf(bytes: usize, gfp: Flags) -> Result<IoBuf, AllocError> {
    AlignedBox::try_new_zeroed_slice_gfp(bytes, gfp)
}

/// The bio's buffer, @M: mapped as the bio's made, and held as long as it
/// lives - which Bio::buf() reaches.
pub struct Buffer<M: MapBuf>(M);

impl<M: MapBuf> Buffer<M> {
    pub fn nr_vecs(buf: &M) -> u16 {
        let (p, len) = buf.as_raw();
        buf_nr_bvecs(p, len).try_into().expect("a mapped buffer of more than 64k bvecs")
    }

    /// @buf mapped, into the bvecs of @bio - fresh, with nr_vecs() of them.
    /// Panics if the bio's op isn't one M allows.
    pub fn fill(bio: &mut Bio, buf: M) -> Self {
        assert!(M::allows_op(bio.op()), "mapping a buffer into a bio for an op it can't serve");
        assert!(bio.vecs.is_empty(), "mapping a buffer into a bio with {} bvecs already", bio.vecs.len());
        let (p, len) = buf.as_raw();
        unsafe { bio.map(p, len).expect("a buffer, into a fresh bio made with room for its nr_vecs()") };
        Self(buf)
    }
}

impl<M: MapBuf> BioOwns for Buffer<M> {
    type Bvec = c::bio_vec;

    fn allows_op(op: u32) -> bool {
        M::allows_op(op)
    }
}

impl<M: MapBuf> Bio<Buffer<M>> {
    pub fn buf(&self) -> &M::Target {
        &self.owns.0
    }

    pub fn buf_mut(&mut self) -> &mut M::Target
    where
        M: core::ops::DerefMut,
    {
        &mut self.owns.0
    }
}

impl super::Typed for Pages<'_> {}
impl<M: MapBuf> super::Typed for Buffer<M> {}
