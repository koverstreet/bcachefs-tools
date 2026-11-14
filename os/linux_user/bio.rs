// SPDX-License-Identifier: GPL-2.0

//! A bio's completion, its reset for another IO, and memory mapped into its
//! bvecs, for userspace: block/bio.c's.

use crate::c;
use crate::os::kernel::PAGE_SIZE;
use core::ffi::c_void;
use core::mem::offset_of;
use core::sync::atomic::{AtomicI32, Ordering::{AcqRel, SeqCst}};

/*
 * As the kernel's, so Rust reads one the same in both builds. A page is its
 * address here, so a bio_vec of contiguous memory may span many of them.
 */
#[repr(C)]
#[derive(Debug, Copy, Clone, ::pin_init::MaybeZeroable)]
pub struct bio_vec {
    pub bv_page:   *mut c::page,
    pub bv_len:    u32,
    pub bv_offset: u32,
}

impl Default for bio_vec {
    fn default() -> Self {
        ::pin_init::Zeroable::zeroed()
    }
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone, ::pin_init::MaybeZeroable)]
pub struct bvec_iter {
    pub bi_sector:    u64,	/* device address in 512 byte sectors */
    pub bi_size:      u32,	/* residual I/O count */
    pub bi_idx:       u32,	/* current index into bvl_vec */
    pub bi_bvec_done: u32,	/* number of bytes completed in current bvec */
}

unsafe fn flagged(bio: *const c::bio, bit: u32) -> bool {
    unsafe { (*bio).bi_flags & (1 << bit) != 0 }
}

/// # Safety
/// @bio is a bio, not in flight, with a bvec free; @vaddr is the allocators',
/// @len bytes of it in one bvec, and outlives its place there.
pub unsafe fn add_virt_nofail(bio: *mut c::bio, vaddr: *mut c_void, len: u32) {
    unsafe {
        crate::warn_on_once!(flagged(bio, c::BIO_CLONED));
        crate::warn_on_once!((*bio).bi_vcnt >= (*bio).bi_max_vecs);

        let bv = &mut *(*bio).bi_io_vec.add((*bio).bi_vcnt.into());
        bv.bv_page   = vaddr.map_addr(|a| a & !(PAGE_SIZE - 1)).cast();
        bv.bv_offset = (vaddr.addr() & (PAGE_SIZE - 1)) as u32;
        bv.bv_len    = len;

        (*bio).bi_iter.bi_size += len;
        (*bio).bi_vcnt += 1;
    }
}

unsafe fn remaining_done(bio: *mut c::bio) -> bool {
    /*
     * If we're not chaining, then ->__bi_remaining is always 1 and
     * we always end io on the first invocation.
     */
    if !unsafe { flagged(bio, c::BIO_CHAIN) } {
        return true;
    }

    let remaining = unsafe { AtomicI32::from_ptr(&raw mut (*bio).__bi_remaining.counter) };
    let r = remaining.fetch_sub(1, AcqRel);
    assert!(r > 0, "bio_endio() of a chained bio with {r} remaining");

    if r == 1 {
        unsafe { (*bio).bi_flags &= !(1 << c::BIO_CHAIN) };
        return true;
    }
    false
}

/// # Safety
/// @bio is a bio whose IO is done: nothing else completes it.
pub unsafe fn endio(bio: *mut c::bio) {
    unsafe {
        if !remaining_done(bio) {
            return;
        }
        if let Some(end_io) = (*bio).bi_end_io {
            end_io(bio);
        }
    }
}

/*
 * Increment chain count for the bio. Make sure the CHAIN flag update
 * is visible before the raised count.
 */
///
/// # Safety
/// @bio is a bio.
pub unsafe fn inc_remaining(bio: *mut c::bio) {
    unsafe {
        (*bio).bi_flags |= 1 << c::BIO_CHAIN;
        AtomicI32::from_ptr(&raw mut (*bio).__bi_remaining.counter).fetch_add(1, SeqCst);
    }
}

/// Nothing to release here: no integrity, crypt or cgroup state.
///
/// # Safety
/// @bio is a bio, not used after.
pub unsafe fn uninit(_bio: *mut c::bio) {}

/// # Safety
/// @bio is a bio's memory, not in flight; @table has room for @max_vecs.
pub unsafe fn init(bio: *mut c::bio, bdev: *mut crate::os::blkdev::BlockDevice, table: *mut c::bio_vec,
                   max_vecs: u16, opf: u32) {
    unsafe {
        bio.write_bytes(0, 1);
        (*bio).bi_bdev = bdev;
        (*bio).bi_opf  = opf;
        (*bio).__bi_remaining.counter = 1;
        (*bio).__bi_cnt.counter = 1;

        (*bio).bi_io_vec   = table;
        (*bio).bi_max_vecs = max_vecs;
    }
}

/// # Safety
/// @bio is a bio, not in flight.
pub unsafe fn reset(bio: *mut c::bio, bdev: *mut crate::os::blkdev::BlockDevice, opf: u32) {
    unsafe {
        let flags = (*bio).bi_flags & (!0 << c::BIO_RESET_BITS);

        bio.cast::<u8>().write_bytes(0, offset_of!(c::bio, bi_max_vecs));
        (*bio).bi_bdev  = bdev;
        (*bio).bi_opf   = opf;
        (*bio).bi_flags = flags;
        (*bio).__bi_remaining.counter = 1;
    }
}
