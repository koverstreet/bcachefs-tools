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

/// What open_by_path() opens: the device's file, closed - bdev_fput() - as it
/// drops.
pub struct BdevFile(NonNull<c::file>);

// a file, any thread's to use and close
unsafe impl Send for BdevFile {}
unsafe impl Sync for BdevFile {}

impl BdevFile {
    /// The device it has open, while it does.
    pub fn bdev(&self) -> &BlockDevice {
        unsafe { &*c::file_bdev(self.0.as_ptr()) }
    }
}

impl Drop for BdevFile {
    fn drop(&mut self) {
        unsafe { c::bdev_fput(self.0.as_ptr()) }
    }
}

/// What a device's exclusive holder is to the block layer, which calls it as
/// the device goes away, or to sync: get(), under bd_holder_lock - which keeps
/// the holder live - for what outlives the lock, then the call with that, the
/// lock dropped. holder_ops::<Self>() are its blk_holder_ops.
pub trait HolderOps: Sized {
    type Ref;

    fn get(&self) -> Option<Self::Ref>;
    fn mark_dead(r: Self::Ref, bdev: &BlockDevice, surprise: bool);
    fn sync(r: Self::Ref);
}

/// The blk_holder_ops of a device opened with an @H as its holder.
pub const fn holder_ops<H: HolderOps>() -> c::blk_holder_ops {
    c::blk_holder_ops {
        mark_dead: Some(holder_mark_dead::<H>),
        sync:      Some(holder_sync::<H>),
        ..::pin_init::zeroed()
    }
}

/// # Safety
/// @bdev's holder is an @H, and bd_holder_lock is held: this drops it.
unsafe fn holder_get<H: HolderOps>(bdev: *mut BlockDevice) -> Option<H::Ref> {
    let r = unsafe { (*(*bdev).bd_holder.cast::<H>()).get() };
    unsafe { c::mutex_unlock((&raw mut (*bdev).bd_holder_lock).cast()) };
    r
}

unsafe extern "C" fn holder_mark_dead<H: HolderOps>(bdev: *mut BlockDevice, surprise: bool) {
    if let Some(r) = unsafe { holder_get::<H>(bdev) } {
        H::mark_dead(r, unsafe { &*bdev }, surprise)
    }
}

unsafe extern "C" fn holder_sync<H: HolderOps>(bdev: *mut BlockDevice) {
    if let Some(r) = unsafe { holder_get::<H>(bdev) } {
        H::sync(r)
    }
}

/// @path opened, as @mode says: bdev_file_open_by_path(). @holder, if
/// BLK_OPEN_EXCL, is the exclusive claim, and @ops the block layer's calls
/// into it - the device going away, a sync.
pub fn open_by_path(path: &CStr, mode: c::blk_mode_t, holder: *const c_void,
                    ops: *const c::blk_holder_ops) -> Result<BdevFile, BchError> {
    let f = unsafe { c::bdev_file_open_by_path(path.as_ptr().cast(), mode, holder.cast_mut(), ops) };
    errptr_to_result(NonNull::new(f).expect("bdev_file_open_by_path(): an ERR_PTR, not NULL")).map(BdevFile)
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

/// Its name, its sysfs directory's: its disk's and partition's - sda1,
/// nvme0n1p1 - as add_partition() names it.
impl crate::util::ToText for BlockDevice {
    fn to_text(&self, out: &mut Printbuf) {
        out.write_bytes(unsafe { CStr::from_ptr(self.bd_device.kobj.name.cast()) }.to_bytes());
    }
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
pub fn issue_discard(bdev: &BlockDevice, sector: u64, nr_sectors: u64, gfp: crate::util::alloc::Flags)
    -> Result<(), BchError>
{
    ret_to_result_void(unsafe {
        c::blkdev_issue_discard(core::ptr::from_ref(bdev).cast_mut(), sector as _, nr_sectors as _, gfp.raw())
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
