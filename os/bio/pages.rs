// SPDX-License-Identifier: GPL-2.0

//! Pages a bio owns - bounce buffers, the read path's, the write path's and
//! the data update's - allocated into its bvecs, and freed from them. Each
//! allocation is at most the bounce pool's element size, PAGE_SIZE <<
//! PAGE_ALLOC_COSTLY_ORDER, and page aligned in the kernel, as the device's
//! DMA alignment isn't known.
//!
//! Freeing what may have come from the bounce pool, free_pages_pool(), tells
//! them apart by size: a bvec of exactly the element size goes back to the
//! pool, if it's given one.

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
use super::{reserve_bvecs, BioVecs};

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

    /// Allocates the rest of @size onto @vecs, waiting for elements - in a
    /// NORECLAIM scope, where it can't, failing, with what it had allocated
    /// left on them. It hands out whole elements, so they may come to more
    /// than @size. Whether it took any: its elements may be the reserve's, and
    /// one holding those mustn't ask again till they're freed.
    pub fn alloc(&self, vecs: &mut BioVecs, size: usize) -> Result<bool, BchError> {
        let mut have = vecs_len(vecs);
        let took = have < size;
        let _lock = self.lock.lock();

        while have < size {
            reserve_bvecs(vecs, 1)?;
            let p = self.pool.alloc_raw()?;
            push_alloc(vecs, p.as_ptr().cast(), POOL_LEN);
            have += POOL_LEN;
        }
        Ok(took)
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

pub(super) fn vecs_len(vecs: &BioVecs) -> usize {
    vecs.iter().map(|bv| bv.bv_len as usize).sum()
}

/// Allocates pages onto @vecs up to @size bytes, each allocation a multiple
/// of @bs. Failing, what was allocated stays on them.
pub fn alloc_pages(vecs: &mut BioVecs, bs: u32, size: usize, gfp: Flags) -> Result<(), BchError> {
    let bs = bs as usize;
    assert!(bs.is_power_of_two() && size % bs == 0, "allocating {size} bytes for a bio of {bs} byte blocks");

    let mut have = vecs_len(vecs);
    while have < size {
        // in userspace all at once, which fragments nothing: one buffer, for
        // MappedPages, which has no vmap there
        let b = if cfg!(kernel) { (size - have).min(bs.max(POOL_LEN)) } else { size - have };
        assert!(b % bs == 0, "a bio's pages {b} bytes, not a multiple of its {bs} byte blocks");

        reserve_bvecs(vecs, 1)?;
        let p = NonNull::new(crate::util::alloc::bio_pages_alloc(b, gfp)).ok_or(AllocError)?;
        push_alloc(vecs, p.as_ptr().cast(), b);
        have += b;
    }
    Ok(())
}

/// @len bytes at @p, a fresh allocation in the direct map - one bvec - pushed
/// where one was reserved.
fn push_alloc(vecs: &mut BioVecs, p: *mut u8, len: usize) {
    for bv in unsafe { crate::os::bio::sys::bvecs(p, len) } {
        vecs.push(bv).expect("pushing a bvec reserved");
    }
}

/// Allocates @size bytes onto @vecs with alloc_pages(), and the rest, up to
/// @at_least, from @pool - failing as the pool does, what was allocated left
/// on them. Whether it took from the pool, as BouncePool::alloc() says.
pub(super) fn alloc_pages_pool(pool: &BouncePool, vecs: &mut BioVecs, bs: u32, size: usize, at_least: usize, gfp: Flags)
    -> Result<bool, BchError>
{
    // short is fine: the pool has the rest
    let _ = alloc_pages(vecs, bs, size, gfp);
    match vecs_len(vecs) < at_least {
        true  => pool.alloc(vecs, at_least),
        false => Ok(false),
    }
}

/// The most bvecs @size takes, from alloc_pages() and the bounce pool: one an
/// allocation, and each but the last at least the pool's element size.
pub fn nr_vecs(size: usize) -> u16 {
    size.div_ceil(POOL_LEN).try_into().expect("a bio's pages in more than 64k allocations")
}

/// The most bytes nr_vecs() fits in @nr_vecs bvecs.
pub fn max_size(nr_vecs: u16) -> usize {
    usize::from(nr_vecs) * POOL_LEN
}

/// Frees what alloc_pages() and the bounce pool, if @pool, put on @vecs, and
/// empties them.
///
/// # Safety
/// Each of @vecs is a whole allocation of one of them, done with: no bio of
/// them in flight.
pub(super) unsafe fn free_pages_pool(pool: Option<&BouncePool>, vecs: &mut BioVecs) {
    for (i, bv) in vecs.iter().enumerate() {
        // at offset 0 in the kernel, whole pages, at most the pool's element;
        // in userspace an allocation is anywhere in its first page, and all
        // of what alloc_pages() was asked for
        assert!((cfg!(not(kernel)) || (bv.bv_offset == 0 && bv.bv_len as usize <= POOL_LEN)) && bv.bv_len != 0,
                "freeing bounce bvec {i} of {}: page {:p} +{}, {} bytes - not one of its allocations",
                vecs.len(), bv.bv_page, bv.bv_offset, bv.bv_len);
    }
    for bv in vecs.iter() {
        // SAFETY: the caller's: an allocation of bv_len, at its page's address
        unsafe {
            let p = page_address(NonNull::new(bv.bv_page).expect("a bvec's page")).add(bv.bv_offset as usize);
            match pool.filter(|_| bv.bv_len as usize == POOL_LEN) {
                Some(pool) => pool.pool.free_raw(p),
                None       => crate::util::alloc::bio_pages_free(p.as_ptr().cast(), bv.bv_len as usize),
            }
        }
    }
    vecs.clear();
}
