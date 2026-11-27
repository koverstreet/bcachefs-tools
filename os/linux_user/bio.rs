// SPDX-License-Identifier: GPL-2.0

//! A bio's completion, its reset for another IO, and memory mapped into its
//! bvecs, for userspace: block/bio.c's.

use crate::c;
use crate::os::kernel::PAGE_SIZE;
use core::mem::offset_of;

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

/// The bvecs of @len bytes at @buf: one, its page - its address here - and
/// the offset into it, however many pages it spans.
///
/// # Safety
/// @buf is the allocators', @len bytes of it.
pub unsafe fn bvecs(buf: *mut u8, len: usize) -> impl Iterator<Item = c::bio_vec> {
    core::iter::once(c::bio_vec {
        bv_page:   buf.map_addr(|a| a & !(PAGE_SIZE - 1)).cast(),
        bv_len:    u32::try_from(len).expect("a bvec of 4G or more"),
        bv_offset: (buf.addr() & (PAGE_SIZE - 1)) as u32,
    })
}

/// How many bvecs mapping @len bytes at @p takes: one.
pub fn buf_nr_bvecs(_p: *const u8, _len: usize) -> usize {
    1
}

/// How much of @len bytes at @p one bio maps: all of it, in one bvec.
pub fn buf_bio_bytes(_p: *const u8, len: usize) -> usize {
    len
}

/// # Safety
/// @bio is a bio whose IO is done: nothing else completes it.
pub unsafe fn endio(bio: *mut c::bio) {
    unsafe {
        if let Some(end_io) = (*bio).bi_end_io {
            end_io(bio);
        }
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
