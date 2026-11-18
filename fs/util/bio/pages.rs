// SPDX-License-Identifier: GPL-2.0

//! Pages a bio owns - bounce buffers, the read path's, the write path's and
//! the data update's - allocated into its bvecs, and freed from them. Each
//! allocation is at most the bounce pool's element size, PAGE_SIZE <<
//! PAGE_ALLOC_COSTLY_ORDER, and page aligned in the kernel, as the device's
//! DMA alignment isn't known.
//!
//! Freeing what may have come from the bounce pool, free_pages_pool(), tells
//! them apart by size: a bvec of exactly the element size goes back to the
//! pool. Pages that never did - a bio's that mustn't take from it - are
//! free_pages()'s.

use core::alloc::Layout;
use core::ptr::NonNull;

use crate::c;
use crate::locking::{Locked, Mutex};
use crate::new_lock;
use crate::errcode::BchError;
use crate::util::alloc::{flags, Alloc, AllocError, AllocGfp, Allocator, Flags};
use crate::os::kernel::PAGE_SIZE;
use crate::util::mempool::Mempool;
use pin_init::{pin_data, pin_init, PinInit};

use super::iter::page_address;

/// The bounce pool's element size
const POOL_LEN: usize = PAGE_SIZE << c::PAGE_ALLOC_COSTLY_ORDER;

/// The fallback that can't fail: enough elements for the largest btree node
/// or extent. They're handed out whole, to one allocator at a time, as two
/// each holding part of the pool would deadlock it.
#[pin_data]
pub struct BouncePool {
    #[pin]
    lock: Locked<Mutex>,
    #[pin]
    pool: Mempool<BouncePages>,
}

impl BouncePool {
    pub(crate) fn new(opts: &c::bch_opts) -> impl PinInit<Self, BchError> {
        let max = opts.btree_node_size.max(opts.encoded_extent_max);
        pin_init!(Self {
            lock <- new_lock!((), "&c->bio_bounce_pages_lock"),
            pool <- Mempool::new(BouncePages, max as usize / POOL_LEN, POOL_LEN),
        }? BchError)
    }

    /// Allocates the rest of @size into @bio, waiting for elements - in a
    /// NORECLAIM scope, where it can't, failing, with what it had allocated
    /// left on the bio. It hands out whole elements, so the bio's size is
    /// trimmed to @size after.
    pub fn alloc(&self, bio: &mut c::bio, size: usize) -> Result<(), AllocError> {
        let _lock = self.lock.lock();

        while (bio.bi_iter.bi_size as usize) < size {
            let p = self.pool.alloc_raw()?;
            unsafe { crate::os::bio::add_virt_nofail(bio, p.as_ptr().cast(), POOL_LEN as u32) };
        }
        bio.bi_iter.bi_size = bio.bi_iter.bi_size.min(size as u32);
        Ok(())
    }
}

/// The bounce pool's elements: data buffers, overwritten before they're
/// read, so not zeroed.
#[derive(Clone, Copy)]
pub struct BouncePages;

unsafe impl Allocator for BouncePages {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        self.allocate_gfp(layout, Flags::empty())
    }

    unsafe fn deallocate(&self, p: NonNull<u8>, layout: Layout) {
        unsafe { crate::util::alloc::bio_pages_free(p.as_ptr().cast(), layout.size()) }
    }
}

unsafe impl Alloc for BouncePages {}

unsafe impl AllocGfp for BouncePages {
    fn allocate_gfp(&self, layout: Layout, gfp: Flags) -> Result<NonNull<[u8]>, AllocError> {
        let p = NonNull::new(crate::util::alloc::bio_pages_alloc(layout.size(), gfp | flags::SKIP_ZERO)).ok_or(AllocError)?;
        Ok(NonNull::slice_from_raw_parts(p.cast(), layout.size()))
    }

    unsafe fn grow_gfp(&self, _: NonNull<u8>, _: Layout, _: Layout, _: Flags) -> Result<NonNull<[u8]>, AllocError> {
        Err(AllocError)
    }
}

/// Allocates pages into @bio up to @size bytes, each allocation a multiple of
/// @bs. On AllocError, what was allocated stays on the bio. The bio must have
/// the bvecs for them.
pub fn alloc_pages(bio: &mut c::bio, bs: u32, size: usize, gfp: Flags) -> Result<(), AllocError> {
    let bs = bs as usize;
    assert!(bs.is_power_of_two() && size % bs == 0, "allocating {size} bytes for a bio of {bs} byte blocks");

    while (bio.bi_iter.bi_size as usize) < size {
        let b = (size - bio.bi_iter.bi_size as usize).min(bs.max(POOL_LEN));
        assert!(b % bs == 0, "a bio's pages {b} bytes, not a multiple of its {bs} byte blocks");

        // SAFETY: no pointers in
        let p = NonNull::new(crate::util::alloc::bio_pages_alloc(b, gfp)).ok_or(AllocError)?;
        // SAFETY: a fresh allocation of @b bytes, the bio's now
        unsafe { crate::os::bio::add_virt_nofail(bio, p.as_ptr(), b as u32) };
    }
    Ok(())
}

/// Allocates @size bytes into @bio with alloc_pages(), and the rest, up to
/// @at_least, from @pool - failing as the pool does, what was allocated left
/// on the bio. The bio must have the bvecs for them.
pub fn alloc_pages_pool(pool: &BouncePool, bio: &mut c::bio, bs: u32, size: usize, at_least: usize, gfp: Flags)
    -> Result<(), AllocError>
{
    // short is fine: the pool has the rest
    let _ = alloc_pages(bio, bs, size, gfp);
    if (bio.bi_iter.bi_size as usize) < at_least {
        pool.alloc(bio, at_least)?;
    }
    Ok(())
}

/// The most bvecs @size takes, from alloc_pages() and the bounce pool: one an
/// allocation, and each but the last at least the pool's element size.
pub fn nr_vecs(size: usize) -> u16 {
    size.div_ceil(POOL_LEN).try_into().expect("a bio's pages in more than 64k allocations")
}

/// How many bytes of pages @bio has, alloc_pages()'s: its bvecs' lengths,
/// whatever size each allocation was.
pub fn bytes(bio: &c::bio) -> usize {
    (0..bio.bi_vcnt as usize)
        .map(|i| unsafe { (*bi_io_vec!(bio).add(i)).bv_len } as usize)
        .sum()
}

/// Frees what alloc_pages() and the bounce pool put on @bio.
///
/// # Safety
/// Each of @bio's bvecs is a whole allocation of one of them, and the bio's
/// done with: not in flight.
pub unsafe fn free_pages_pool(pool: &BouncePool, bio: &mut c::bio) {
    unsafe { free(Some(pool), bio) }
}

/// Frees what alloc_pages() put on @bio.
///
/// # Safety
/// Each of @bio's bvecs is a whole allocation of alloc_pages()'s, and the
/// bio's done with: not in flight.
pub unsafe fn free_pages(bio: &mut c::bio) {
    unsafe { free(None, bio) }
}

unsafe fn free(pool: Option<&BouncePool>, bio: &mut c::bio) {
    for i in 0..bio.bi_vcnt as usize {
        // SAFETY: one of its bvecs
        let bv = unsafe { *bi_io_vec!(bio).add(i) };
        // SAFETY: the caller's: an allocation of bv_len, at its page's address
        unsafe {
            let p = page_address(NonNull::new(bv.bv_page).expect("a bvec's page")).add(bv.bv_offset as usize);
            match pool {
                Some(pool) if bv.bv_len as usize == POOL_LEN => pool.pool.free_raw(p),
                _ => crate::util::alloc::bio_pages_free(p.as_ptr().cast(), bv.bv_len as usize),
            }
        }
    }
    bio.bi_vcnt = 0;
}
