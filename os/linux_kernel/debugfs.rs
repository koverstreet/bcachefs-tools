// SPDX-License-Identifier: GPL-2.0

//! debugfs files: os/debugfs.rs's, as the kernel's debugfs files, read by
//! their file_operations.
//!
//! The VFS doesn't serialize reads of one open file - pread() takes no
//! f_pos_lock - so its state is under a mutex.

use crate::c;
use crate::errcode::{errptr_to_result, BchError};
use crate::locking::Locked;
use crate::new_lock;
use crate::util::alloc::{kbox_pin_init, KBox};
use crate::util::ffi::Opaque;
use crate::os::file::{File, FileOps, FileVTable};
use crate::util::Printbuf;
use core::ffi::CStr;
use core::pin::Pin;
use core::ptr::NonNull;
use cstruct_macros::vtable;
use pin_init::{pin_data, pin_init};

#[path = "../debugfs.rs"]
mod common;
pub use common::*;

use crate::os::file::UserBuf;

pub type Dentry = c::dentry;

/* The file */

#[pin_data]
struct Open<F: DebugFile> {
    obj: NonNull<F::Obj>,
    #[pin]
    state: Locked<c::mutex, (F, Printbuf)>,
}

// SAFETY: readers share @obj, as C's do; the state's under its lock
unsafe impl<F: DebugFile> Send for Open<F> {}
unsafe impl<F: DebugFile> Sync for Open<F> {}

#[vtable]
impl<F: DebugFile> FileOps for Open<F> {
    fn open(inode: &Opaque<c::inode>, _file: &File) -> Result<Pin<KBox<Self>>, BchError> {
        // SAFETY: i_private is create_file()'s @obj
        let obj = NonNull::new(unsafe { (*inode.as_ptr()).i_private }.cast::<F::Obj>())
            .expect("debugfs file without its object");

        let mut buf = Printbuf::new();
        buf.set_may_vmalloc(F::MAY_VMALLOC);

        kbox_pin_init(pin_init!(Open::<F> {
            obj,
            state <- new_lock!((F::default(), buf)),
        }? BchError))
    }

    fn read(&self, _file: &File, u: &mut UserBuf) -> Result<(), BchError> {
        // SAFETY: @obj outlives reads
        let obj = unsafe { self.obj.as_ref() };

        let mut state = self.state.lock();
        let (cursor, buf) = &mut *state;
        read_locked(cursor, obj, &mut DebugOut { buf, u })
    }
}

/* Creating and removing them */

/// None if debugfs_create_dir() failed, or debugfs is disabled.
pub fn create_dir(name: &CStr, parent: Option<NonNull<Dentry>>) -> Option<NonNull<Dentry>> {
    let parent = parent.map_or(core::ptr::null_mut(), NonNull::as_ptr);
    let dir = unsafe { c::debugfs_create_dir(name.as_ptr().cast(), parent) };
    errptr_to_result(NonNull::new(dir).expect("a dentry, or an error")).ok()
}

/// Read-only: a DebugFile has no write. Failure is ignored, as in C.
///
/// # Safety
/// @obj outlives the file: it's removed, with remove_recursive(), first.
pub unsafe fn create_file<F: DebugFile>(name: &CStr, parent: NonNull<Dentry>, obj: &F::Obj) {
    unsafe {
        c::bch2_debugfs_create_file(name.as_ptr().cast(), 0o400, parent.as_ptr(),
                                    core::ptr::from_ref(obj).cast_mut().cast(),
                                    &FileVTable::<Open<F>>::OPS);
    }
}

pub fn remove_recursive(dir: NonNull<Dentry>) {
    // debugfs_remove_recursive() is a macro for it, in-kernel
    unsafe { c::debugfs_remove(dir.as_ptr()) }
}
