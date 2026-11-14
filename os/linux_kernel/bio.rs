// SPDX-License-Identifier: GPL-2.0

//! A bio's completion, its reset for another IO, and memory mapped into its
//! bvecs.

use crate::c;
use core::ffi::c_void;

/// # Safety
/// @bio is a bio whose IO is done: nothing else completes it.
pub unsafe fn endio(bio: *mut c::bio) {
    unsafe { c::bio_endio(bio) }
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

/// # Safety
/// @bio is a bio, not in flight, with a bvec free; @vaddr is in the direct map,
/// @len bytes of it in one bvec, and outlives its place there.
pub unsafe fn add_virt_nofail(bio: *mut c::bio, vaddr: *mut c_void, len: u32) {
    unsafe { c::bio_add_virt_nofail(bio, vaddr, len) }
}
