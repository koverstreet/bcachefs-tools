// SPDX-License-Identifier: GPL-2.0

//! A bio's completion, its reset for another IO, and memory mapped into its
//! bvecs.

use crate::c;
use crate::os::kernel::{is_vmalloc_addr, PAGE_SIZE};

/// # Safety
/// @bio is a bio whose IO is done: nothing else completes it.
pub unsafe fn endio(bio: *mut c::bio) {
    unsafe { c::bio_endio(bio) }
}

/// # Safety
/// @bio is a bio.
pub unsafe fn inc_remaining(bio: *mut c::bio) {
    unsafe { c::bio_inc_remaining(bio) }
}

/// # Safety
/// @bio is a bio, not used after.
pub unsafe fn uninit(bio: *mut c::bio) {
    unsafe { c::bio_uninit(bio) }
}

/// # Safety
/// @bio is a bio's memory, not in flight; @table has room for @max_vecs.
pub unsafe fn init(bio: *mut c::bio, bdev: *mut crate::os::blkdev::BlockDevice, table: *mut c::bio_vec,
                   max_vecs: u16, opf: u32) {
    unsafe { c::bio_init(bio, bdev, table, max_vecs, opf) }
}

/// # Safety
/// @bio is a bio, not in flight.
pub unsafe fn reset(bio: *mut c::bio, bdev: *mut crate::os::blkdev::BlockDevice, opf: u32) {
    unsafe { c::bio_reset(bio, bdev, opf) }
}

/// The bvecs of @len bytes at @buf: one in the direct map, its folio's and
/// the offset into it, as bio_add_folio() makes them; a page's each in
/// vmalloc memory, which isn't contiguous in pages.
///
/// # Safety
/// @buf is the allocators' - the direct map, or vmalloc - @len bytes of it.
pub unsafe fn bvecs(buf: *mut u8, len: usize) -> impl Iterator<Item = c::bio_vec> {
    let vmalloc = is_vmalloc_addr(buf);
    let (mut p, mut left) = (buf, len);
    core::iter::from_fn(move || {
        if left == 0 {
            return None;
        }
        let n = match vmalloc {
            true  => (PAGE_SIZE - (p.addr() & (PAGE_SIZE - 1))).min(left),
            false => left,
        };
        let len = u32::try_from(n).expect("a bvec of 4G or more");
        let bv = unsafe {
            match vmalloc {
                true => c::bio_vec {
                    bv_page:   c::vmalloc_to_page(p.cast()),
                    bv_len:    len,
                    bv_offset: (p.addr() & (PAGE_SIZE - 1)) as u32,
                },
                false => {
                    let folio = c::virt_to_folio(p.cast());
                    let offset = p.byte_offset_from(c::folio_address(folio).cast::<u8>());
                    c::bio_vec { bv_page: folio.cast(), bv_len: len, bv_offset: offset as u32 }
                }
            }
        };
        (p, left) = (p.wrapping_add(n), left - n);
        Some(bv)
    })
}

/// How many bvecs mapping @len bytes at @p takes: a page each for vmalloc
/// memory, one for the direct map.
pub fn buf_nr_bvecs(p: *const u8, len: usize) -> usize {
    if is_vmalloc_addr(p) {
        return (len + (p as usize & (PAGE_SIZE - 1))).div_ceil(PAGE_SIZE);
    }
    1
}

/// How much of @len bytes at @p one bio maps, in at most BIO_MAX_VECS bvecs:
/// all of them, but vmalloc memory spanning more pages - where @p is in its
/// first page says how many fit.
pub fn buf_bio_bytes(p: *const u8, len: usize) -> usize {
    if is_vmalloc_addr(p) {
        return len.min(c::BIO_MAX_VECS as usize * PAGE_SIZE - (p as usize & (PAGE_SIZE - 1)));
    }
    len
}
