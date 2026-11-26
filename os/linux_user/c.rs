// SPDX-License-Identifier: GPL-2.0

//! What userspace's shim headers had, and fs/ still names by C's names - the
//! crate's c module has them: a task, work, a bio, and the block layer's
//! constants. Nothing in C uses them.

#![allow(non_camel_case_types, non_upper_case_globals)]

pub use super::bio::sys::{bio_vec, bvec_iter};
pub use super::blkdev::{blk_holder_ops, MINORBITS};
pub use super::fs::*;
pub use super::jump_label::static_key_false;
pub use super::sched::{get_task_struct, put_task_struct, task_struct, wake_up_process,
                       PF_KTHREAD, PF_MEMALLOC_NOIO, PF_MEMALLOC_NORECLAIM, PF_MEMALLOC_NOWARN,
                       TASK_COMM_LEN, TASK_FREEZABLE, TASK_IDLE, TASK_INTERRUPTIBLE, TASK_KILLABLE,
                       TASK_NOLOAD, TASK_RUNNING, TASK_UNINTERRUPTIBLE, TASK_WAKEKILL};
pub use super::workqueue::{delayed_work, work_struct, workqueue_struct};
pub use linux_raw_sys::general::{S_IFDIR, S_IFLNK, S_IFMT, S_IFREG, S_ISGID};
use core::ffi::c_void;
use crate::util::ffi::{c_long, c_ulong};
use core::sync::atomic::{fence, AtomicIsize, Ordering::{Acquire, Relaxed, Release, SeqCst}};

use super::blkdev::BlockDevice;

pub use linux_raw_sys::errno::*;

pub type umode_t = u16;
pub type gfp_t   = u32;
pub type uid_t   = u32;
pub type gid_t   = u32;
pub type dev_t   = u64;

pub const GFP_KERNEL: gfp_t = 2;
pub const PAGE_ALLOC_COSTLY_ORDER: u32 = 6;

pub const NSEC_PER_MSEC: u32 = 1_000_000;
pub const NSEC_PER_SEC:  u32 = 1_000_000_000;

/// Only ever pointed to.
pub struct page {
    _opaque: [u8; 0],
}

#[derive(::pin_init::Zeroable)]
pub struct atomic_t {
    pub counter: i32,
}

#[derive(Clone, Copy, ::pin_init::Zeroable)]
pub struct list_head {
    pub next: *mut list_head,
    pub prev: *mut list_head,
}

#[derive(Clone, Copy, Debug, Default, ::pin_init::Zeroable)]
pub struct timespec64 {
    pub tv_sec:  i64,
    pub tv_nsec: c_long,
}

/// Userspace's user memory is its own: nothing faults.
///
/// # Safety
/// @to and @from are valid for @n bytes, and don't overlap.
pub unsafe fn copy_to_user(to: *mut c_void, from: *const c_void, n: c_ulong) -> c_ulong {
    unsafe { core::ptr::copy_nonoverlapping(from.cast::<u8>(), to.cast::<u8>(), n as usize) };
    0
}

/// A percpu_ref, where a thread is its own CPU: one atomic count.
pub struct percpu_ref {
    count:   AtomicIsize,
    release: Option<percpu_ref_func_t>,
}

pub type percpu_ref_func_t = unsafe extern "C" fn(*mut percpu_ref);

/* @flags for percpu_ref_init() */
pub const PERCPU_REF_INIT_ATOMIC: u32 = 1 << 0;
/*
 * Start dead w/ ref == 0 in atomic mode.  Must be revived with
 * percpu_ref_reinit() before used.  Implies INIT_ATOMIC.
 */
pub const PERCPU_REF_INIT_DEAD:   u32 = 1 << 1;

/// # Safety
/// @r is a percpu_ref's place, not live.
pub unsafe fn percpu_ref_init(r: *mut percpu_ref, release: Option<percpu_ref_func_t>,
                              flags: u32, _gfp: gfp_t) -> i32 {
    let count = (flags & PERCPU_REF_INIT_DEAD == 0) as isize;
    unsafe { r.write(percpu_ref { count: AtomicIsize::new(count), release }) };
    0
}

/// # Safety
/// @r is a percpu_ref, initialized or zeroed.
pub unsafe fn percpu_ref_exit(_r: *mut percpu_ref) {}

/// # Safety
/// @r is a percpu_ref, between init and exit.
pub unsafe fn percpu_ref_get(r: *mut percpu_ref) {
    unsafe { (*r).count.fetch_add(1, Relaxed) };
}

/// # Safety
/// As percpu_ref_get()'s.
pub unsafe fn percpu_ref_tryget(r: *mut percpu_ref) -> bool {
    let count = unsafe { &(*r).count };
    let mut c = count.load(Relaxed);
    while c != 0 {
        match count.compare_exchange_weak(c, c + 1, SeqCst, SeqCst) {
            Ok(_)    => return true,
            Err(old) => c = old,
        }
    }
    false
}

/// # Safety
/// As percpu_ref_get()'s.
pub unsafe fn percpu_ref_tryget_live(r: *mut percpu_ref) -> bool {
    unsafe { percpu_ref_tryget(r) }
}

/// # Safety
/// As percpu_ref_get()'s, and the caller's ref is put.
pub unsafe fn percpu_ref_put(r: *mut percpu_ref) {
    if unsafe { (*r).count.fetch_sub(1, Release) } == 1 {
        fence(Acquire);
        unsafe { (*r).release.expect("a percpu_ref's release")(r) }
    }
}

/// # Safety
/// As percpu_ref_get()'s, and @r is killed and released.
pub unsafe fn percpu_ref_reinit(r: *mut percpu_ref) {
    unsafe { percpu_ref_get(r) }
}

/// Drop the initial ref.
///
/// # Safety
/// As percpu_ref_get()'s, and once before shutdown.
pub unsafe fn percpu_ref_kill(r: *mut percpu_ref) {
    unsafe { percpu_ref_put(r) }
}

/// # Safety
/// As percpu_ref_get()'s.
pub unsafe fn percpu_ref_is_zero(r: *mut percpu_ref) -> bool {
    unsafe { (*r).count.load(Relaxed) == 0 }
}

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

/// local_lock_t: nothing to it here, where a thread is its own CPU
pub struct local_lock_t;
