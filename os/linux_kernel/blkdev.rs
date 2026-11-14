// SPDX-License-Identifier: GPL-2.0

//! The block layer: a device opened by path, what it is, and bios submitted
//! to it.

use crate::c;
use crate::errcode::{errptr_to_result, ret_to_result_void, BchError};
use crate::os::kobject::Kobject;
use crate::util::Printbuf;
use core::ffi::{c_void, CStr};
use core::ptr::NonNull;

pub type BlockDevice = c::block_device;

/// What open_by_path() opens: the device's file.
pub type BdevFile = c::file;

/// @path opened, as @mode says: bdev_file_open_by_path(). @holder, if
/// BLK_OPEN_EXCL, is the exclusive claim, and @ops the block layer's calls
/// into it - the device going away, a sync.
pub fn open_by_path(path: &CStr, mode: c::blk_mode_t, holder: *const c_void,
                    ops: *const c::blk_holder_ops) -> Result<NonNull<BdevFile>, BchError> {
    let f = unsafe { c::bdev_file_open_by_path(path.as_ptr().cast(), mode, holder.cast_mut(), ops) };
    errptr_to_result(NonNull::new(f).expect("bdev_file_open_by_path(): an ERR_PTR, not NULL"))
}

/// The device @file has open.
///
/// # Safety
/// @file is open_by_path()'s, and not yet closed.
pub unsafe fn file_bdev(file: NonNull<BdevFile>) -> *mut BlockDevice {
    unsafe { c::file_bdev(file.as_ptr()) }
}

/// bdev_fput(): @file closed.
///
/// # Safety
/// @file is open_by_path()'s, and nothing uses it, or its device, after.
pub unsafe fn close(file: NonNull<BdevFile>) {
    unsafe { c::bdev_fput(file.as_ptr()) }
}

/// The device @bio is for, if it's set: bio_set_dev()'s.
///
/// # Safety
/// @bio's device, if set, is open.
pub unsafe fn bio_bdev<'a>(bio: &c::bio) -> Option<&'a BlockDevice> {
    unsafe { bio.bi_bdev.as_ref() }
}

/// Its number, major and minor.
pub fn dev(bdev: &BlockDevice) -> c::dev_t {
    bdev.bd_dev
}

/// Its name, as %pg prints it: its disk's and partition's.
pub fn prt_name(out: &mut Printbuf, bdev: &BlockDevice) {
    out.prt_ptr(c"%pg", core::ptr::from_ref(bdev).cast());
}

/// Its sysfs directory's kobject.
pub fn kobj(bdev: &BlockDevice) -> &Kobject {
    unsafe { Kobject::from_raw((&raw const bdev.bd_device.kobj).cast_mut()) }
}

/// @path's device, by number: lookup_bdev() - no IO, nothing claimed.
pub fn lookup(path: &CStr) -> Result<c::dev_t, BchError> {
    let mut dev = 0;
    ret_to_result_void(unsafe { c::lookup_bdev(path.as_ptr().cast(), &mut dev) })?;
    Ok(dev)
}

/// The device's size, in 512 byte sectors: get_capacity() of its disk.
pub fn capacity(bdev: &BlockDevice) -> u64 {
    // an open block device has a disk, and its disk a part0 - itself
    unsafe { (*(*bdev.bd_disk).part0).bd_nr_sectors as u64 }
}

/// The smallest IO @bdev takes: bdev_logical_block_size().
pub fn logical_block_size(bdev: &BlockDevice) -> u32 {
    unsafe { (*bdev.bd_queue).limits.logical_block_size }
}

/// The most @bdev discards at once, in sectors - 0 if it doesn't:
/// bdev_max_discard_sectors().
pub fn max_discard_sectors(bdev: &BlockDevice) -> u32 {
    unsafe { (*bdev.bd_queue).limits.max_discard_sectors }
}

/// Discard @nr_sectors from @sector on @bdev, and wait for it:
/// blkdev_issue_discard().
pub fn issue_discard(bdev: &BlockDevice, sector: u64, nr_sectors: u64, gfp: c::gfp_t)
    -> Result<(), BchError>
{
    ret_to_result_void(unsafe {
        c::blkdev_issue_discard(core::ptr::from_ref(bdev).cast_mut(), sector as _, nr_sectors as _, gfp)
    })
}

/// Whether @bdev is a spinning disk: bdev_rot().
pub fn rotational(bdev: &BlockDevice) -> bool {
    unsafe { c::bch2_bdev_rot(core::ptr::from_ref(bdev).cast_mut()) }
}

/// Its page cache dropped: invalidate_bdev().
pub fn invalidate(bdev: &BlockDevice) {
    unsafe { c::invalidate_bdev(core::ptr::from_ref(bdev).cast_mut()) }
}

/// @bio for @bdev: bio_set_dev(), its blkg associated.
pub fn bio_set_dev(bio: &mut c::bio, bdev: *mut BlockDevice) {
    unsafe { c::bio_set_dev(bio, bdev) }
}

/// submit_bio(): its bi_end_io is called when it's done.
///
/// # Safety
/// @bio is ours, ready, and nothing touches it till its endio.
pub unsafe fn submit_bio(bio: *mut c::bio) {
    unsafe { c::submit_bio(bio) }
}

/// submit_bio_wait(): submitted, and waited for, its own endio not called.
///
/// # Safety
/// As submit_bio()'s.
pub unsafe fn submit_bio_wait(bio: *mut c::bio) -> Result<(), BchError> {
    ret_to_result_void(unsafe { c::submit_bio_wait(bio) })
}

/// @f with the block layer's plug held: the bios it submits held back to be
/// merged, then issued together - blk_start_plug() and blk_finish_plug(). The
/// plug's the task's, so it can't move while it's held.
pub fn plugged<R>(f: impl FnOnce() -> R) -> R {
    let mut plug = core::mem::MaybeUninit::<c::blk_plug>::uninit();
    unsafe { c::blk_start_plug(plug.as_mut_ptr()) };
    let ret = f();
    unsafe { c::blk_finish_plug(plug.as_mut_ptr()) };
    ret
}
