// SPDX-License-Identifier: GPL-2.0

//! debugfs files: os/debugfs.rs's, as the kernel's debugfs files, read by
//! their file_operations.
//!
//! The VFS doesn't serialize reads of one open file - pread() takes no
//! f_pos_lock - so its read is under a mutex.

use crate::c;
use crate::errcode::{errptr_to_result, BchError};
use crate::locking::{Locked, Mutex};
use crate::new_lock;
use crate::util::alloc::{kbox_pin_init, KBox};
use crate::util::ffi::Opaque;
use crate::os::file::{File, FileOps, FileVTable};
use core::ffi::CStr;
use core::marker::PhantomData;
use core::pin::Pin;
use core::ptr::NonNull;
use cstruct_macros::vtable;
use kernel::alloc::flags::GFP_KERNEL;
use pin_init::{pin_data, pin_init};

#[path = "../debugfs.rs"]
mod common;
pub use common::*;

use crate::os::file::UserBuf;

type Dentry = c::dentry;

/* The file */

#[pin_data]
struct Open<F: DebugFile> {
    #[pin]
    reading: Locked<Mutex, Reading>,
    _file:   PhantomData<fn() -> F>,
}

// SAFETY: readers share the object, as C's do; the read's under its lock
unsafe impl<F: DebugFile> Send for Open<F> {}
unsafe impl<F: DebugFile> Sync for Open<F> {}

#[vtable]
impl<F: DebugFile> FileOps for Open<F> {
    type Data = Self;

    fn open(inode: &Opaque<c::inode>, _file: &File) -> Result<Pin<KBox<Self>>, BchError> {
        // SAFETY: i_private is create_file()'s @obj, live for each op:
        // debugfs's proxy fails ops once the file's removed
        let obj = unsafe { (*inode.as_ptr()).i_private.cast::<F::Obj>().as_ref() }
            .expect("debugfs file without its object");
        let reading = unsafe { reading(KBox::new(read::<F>(obj), GFP_KERNEL)?) };

        kbox_pin_init(pin_init!(Open::<F> {
            reading <- new_lock!(reading),
            _file: PhantomData,
        }? BchError))
    }

    fn read(&self, _file: &File<Self>, u: &mut UserBuf) -> Result<(), BchError> {
        read_into(&mut self.reading.lock(), u)
    }
}

/* Creating and removing them */

/// None if debugfs_create_dir() failed, or debugfs is disabled.
fn create_dir(name: &CStr, parent: Option<NonNull<Dentry>>) -> Option<NonNull<Dentry>> {
    let parent = parent.map_or(core::ptr::null_mut(), NonNull::as_ptr);
    let dir = unsafe { c::debugfs_create_dir(name.as_ptr().cast(), parent) };
    errptr_to_result(NonNull::new(dir).expect("a dentry, or an error")).ok()
}

/// # Safety
/// @obj outlives the file.
unsafe fn create_file<F: DebugFile>(name: &CStr, parent: NonNull<Dentry>, obj: &F::Obj) {
    unsafe {
        // debugfs_create_file() is a _Generic() over its fops' type
        c::debugfs_create_file_full(name.as_ptr().cast(), 0o400, parent.as_ptr(),
                                    core::ptr::from_ref(obj).cast_mut().cast(),
                                    core::ptr::null(), &FileVTable::<Open<F>>::OPS);
    }
}

fn remove_recursive(dir: NonNull<Dentry>) {
    // debugfs_remove_recursive() is a macro for it, in-kernel
    unsafe { c::debugfs_remove(dir.as_ptr()) }
}
