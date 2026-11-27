// SPDX-License-Identifier: GPL-2.0

//! Character devices: a dynamic major's minors, opened as a file type's; the
//! class their nodes are in; and a node, a device in the class. The major and
//! the class are statics', from module init to exit.

use core::ffi::CStr;
use core::ptr::{null_mut, NonNull};
use core::sync::atomic::{AtomicU32, Ordering::Relaxed};

use crate::c;
use crate::errcode::{errptr_to_result, ret_to_result_void, BchError};
use crate::os::file::{Any, FileData, FileOps, FileVTable};
use crate::os::module::{ModuleExit, ThisModule};
use crate::types::CStatic;

/// A major's 256 minors: __register_chrdev()'s.
pub struct Chrdev {
    name:  &'static CStr,
    major: AtomicU32,
}

impl Chrdev {
    pub const fn new(name: &'static CStr) -> Self {
        Chrdev { name, major: AtomicU32::new(0) }
    }

    /// A dynamic major, its minors opened as @P's files.
    pub fn register<P: FileData>(&'static self, _: &ThisModule) -> Result<(), BchError>
        where P::Ops: FileOps<Inode = Any>
    {
        // SAFETY: a name, and ops that last
        let major = unsafe {
            c::__register_chrdev(0, 0, 256, self.name.as_ptr().cast(), &FileVTable::<P>::OPS)
        };
        if let Some(e) = BchError::from_ret(major.into()) {
            return Err(e);
        }
        self.major.store(major as u32, Relaxed);
        Ok(())
    }

    /// A no-op if it wasn't registered.
    pub fn unregister(&'static self, _: &ModuleExit) {
        let major = self.major.swap(0, Relaxed);
        if major != 0 {
            // SAFETY: register()'s
            unsafe { c::__unregister_chrdev(major, 0, 256, self.name.as_ptr().cast()) }
        }
    }

    /// @minor's device number: MKDEV().
    pub fn dev(&self, minor: u32) -> c::dev_t {
        self.major.load(Relaxed) << c::MINORBITS | minor
    }
}

/// @dev's minor: MINOR().
pub fn minor(dev: c::dev_t) -> u32 {
    dev & ((1 << c::MINORBITS) - 1)
}

/// A device class: class_register()'s.
pub struct Class(CStatic<c::class>);

impl Class {
    pub const fn new(name: &'static CStr) -> Self {
        // SAFETY, of the CStatic: a name, and NULLs
        Class(unsafe { CStatic::new(c::class { name: name.as_ptr().cast(), ..::pin_init::zeroed() }) })
    }

    pub fn register(&'static self, _: &ThisModule) -> Result<(), BchError> {
        // SAFETY: a static, with a name
        ret_to_result_void(unsafe { c::class_register(&*self.0) })
    }

    /// A no-op if it wasn't registered.
    pub fn unregister(&'static self, _: &ModuleExit) {
        // SAFETY: a static's, registered or not
        unsafe { c::class_unregister(&*self.0) }
    }
}

/// A device in a class - a node, /dev/<name> - device_create()'s,
/// unregistered when dropped.
pub struct Device(NonNull<c::device>);

// SAFETY: the driver core's, which locks it
unsafe impl Send for Device {}
unsafe impl Sync for Device {}

impl Device {
    pub fn create(class: &'static Class, dev: c::dev_t, name: &CStr) -> Result<Device, BchError> {
        // SAFETY: a static class, and %s of a string
        let p = unsafe {
            c::device_create(&*class.0, null_mut(), dev, null_mut(), c"%s".as_ptr().cast(), name.as_ptr())
        };
        Ok(Device(errptr_to_result(NonNull::new(p).expect("a device, or an error"))?))
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        // SAFETY: create()'s
        unsafe { c::device_unregister(self.0.as_ptr()) }
    }
}
