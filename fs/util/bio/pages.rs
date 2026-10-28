// SPDX-License-Identifier: GPL-2.0

//! Pages a bio owns - the data update's and the write path's bounce buffers -
//! allocated into its bvecs, and freed from them. Each allocation is at most
//! the bounce pool's element size, PAGE_SIZE << PAGE_ALLOC_COSTLY_ORDER, and
//! page aligned in the kernel, as the device's DMA alignment isn't known.
//!
//! The bounce pool, c->bio_bounce_bufs, is the fallback that can't fail: its
//! elements are whole, and taken under bio_bounce_pages_lock, as two
//! allocators each holding part of the pool would deadlock it. Freeing tells
//! them apart by size: a bvec of exactly the element size goes back to the
//! pool.

use core::ffi::{c_int, c_uint};
use core::ptr::NonNull;

use crate::c;
use crate::fs::Fs;
use crate::locking::Lock;
use crate::util::alloc::AllocError;
use crate::util::ffi::Opaque;
use crate::util::kernel::PAGE_SIZE;
use cstruct_macros::c_export;

use super::iter::page_address;

/// The bounce pool's element size: read.h's BIO_BOUNCE_BUF_POOL_LEN.
const POOL_LEN: usize = PAGE_SIZE << c::PAGE_ALLOC_COSTLY_ORDER;

// SAFETY: a zeroed mempool is a valid uninitialized one, as in C's
// kvzalloc()'d bch_fs; bch2_fs_init() initializes it
unsafe impl ::pin_init::Zeroable for Opaque<c::mempool_t> {}

/// Allocates pages into @bio up to @size bytes, each allocation a multiple of
/// @bs. On AllocError, what was allocated stays on the bio. The bio must have
/// the bvecs for them.
pub fn alloc_pages(bio: &mut c::bio, bs: u32, size: usize, gfp: c::gfp_t) -> Result<(), AllocError> {
    let bs = bs as usize;
    assert!(bs.is_power_of_two() && size % bs == 0, "allocating {size} bytes for a bio of {bs} byte blocks");

    while (bio.bi_iter.bi_size as usize) < size {
        let b = (size - bio.bi_iter.bi_size as usize).min(bs.max(POOL_LEN));
        assert!(b % bs == 0, "a bio's pages {b} bytes, not a multiple of its {bs} byte blocks");

        // SAFETY: no pointers in
        let p = NonNull::new(unsafe { c::rust_bio_pages_alloc(b, gfp) }).ok_or(AllocError)?;
        // SAFETY: a fresh allocation of @b bytes, the bio's now
        unsafe { c::bio_add_virt_nofail(bio, p.as_ptr(), b as u32) };
    }
    Ok(())
}

/// Allocates the rest of @size from the bounce pool, which waits rather than
/// fails. It hands out whole elements, so the bio's size is trimmed to @size
/// after.
#[c_export]
pub fn __bch2_bio_alloc_pages_pool(c: &Opaque<c::bch_fs>, bio: &mut c::bio, _bs: c_uint, size: usize, gfp: c::gfp_t) {
    let fs = Fs::from_c(c);
    let _lock = Lock::from_c(&fs.bio_bounce_pages_lock).lock();

    while (bio.bi_iter.bi_size as usize) < size {
        // SAFETY: the filesystem's pool; an element is POOL_LEN bytes, the
        // bio's now
        unsafe {
            let p = c::mempool_alloc_noprof(fs.bio_bounce_bufs.as_ptr(), gfp);
            c::bio_add_virt_nofail(bio, p, POOL_LEN as u32);
        }
    }
    bio.bi_iter.bi_size = bio.bi_iter.bi_size.min(size as u32);
}

/// Allocates @size bytes into @bio with alloc_pages(), and the rest from the
/// bounce pool. The bio must have the bvecs for them.
#[c_export]
pub fn bch2_bio_alloc_pages_pool(c: &Opaque<c::bch_fs>, bio: &mut c::bio, bs: c_uint, size: usize, gfp: c::gfp_t) {
    // short is fine: the pool has the rest
    let _ = alloc_pages(bio, Fs::from_c(c).opts().block_size.into(), size, gfp);
    if (bio.bi_iter.bi_size as usize) < size {
        __bch2_bio_alloc_pages_pool(c, bio, bs, size, gfp);
    }
}

/// Frees what alloc_pages() and the bounce pool put on @bio.
///
/// # Safety
/// Each of @bio's bvecs is a whole allocation of one of them, and the bio's
/// done with: not in flight.
pub unsafe fn free_pages_pool(fs: &Fs, bio: &mut c::bio) {
    for i in 0..bio.bi_vcnt as usize {
        // SAFETY: one of its bvecs
        let bv = unsafe { *bio.bi_io_vec.add(i) };
        // SAFETY: the caller's: an allocation of bv_len, at its page's address
        unsafe {
            let p = page_address(NonNull::new(bv.bv_page).expect("a bvec's page"))
                .add(bv.bv_offset as usize).as_ptr().cast();
            if bv.bv_len as usize == POOL_LEN {
                c::mempool_free(p, fs.bio_bounce_bufs.as_ptr());
            } else {
                c::rust_bio_pages_free(p, bv.bv_len as usize);
            }
        }
    }
    bio.bi_vcnt = 0;
}

// ── For C ────────────────────────────────────────────────────────────────

#[c_export]
pub fn bch2_bio_alloc_pages(bio: &mut c::bio, bs: c_uint, size: usize, gfp: c::gfp_t) -> c_int {
    alloc_pages(bio, bs, size, gfp).map_or(-(c::ENOMEM as c_int), |()| 0)
}

/// # Safety
/// free_pages_pool()'s.
#[c_export]
pub unsafe fn bch2_bio_free_pages_pool(c: &Opaque<c::bch_fs>, bio: &mut c::bio) {
    // SAFETY: the caller's
    unsafe { free_pages_pool(Fs::from_c(c), bio) }
}
