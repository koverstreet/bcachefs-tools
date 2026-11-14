// SPDX-License-Identifier: GPL-2.0

//! What userspace's shim headers had, and fs/ still names by C's names - the
//! crate's c module has them: a bio, and the block layer's constants. Nothing
//! in C uses them.

#![allow(non_camel_case_types, non_upper_case_globals)]

use bcachefs_shim::c::{atomic_t, bio_vec, bvec_iter};
use core::ffi::c_void;

use super::blkdev::BlockDevice;

pub type blk_status_t = u8;
pub type blk_opf_t    = u32;
pub type blk_mode_t   = u32;
pub type bio_end_io_t = unsafe extern "C" fn(*mut bio);

/// The kernel's struct bio, as much of it as bcachefs uses.
#[repr(C)]
pub struct bio {
    pub bi_next:        *mut bio,
    pub bi_bdev:        *mut BlockDevice,
    pub bi_status:      blk_status_t,
    /// bottom bits the op, top bits req flags
    pub bi_opf:         blk_opf_t,
    pub bi_flags:       u16,
    pub bi_ioprio:      u16,

    pub bi_iter:        bvec_iter,

    pub __bi_remaining: atomic_t,

    pub bi_end_io:      Option<bio_end_io_t>,
    pub bi_private:     *mut c_void,

    pub bi_vcnt:        u16,

    /* Everything from bi_max_vecs on is kept by bio_reset() */
    pub bi_max_vecs:    u16,

    pub __bi_cnt:       atomic_t,

    pub bi_io_vec:      *mut bio_vec,

    pub bi_pool:        *mut c_void,
}

unsafe impl ::pin_init::Zeroable for bio {}

pub const BLK_STS_OK:                   blk_status_t = 0;
pub const BLK_STS_NOTSUPP:              blk_status_t = 1;
pub const BLK_STS_TIMEOUT:              blk_status_t = 2;
pub const BLK_STS_NOSPC:                blk_status_t = 3;
pub const BLK_STS_TRANSPORT:            blk_status_t = 4;
pub const BLK_STS_TARGET:               blk_status_t = 5;
pub const BLK_STS_RESV_CONFLICT:        blk_status_t = 6;
pub const BLK_STS_MEDIUM:               blk_status_t = 7;
pub const BLK_STS_PROTECTION:           blk_status_t = 8;
pub const BLK_STS_RESOURCE:             blk_status_t = 9;
pub const BLK_STS_IOERR:                blk_status_t = 10;
/* hack for device mapper, don't use elsewhere: */
pub const BLK_STS_DM_REQUEUE:           blk_status_t = 11;
pub const BLK_STS_AGAIN:                blk_status_t = 12;
pub const BLK_STS_DEV_RESOURCE:         blk_status_t = 13;
pub const BLK_STS_ZONE_OPEN_RESOURCE:   blk_status_t = 14;
pub const BLK_STS_ZONE_ACTIVE_RESOURCE: blk_status_t = 15;
pub const BLK_STS_OFFLINE:              blk_status_t = 16;
pub const BLK_STS_DURATION_LIMIT:       blk_status_t = 17;
pub const BLK_STS_INVAL:                blk_status_t = 19;

/* open for reading */
pub const BLK_OPEN_READ:     blk_mode_t = 1 << 0;
/* open for writing */
pub const BLK_OPEN_WRITE:    blk_mode_t = 1 << 1;
/* open exclusively (vs other exclusive openers */
pub const BLK_OPEN_EXCL:     blk_mode_t = 1 << 2;
pub const BLK_OPEN_BUFFERED: blk_mode_t = 1 << 5;
pub const BLK_OPEN_CREAT:    blk_mode_t = 1 << 6;

pub const BDEVNAME_SIZE: u32 = 32;
pub const BIO_MAX_VECS:  u32 = 256;
pub const SECTOR_SIZE:   u32 = 512;

pub const BIO_CLONED:     u32 = 2;	/* doesn't own data */
pub const BIO_CHAIN:      u32 = 7;	/* chained bio, ->bi_remaining in effect */
/* Flags starting here are kept by bio_reset() */
pub const BIO_RESET_BITS: u32 = 10;

/* The op is the bottom REQ_OP_BITS of bi_opf, the flags the rest */
pub const REQ_OP_BITS: u32 = 8;

pub type req_op = u32;
pub const req_op_REQ_OP_READ:         req_op = 0;
pub const req_op_REQ_OP_WRITE:        req_op = 1;
pub const req_op_REQ_OP_FLUSH:        req_op = 2;
pub const req_op_REQ_OP_DISCARD:      req_op = 3;
pub const req_op_REQ_OP_SECURE_ERASE: req_op = 5;
pub const req_op_REQ_OP_ZONE_APPEND:  req_op = 7;
pub const req_op_REQ_OP_WRITE_ZEROES: req_op = 9;
pub const req_op_REQ_OP_ZONE_OPEN:    req_op = 10;
pub const req_op_REQ_OP_ZONE_CLOSE:   req_op = 11;
pub const req_op_REQ_OP_ZONE_FINISH:  req_op = 13;
pub const req_op_REQ_OP_ZONE_RESET:   req_op = 15;
pub const req_op_REQ_OP_ZONE_RESET_ALL: req_op = 17;
pub const req_op_REQ_OP_DRV_IN:       req_op = 34;
pub const req_op_REQ_OP_DRV_OUT:      req_op = 35;

pub const READ: req_op = req_op_REQ_OP_READ;

pub const REQ_SYNC:     blk_opf_t = 1 << 11;
pub const REQ_META:     blk_opf_t = 1 << 12;
pub const REQ_IDLE:     blk_opf_t = 1 << 15;
pub const REQ_FUA:      blk_opf_t = 1 << 17;
pub const REQ_PREFLUSH: blk_opf_t = 1 << 18;
