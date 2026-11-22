// SPDX-License-Identifier: GPL-2.0

//! What a bio's bvecs point at: got as the bio is made, given back as it's
//! destroyed - a bio's buffers are its contents, and live as long as it does.
//!
//! The bvecs are the record of what was got, and release() walks them: a
//! Buffers is mostly a type, saying how, and its value what giving them back
//! needs - the bounce pool, or a mapped buffer itself.

use core::mem::MaybeUninit;
use core::ops::Deref;
use core::ptr;

use super::pages::{self, BouncePool};
use super::{buf_nr_bvecs, Bio, BioInPlace};
use crate::c;
use crate::errcode::BchError;
use crate::os::kernel::PAGE_SIZE;
use crate::util::alloc::{AlignedBox, AllocError, Flags};

/// # Safety
/// fill() puts in the bvecs only what release() gives back - or, failing,
/// gives back what it put there itself; nothing else changes the bvecs until
/// release().
pub unsafe trait Buffers: Sized {
    type Src<'s>;

    fn nr_vecs(src: &Self::Src<'_>) -> u16;

    /// Into the bvecs of @bio, fresh, with nr_vecs() of them.
    fn fill(bio: &mut Bio, src: Self::Src<'_>) -> Result<Self, BchError>;

    /// As the bio is destroyed: done with, and not in flight.
    fn release(&mut self, bio: &mut Bio);

    /// Whether an IO of @op may go to or from them: they last the bio's life,
    /// through each reset() for another IO.
    fn allows_op(_op: u32) -> bool {
        true
    }
}

/// Bvecs its owner fills and empties by hand, as every bio's were before
/// Buffers: @Src is how many. XXX: each user goes to a Buffers of its own.
#[derive(Default)]
pub struct BioBuffersUnmanaged;

unsafe impl Buffers for BioBuffersUnmanaged {
    type Src<'s> = u16;

    fn nr_vecs(nr: &u16) -> u16 {
        *nr
    }

    fn fill(_: &mut Bio, _: u16) -> Result<Self, BchError> {
        Ok(Self)
    }

    fn release(&mut self, _: &mut Bio) {}
}

/// Pages of the bio's own, from the page allocator - or what it can't give
/// from the bounce pool, which waits rather than fails, but in a NORECLAIM
/// scope. @Src is the pool, the block size each allocation is a multiple of,
/// the size, how much of it the pool makes up - all of it, or for a bio that
/// can do with less, a write's, that much - and how to allocate.
pub struct BioBuffersOwned<'p> {
    pool:      &'p BouncePool,
    used_pool: bool,
}

unsafe impl<'p> Buffers for BioBuffersOwned<'p> {
    type Src<'s> = (&'p BouncePool, u32, usize, usize, Flags);

    fn nr_vecs(&(_, _, size, _, _): &Self::Src<'_>) -> u16 {
        pages::nr_vecs(size)
    }

    fn fill(bio: &mut Bio, (pool, bs, size, at_least, gfp): Self::Src<'_>) -> Result<Self, BchError> {
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

    fn release(&mut self, bio: &mut Bio) {
        unsafe { pages::free_pages_pool(self.pool, &mut bio.vecs) };
        bio.sync();
    }
}

impl<'p> BioBuffersOwned<'p> {
    /// The pages fill() put on a raw bounce bio, which kept no
    /// BioBuffersOwned of them, or Bio::init_pages()'s: for free_from().
    ///
    /// # Safety
    /// The bio it's freed from has fill()'s bvecs, from @pool, or
    /// init_pages()'s.
    pub unsafe fn from_raw(pool: &'p BouncePool) -> Self {
        Self { pool, used_pool: false }
    }

    /// What it holds freed, from @bio, which they were put on beside it, not
    /// owned by it: a fragment task's home's.
    pub fn free_from(mut self, bio: &mut Bio) {
        self.release(bio);
        bio.raw_mut().bi_iter.bi_size = 0;
    }

    /// Whether the pool made some of it up: its elements may be its reserve,
    /// and a bio holding those mustn't ask for more till they're freed.
    pub fn used_pool(&self) -> bool {
        self.used_pool
    }
}

impl Bio {
    /// bio_init() of a Bio at @bio with @size bytes of pages, from the page
    /// allocator alone, each allocation a multiple of @bs - failing as it
    /// does, with nothing made: a data update's, whose read and write bios
    /// borrow its bvecs, and which frees its pages from it as @pool's.
    ///
    /// # Safety
    /// @bio is room for one, not in flight.
    pub unsafe fn init_pages(bio: *mut Bio, pool: &BouncePool, bs: u32, size: usize, gfp: Flags)
        -> Result<(), AllocError>
    {
        unsafe {
            BioInPlace::own_at(bio.cast(), ptr::null_mut(), pages::nr_vecs(size), 0)?;
            let b = &mut *bio;
            if let Err(e) = pages::alloc_pages(&mut b.vecs, bs, size, gfp) {
                pages::free_pages_pool(pool, &mut b.vecs);
                ptr::drop_in_place(b);
                return Err(e);
            }
            b.sync();
            b.raw_mut().bi_iter.bi_size = size as u32;
        }
        Ok(())
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

/// The bio's buffer, @B: mapped as the bio's made, and held as long as it
/// lives - which BioBox::buf() reaches.
pub struct BioBuffersMapped<B: MapBuf>(pub(super) B);

unsafe impl<B: MapBuf> Buffers for BioBuffersMapped<B> {
    type Src<'s> = B;

    fn nr_vecs(buf: &B) -> u16 {
        let (p, len) = buf.as_raw();
        buf_nr_bvecs(p, len).try_into().expect("a mapped buffer of more than 64k bvecs")
    }

    /// Panics if the bio's op isn't one B allows.
    fn fill(bio: &mut Bio, buf: B) -> Result<Self, BchError> {
        assert!(B::allows_op(bio.op()), "mapping a buffer into a bio for an op it can't serve");
        let (p, len) = buf.as_raw();
        unsafe { bio.map(p, len) };
        Ok(Self(buf))
    }

    fn release(&mut self, _: &mut Bio) {}

    fn allows_op(op: u32) -> bool {
        B::allows_op(op)
    }
}
