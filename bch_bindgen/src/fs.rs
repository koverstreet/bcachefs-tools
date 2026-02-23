use crate::bcachefs;
use crate::c;
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
    /// `dev_idx` must be a valid device index.
    #[allow(clippy::mut_from_ref)] // interior mutability guarded by sb_lock
    pub unsafe fn members_v2_get_mut(&self, dev_idx: u32) -> &mut c::bch_member {
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

    /// Check if a device index exists and has a device pointer.
    pub fn dev_exists(&self, dev: u32) -> bool {
        unsafe {
            let c = &*self.raw;
            (dev as usize) < c.sb.nr_devices as usize
                && !c.devs[dev as usize].is_null()
        }
    }
}

impl Drop for Fs {
    fn drop(&mut self) {
        unsafe { c::bch2_fs_exit(self.raw); }
    }
}
