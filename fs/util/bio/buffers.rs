// SPDX-License-Identifier: GPL-2.0

//! What a bio's bvecs point at: got as the bio is made, given back as it's
//! destroyed - a bio's buffers are its contents, and live as long as it does.
//!
//! The bvecs are the record of what was got, and release() walks them: a
//! Buffers is mostly a type, saying how, and its value what giving them back
//! needs - the bounce pool, a borrow's lifetime, or a mapped buffer itself.

use core::marker::PhantomData;
use core::ops::Deref;
use core::pin::Pin;

use super::pages::{self, BouncePool};
use super::{buf_nr_bvecs, Bio, BioRange};
use crate::c;
use crate::errcode::BchError;
use crate::util::alloc::{ABox, ConstAlign};

/// # Safety
/// fill() puts in the bvecs only what release() gives back - or, failing,
/// gives back what it put there itself; nothing else changes the bvecs until
/// release().
pub unsafe trait Buffers: Sized {
    type Src<'s>;

    fn nr_vecs(src: &Self::Src<'_>) -> u16;

    /// Into the bvecs of @bio, fresh, with nr_vecs() of them.
    fn fill(bio: Pin<&mut Bio>, src: Self::Src<'_>) -> Result<Self, BchError>;

    /// As the bio is destroyed: done with, and not in flight.
    fn release(&mut self, bio: Pin<&mut Bio>);

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

    fn fill(_: Pin<&mut Bio>, _: u16) -> Result<Self, BchError> {
        Ok(Self)
    }

    fn release(&mut self, _: Pin<&mut Bio>) {}
}

/// Pages of the bio's own, from the page allocator - or what it can't give
/// from the bounce pool, which waits rather than fails, but in a NORECLAIM
/// scope. @Src is the pool, the
/// block size each allocation is a multiple of, the size, and how to
/// allocate.
pub struct BioBuffersOwned<'p> {
    pool: &'p BouncePool,
}

unsafe impl<'p> Buffers for BioBuffersOwned<'p> {
    type Src<'s> = (&'p BouncePool, u32, usize, c::gfp_t);

    fn nr_vecs(&(_, _, size, _): &Self::Src<'_>) -> u16 {
        pages::nr_vecs(size)
    }

    fn fill(bio: Pin<&mut Bio>, (pool, bs, size, gfp): Self::Src<'_>) -> Result<Self, BchError> {
        let bio = bio.raw_mut();
        if let Err(e) = pages::alloc_pages_pool(pool, bio, bs, size, gfp) {
            unsafe { pages::free_pages_pool(pool, bio) };
            return Err(e.into());
        }
        Ok(Self { pool })
    }

    fn release(&mut self, bio: Pin<&mut Bio>) {
        unsafe { pages::free_pages_pool(self.pool, bio.raw_mut()) }
    }
}

/// Pages of the bio's own, from the page allocator alone: failing, where
/// BioBuffersOwned waits on the bounce pool, as a bio that holds them longer
/// than an IO - the journal's buffers - mustn't take from it. @Src as Owned's,
/// but the pool.
pub struct BioBuffersPages;

unsafe impl Buffers for BioBuffersPages {
    type Src<'s> = (u32, usize, c::gfp_t);

    fn nr_vecs(&(_, size, _): &Self::Src<'_>) -> u16 {
        pages::nr_vecs(size)
    }

    fn fill(bio: Pin<&mut Bio>, (bs, size, gfp): Self::Src<'_>) -> Result<Self, BchError> {
        let bio = bio.raw_mut();
        if pages::alloc_pages(bio, bs, size, gfp).is_err() {
            unsafe { pages::free_pages(bio) };
            return Err(c::ENOMEM.into());
        }
        Ok(Self)
    }

    fn release(&mut self, bio: Pin<&mut Bio>) {
        unsafe { pages::free_pages(bio.raw_mut()) }
    }
}

/// A clone of @Src's bio, at @Src: its bvecs, and the data, are the source's,
/// borrowed as long as the clone lives.
///
/// fill() is __bio_clone(), which isn't exported - but for its device's half,
/// as a clone's made for no device and bio_set_dev() associates the blkg, and
/// the crypt context and integrity payload a source has only once submitted.
pub struct BioClone<'a>(PhantomData<BioRange<'a>>);

unsafe impl<'a> Buffers for BioClone<'a> {
    type Src<'s> = BioRange<'a>;

    fn nr_vecs(_: &BioRange<'a>) -> u16 {
        0
    }

    fn fill(bio: Pin<&mut Bio>, src: BioRange<'a>) -> Result<Self, BchError> {
        unsafe { clone_from(bio.raw_mut(), src.bio().raw(), src.pos()) };
        Ok(Self(PhantomData))
    }

    fn release(&mut self, _: Pin<&mut Bio>) {}
}

/// __bio_clone(), of @s from @iter, into a bio of no bvecs and no device.
///
/// # Safety
/// @s's bvecs outlive @b.
pub(super) unsafe fn clone_from(b: &mut c::bio, s: &c::bio, iter: c::bvec_iter) {
    assert!(b.bi_bdev.is_null(), "a clone made for a device: it's bio_set_dev()'s, after");
    b.bi_flags |= 1 << c::BIO_CLONED;
    b.bi_ioprio = s.bi_ioprio;
    clone_hints(b, s);
    b.bi_iter = iter;
    bi_io_vec!(clone b, s);
}

/// The rest of __bio_clone()'s copies, as each kernel's struct bio has them;
/// the shim's has none.
#[cfg(kernel)]
fn clone_hints(b: &mut c::bio, s: &c::bio) {
    b.bi_write_hint = s.bi_write_hint;
    #[cfg(linux_ge = "6.16")]
    { b.bi_write_stream = s.bi_write_stream; }
    #[cfg(linux_ge = "6.19")]
    { b.bi_bvec_gap_bit = s.bi_bvec_gap_bit; }
}

#[cfg(not(kernel))]
fn clone_hints(_: &mut c::bio, _: &c::bio) {}

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

unsafe impl<const A: usize> MapBuf for ABox<[u8], ConstAlign<A>> {
    fn as_raw(&self) -> (*mut u8, usize) {
        (self.as_ptr().cast_mut(), self.len())
    }
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
    fn fill(bio: Pin<&mut Bio>, buf: B) -> Result<Self, BchError> {
        assert!(B::allows_op(bio.op()), "mapping a buffer into a bio for an op it can't serve");
        let (p, len) = buf.as_raw();
        unsafe { bio.map(p, len) };
        Ok(Self(buf))
    }

    fn release(&mut self, _: Pin<&mut Bio>) {}

    fn allows_op(op: u32) -> bool {
        B::allows_op(op)
    }
}
