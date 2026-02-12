pub mod accounting;
pub mod format;
pub mod handle;
pub mod ioctl;
pub mod super_io;
pub mod sysfs;

/// Convert a bcachefs error code to a human-readable string.
pub fn bch_err_str(err: i32) -> std::borrow::Cow<'static, str> {
    unsafe { std::ffi::CStr::from_ptr(bch_bindgen::c::bch2_err_str(err)).to_string_lossy() }
}

/// RAII guard for the bch_fs superblock lock. Unlocks on drop.
pub struct SbLockGuard(*mut libc::pthread_mutex_t);

impl Drop for SbLockGuard {
    fn drop(&mut self) {
        unsafe { libc::pthread_mutex_unlock(self.0); }
    }
}

/// Lock the superblock mutex and return a guard that unlocks on drop.
///
/// # Safety
/// `fs` must be a valid pointer to an open `bch_fs`.
pub unsafe fn sb_lock(fs: *mut bch_bindgen::c::bch_fs) -> SbLockGuard {
    let lock = &mut (*fs).sb_lock.lock as *mut _ as *mut libc::pthread_mutex_t;
    libc::pthread_mutex_lock(lock);
    SbLockGuard(lock)
}
