// SPDX-License-Identifier: GPL-2.0

//! What a bio owns of what its bvecs point at, as its B: pages of its own, or
//! a mapped buffer.

use core::mem::MaybeUninit;
use core::ops::Deref;
use core::ptr;

use super::pages::{self, BouncePool};
use super::{buf_nr_bvecs, Bio, BioInPlace, BioOwns, BioVecs};
use crate::c;
use crate::errcode::BchError;
use crate::os::kernel::PAGE_SIZE;
use crate::util::alloc::{AlignedBox, AllocError, Flags};

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
        let ret = pages::alloc_pages_pool(pool, &mut bio.vecs, bs, size, at_least, gfp);
        if ret.is_err() {
            unsafe { pages::free_pages_pool(pool, &mut bio.vecs) };
        }
        bio.sync();
        let used_pool = ret?;
        /* the pool's whole elements may come to more */
        let covers = pages::vecs_len(&bio.vecs).min(if used_pool { at_least } else { size });
        bio.raw_mut().bi_iter.bi_size = covers as u32;
        Ok(Self { pool, used_pool })
    }

    /// The pages fill() put on a raw bounce bio, which kept no Pages of them:
    /// for free_from().
    ///
    /// # Safety
    /// The bio it's freed from has fill()'s bvecs, from @pool.
    pub unsafe fn from_raw(pool: &'p BouncePool) -> Self {
        Self { pool, used_pool: false }
    }

    /// What it holds freed, from @bio, which they were put on beside it, not
    /// owned by it: a fragment task's home's.
    pub fn free_from(self, bio: &mut Bio) {
        unsafe { pages::free_pages_pool(self.pool, &mut bio.vecs) };
        bio.sync();
        bio.raw_mut().bi_iter.bi_size = 0;
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
        unsafe { pages::free_pages_pool(self.pool, vecs) }
    }
}

impl<'p> Bio<Pages<'p>> {
    /// bio_init() of a Bio at @bio owning @size bytes of pages, from the page
    /// allocator alone, each allocation a multiple of @bs - failing as it
    /// does, with nothing made: a data update's, whose read and write bios
    /// borrow its bvecs.
    ///
    /// # Safety
    /// @bio is room for one, not in flight.
    pub unsafe fn init_pages(bio: *mut Self, pool: &'p BouncePool, bs: u32, size: usize, gfp: Flags)
        -> Result<(), AllocError>
    {
        unsafe {
            BioInPlace::own_at(bio.cast(), ptr::null_mut(), pages::nr_vecs(size), 0)?;
            let b = &mut *bio.cast::<Bio>();
            if let Err(e) = pages::alloc_pages(&mut b.vecs, bs, size, gfp) {
                pages::free_pages_pool(pool, &mut b.vecs);
                ptr::drop_in_place(b);
                return Err(e);
            }
            b.sync();
            b.raw_mut().bi_iter.bi_size = size as u32;
            (&raw mut (*bio).owns).write(Pages { pool, used_pool: false });
        }
        Ok(())
    }

    /// Its pages freed now, before it's dropped: a data update's, once its
    /// write's data is written, before the index update.
    pub fn free_pages(&mut self) {
        unsafe { pages::free_pages_pool(self.owns.pool, &mut self.vecs) };
        self.sync();
        self.raw_mut().bi_iter.bi_size = 0;
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

unsafe impl<const A: usize> MapBuf for AlignedBox<[MaybeUninit<u8>], A> {
    fn as_raw(&self) -> (*mut u8, usize) {
        (self.as_ptr().cast::<u8>().cast_mut(), self.len())
    }
}

/// A buffer for IO: page aligned, as userspace reads into it O_DIRECT. Not
/// initialized: what reads into it, or writes it, has its bytes.
pub type IoBuf = AlignedBox<[MaybeUninit<u8>], PAGE_SIZE>;

/// An IoBuf of @bytes, with @gfp, not zeroed: as kvmalloc() leaves them, as
/// C's buffers were.
pub fn io_buf(bytes: usize, gfp: Flags) -> Result<IoBuf, AllocError> {
    AlignedBox::try_new_uninit_slice_gfp(bytes, gfp)
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
        let (p, len) = buf.as_raw();
        unsafe { bio.map(p, len) };
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
