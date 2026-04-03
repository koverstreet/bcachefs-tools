use crate::bcachefs;
use crate::c;
use crate::data::io::{ReadOp, WriteOp};
use crate::errcode::{self, BchError, errptr_to_result};

fn ret_to_result(ret: i32) -> Result<(), BchError> {
    errcode::ret_to_result(ret).map(|_| ())
}
use std::ffi::CString;
use std::ops::ControlFlow;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

extern "C" {
    fn rust_get_next_online_dev(
        c: *mut c::bch_fs,
        ca: *mut c::bch_dev,
        ref_idx: u32,
    ) -> *mut c::bch_dev;
    fn rust_put_online_dev_ref(ca: *mut c::bch_dev, ref_idx: u32);
    fn rust_dev_tryget_noerror(c: *mut c::bch_fs, dev: u32) -> *mut c::bch_dev;
    fn rust_dev_put(ca: *mut c::bch_dev);
    fn pthread_mutex_lock(mutex: *mut c::pthread_mutex_t) -> i32;
    fn pthread_mutex_unlock(mutex: *mut c::pthread_mutex_t) -> i32;
    fn rust_accounting_mem_read(
        c: *mut c::bch_fs,
        p: c::bpos,
        v: *mut u64,
        nr: u32,
    );
}

/// RAII guard for a device reference. Calls bch2_dev_put on drop.
///
/// Obtained via `Fs::dev_get()`. Derefs to `&bch_dev` for read access.
pub struct DevRef(*mut c::bch_dev);

impl DevRef {
    /// Get a raw mutable pointer to the device. Needed for C functions
    /// that take `*mut bch_dev`.
    pub fn as_mut_ptr(&self) -> *mut c::bch_dev {
        self.0
    }
}

impl std::ops::Deref for DevRef {
    type Target = c::bch_dev;
    fn deref(&self) -> &c::bch_dev {
        unsafe { &*self.0 }
    }
}

impl Drop for DevRef {
    fn drop(&mut self) {
        unsafe { rust_dev_put(self.0) };
    }
}

/// RAII guard for bch_fs::sb_lock. Unlocks on drop.
pub struct SbLockGuard<'a> {
    fs: &'a Fs,
}

impl Drop for SbLockGuard<'_> {
    fn drop(&mut self) {
        unsafe { pthread_mutex_unlock(&mut (*self.fs.raw).sb_lock.lock); }
    }
}

pub struct Fs {
    pub raw: *mut c::bch_fs,
}

impl Fs {
    /// Create a non-owning `Fs` view from a raw pointer.
    ///
    /// Returns `ManuallyDrop<Fs>` to prevent `Fs::drop` from calling
    /// `bch2_fs_exit`. Deref gives `&Fs` for all methods.
    ///
    /// # Safety
    /// `raw` must point to a valid, live `bch_fs`.
    pub unsafe fn borrow_raw(raw: *mut c::bch_fs) -> std::mem::ManuallyDrop<Fs> {
        std::mem::ManuallyDrop::new(Fs { raw })
    }

    /// Access the superblock handle.
    pub fn sb_handle(&self) -> &bcachefs::bch_sb_handle {
        unsafe { &(*self.raw).disk_sb }
    }

    /// Access the superblock.
    pub fn sb(&self) -> &bcachefs::bch_sb {
        self.sb_handle().sb()
    }

    /// Acquire the superblock lock, returning a guard that releases it on drop.
    pub fn sb_lock(&self) -> SbLockGuard<'_> {
        unsafe { pthread_mutex_lock(&mut (*self.raw).sb_lock.lock); }
        SbLockGuard { fs: self }
    }

    /// Write superblock to disk. Caller must hold sb_lock.
    pub fn write_super(&self) {
        unsafe { c::bch2_write_super(self.raw) };
    }

    /// Get a mutable reference to a member entry in the superblock.
    /// Caller must hold sb_lock.
    ///
    /// # Safety
    /// Caller must hold sb_lock for mutation safety.
    #[allow(clippy::mut_from_ref)] // interior mutability guarded by sb_lock
    pub unsafe fn member_mut(&self, dev_idx: u32) -> &mut c::bch_member {
        unsafe { &mut *c::bch2_members_v2_get_mut((*self.raw).disk_sb.sb, dev_idx as i32) }
    }

    pub fn open(devs: &[PathBuf], mut opts: c::bch_opts) -> Result<Fs, BchError> {
        let devs_cstrs : Vec<_> = devs
            .iter()
            .map(|i| CString::new(i.as_os_str().as_bytes()).unwrap())
            .collect();

        let mut devs_array: Vec<_> = devs_cstrs
            .iter()
            .map(|i| i.as_ptr())
            .collect();

        let ret = unsafe {
            let mut devs: c::darray_const_str = std::mem::zeroed();

            devs.data = devs_array[..].as_mut_ptr();
            devs.nr = devs_array.len();

            c::bch2_fs_open(&mut devs, &mut opts)
        };

        errptr_to_result(ret).map(|fs| Fs { raw: fs })
    }

    /// Shut down the filesystem, returning the error code from bch2_fs_exit.
    /// Consumes self so the caller can't use it afterward; forget prevents
    /// Drop from double-freeing.
    pub fn exit(self) -> i32 {
        let ret = unsafe { c::bch2_fs_exit(self.raw) };
        std::mem::forget(self);
        ret
    }

    /// Iterate over all online member devices.
    ///
    /// Equivalent to the C `for_each_online_member` macro. Ref counting
    /// is handled automatically, including on early break.
    pub fn for_each_online_member<F>(&self, mut f: F) -> ControlFlow<()>
    where
        F: FnMut(&c::bch_dev) -> ControlFlow<()>,
    {
        let mut ca: *mut c::bch_dev = std::ptr::null_mut();
        loop {
            ca = unsafe { rust_get_next_online_dev(self.raw, ca, 0) };
            if ca.is_null() {
                return ControlFlow::Continue(());
            }
            if f(unsafe { &*ca }).is_break() {
                unsafe { rust_put_online_dev_ref(ca, 0) };
                return ControlFlow::Break(());
            }
        }
    }

    /// Get the root btree node for a btree ID.
    pub fn btree_id_root(&self, id: u32) -> Option<&c::btree> {
        unsafe {
            let c = &*self.raw;
            let nr_known = c::btree_id::BTREE_ID_NR as u32;

            let r = if id < nr_known {
                &c.btree.cache.roots_known[id as usize]
            } else {
                let idx = (id - nr_known) as usize;
                if idx >= c.btree.cache.roots_extra.nr {
                    return None;
                }
                &*c.btree.cache.roots_extra.data.add(idx)
            };

            let b = r.b;
            if b.is_null() { None } else { Some(&*b) }
        }
    }

    /// Total number of btree IDs (known + dynamic) on this filesystem.
    pub fn btree_id_nr_alive(&self) -> u32 {
        unsafe {
            let c = &*self.raw;
            c::btree_id::BTREE_ID_NR as u32 + c.btree.cache.roots_extra.nr as u32
        }
    }

    /// Number of devices in the filesystem superblock.
    pub fn nr_devices(&self) -> u32 {
        unsafe { (*self.raw).sb.nr_devices as u32 }
    }

    /// Get a reference to a device by index. Returns None if the device
    /// doesn't exist or can't be referenced.
    pub fn dev_get(&self, dev: u32) -> Option<DevRef> {
        let ca = unsafe { rust_dev_tryget_noerror(self.raw, dev) };
        if ca.is_null() { None } else { Some(DevRef(ca)) }
    }

    /// Start the filesystem (recovery, journal replay, etc).
    pub fn start(&self) -> Result<(), BchError> {
        ret_to_result(unsafe { c::bch2_fs_start(self.raw) })
    }

    /// Allocate the buckets_nouse bitmaps for all devices.
    pub fn buckets_nouse_alloc(&self) -> Result<(), BchError> {
        ret_to_result(unsafe { c::bch2_buckets_nouse_alloc(self.raw) })
    }

    /// Mark device superblock buckets in btree metadata.
    pub fn trans_mark_dev_sb(&self, ca: &DevRef, flags: c::btree_iter_update_trigger_flags) -> Result<(), BchError> {
        ret_to_result(unsafe { c::bch2_trans_mark_dev_sb(self.raw, ca.as_mut_ptr(), flags) })
    }

    /// Write superblock to disk (locked version). Caller must hold sb_lock.
    /// Returns Ok(()) on success or the error code on failure.
    pub fn write_super_ret(&self) -> Result<(), BchError> {
        ret_to_result(unsafe { c::bch2_write_super(self.raw) })
    }

    pub fn write(&self, inum: u64, offset: u64, subvol: u32, replicas: u32, data: &[u8], new_i_size: u64) -> WriteOp {
        WriteOp::new(self, inum, offset, subvol, replicas, data, new_i_size)
    }

    pub fn read<'a>(
        &'a self,
        inum: c::subvol_inum,
        offset: u64,
        inode: &c::bch_inode_unpacked,
        buf: &'a mut [u8],
    ) -> ReadOp {
        ReadOp::new(self, inum, offset, inode, buf)
    }

    /// Check if a device index exists and has a device pointer.
    pub fn dev_exists(&self, dev: u32) -> bool {
        unsafe {
            let c = &*self.raw;
            (dev as usize) < c.sb.nr_devices as usize
                && !c.devs[dev as usize].is_null()
        }
    }

    /// Transition filesystem to read-only mode.
    pub fn read_only(&self) {
        unsafe { c::bch2_fs_read_only(self.raw) };
    }

    /// Set a device's allocator to RW or RO.
    pub fn dev_allocator_set_rw(&self, dev: u32, rw: bool) {
        unsafe {
            let ca = (*self.raw).devs[dev as usize];
            if !ca.is_null() {
                c::bch2_dev_allocator_set_rw(self.raw, ca, rw);
            }
        }
    }

    /// Flush all journal pins (equivalent to bch2_journal_flush_all_pins).
    pub fn journal_flush_all_pins(&self) {
        unsafe {
            c::bch2_journal_flush_pins(
                &mut (*self.raw).journal,
                u64::MAX,
            );
        }
    }

    /// Delete a range of keys in a btree.
    pub fn btree_delete_range(
        &self,
        btree_id: c::btree_id,
        start: c::bpos,
        end: c::bpos,
        flags: c::btree_iter_update_trigger_flags,
    ) -> Result<(), BchError> {
        ret_to_result(unsafe {
            c::bch2_btree_delete_range(self.raw, btree_id, start, end, flags)
        })
    }

    /// Read accounting counters for a given bpos key.
    pub fn accounting_mem_read(&self, pos: c::bpos, nr: u32) -> Vec<u64> {
        let mut v = vec![0u64; nr as usize];
        unsafe {
            rust_accounting_mem_read(self.raw, pos, v.as_mut_ptr(), nr);
        }
        v
    }

    /// Read full device usage stats.
    pub fn dev_usage_full_read(&self, dev: u32) -> c::bch_dev_usage_full {
        unsafe {
            let ca = (*self.raw).devs[dev as usize];
            let mut usage: c::bch_dev_usage_full = std::mem::zeroed();
            c::bch2_dev_usage_full_read_fast(ca, &mut usage);
            usage
        }
    }

    /// Get the raw device pointer by index.
    ///
    /// # Safety
    /// Caller must ensure the device exists and the pointer is valid.
    pub unsafe fn dev_raw(&self, dev: u32) -> *mut c::bch_dev {
        (*self.raw).devs[dev as usize]
    }

    /// Access the mutable superblock handle for resize operations.
    ///
    /// # Safety
    /// Caller must hold sb_lock.
    #[allow(clippy::mut_from_ref)] // interior mutability guarded by sb_lock
    pub unsafe fn disk_sb_mut(&self) -> &mut c::bch_sb_handle {
        &mut (*self.raw).disk_sb
    }

    /// Filesystem block size in bytes.
    pub fn block_bytes(&self) -> u64 {
        unsafe { c::rust_block_bytes(self.raw) as u64 }
    }

    /// Look up an inode by (subvol, inum).
    pub fn inode_find_by_inum(&self, inum: c::subvol_inum) -> Result<c::bch_inode_unpacked, BchError> {
        let mut bi: c::bch_inode_unpacked = Default::default();
        ret_to_result(unsafe { c::bch2_inode_find_by_inum(self.raw, inum, &mut bi) })?;
        Ok(bi)
    }

    /// Convert a bcachefs internal time to a timespec.
    pub fn time_to_timespec(&self, time: i64) -> c::timespec {
        unsafe { c::rust_bch2_time_to_timespec(self.raw, time) }
    }

    /// Convert a timespec to a bcachefs internal time.
    pub fn timespec_to_time(&self, ts: c::timespec) -> i64 {
        unsafe { c::rust_timespec_to_bch2_time(self.raw, ts) }
    }

    /// Get the link count for an inode.
    pub fn inode_nlink_get(bi: &c::bch_inode_unpacked) -> u32 {
        unsafe { c::rust_inode_nlink_get(bi as *const _ as *mut _) }
    }

    /// Set the filesystem log level.
    pub fn set_loglevel(&self, level: u32) {
        unsafe { (*self.raw).loglevel = level; }
    }

    /// Add a device to the filesystem.
    pub fn dev_add(&self, path: &str) -> Result<(), BchError> {
        let path_cstr = CString::new(path).unwrap();
        let mut err = c::printbuf::new();
        let ret = unsafe { c::bch2_dev_add(self.raw, path_cstr.as_ptr(), &mut err) };
        if ret != 0 {
            let msg = unsafe { std::ffi::CStr::from_ptr(err.buf) }
                .to_string_lossy();
            eprintln!("error adding device: {}", msg);
        }
        ret_to_result(ret)
    }
}

impl Drop for Fs {
    fn drop(&mut self) {
        unsafe { c::bch2_fs_exit(self.raw); }
    }
}

// Standalone helpers — pure Rust reimplementations of C static inlines.

/// Sector offset of bucket `b` on device `ca`.
pub fn bucket_to_sector(ca: &c::bch_dev, b: u64) -> u64 {
    b * ca.mi.bucket_size as u64
}

/// Size of one bucket in bytes.
pub fn bucket_bytes(ca: &c::bch_dev) -> u64 {
    ca.mi.bucket_size as u64 * 512
}

/// Build a hashed writepoint specifier (sets low bit to mark as hashed).
pub fn writepoint_hashed(v: usize) -> c::write_point_specifier {
    c::write_point_specifier { v: (v | 1) as _ }
}

/// Convert a device index to a target (TARGET_DEV_START = 1).
pub fn dev_to_target(dev: u32) -> u16 {
    1 + dev as u16
}

/// Check if a btree ID is an allocator btree.
pub fn btree_id_is_alloc(id: u32) -> bool {
    use c::btree_id::*;
    matches!(
        c::btree_id::from_raw(id),
        Some(BTREE_ID_alloc
            | BTREE_ID_backpointers
            | BTREE_ID_stripe_backpointers
            | BTREE_ID_need_discard
            | BTREE_ID_freespace
            | BTREE_ID_bucket_gens
            | BTREE_ID_lru
            | BTREE_ID_accounting
            | BTREE_ID_reconcile_work
            | BTREE_ID_reconcile_hipri
            | BTREE_ID_reconcile_pending
            | BTREE_ID_reconcile_scan)
    )
}
