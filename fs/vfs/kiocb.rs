// SPDX-License-Identifier: GPL-2.0

//! The VFS's kiocb: a read or a write of a file, at a position - done by the
//! call it's passed to, or for an async one, queued and completed later.

use super::inode::InodeInfo;
use crate::c;
use crate::util::file::File;
use crate::util::iov_iter::IovIter;
use core::ffi::c_int;
use core::ptr::NonNull;

/// A kiocb, borrowed from the VFS for a call.
pub struct Kiocb<'a>(&'a mut c::kiocb);

impl<'a> Kiocb<'a> {
    /// # Safety
    /// @k is valid, and used by nothing else, for 'a: the VFS's, for the call
    /// it's passed to.
    pub unsafe fn from_raw(k: *mut c::kiocb) -> Self {
        Self(unsafe { &mut *k })
    }

    pub fn as_ptr(&mut self) -> *mut c::kiocb {
        self.0
    }

    /// Open for the call, and after: the VFS holds it until the kiocb's done.
    pub fn file(&self) -> &'a File {
        unsafe { File::from_raw(self.0.ki_filp) }
    }

    /// file_bch_inode()
    pub fn inode(&self) -> &'a InodeInfo {
        unsafe { InodeInfo::from_inode((*self.0.ki_filp).f_inode) }
    }

    pub fn pos(&self) -> i64 {
        self.0.ki_pos
    }

    pub fn advance(&mut self, bytes: i64) {
        self.0.ki_pos += bytes;
    }

    pub fn direct(&self) -> bool {
        self.0.ki_flags & c::IOCB_DIRECT as c_int != 0
    }

    /// filemap_read(): a buffered read, through the page cache.
    pub fn filemap_read(&mut self, iter: &mut IovIter<'_>) -> isize {
        unsafe { c::filemap_read(self.0, iter.as_ptr(), 0) }
    }

    /// is_sync_kiocb(): done by the call, not completed later.
    pub fn is_sync(&self) -> bool {
        self.0.ki_complete.is_none()
    }

    /// An async kiocb's completion, for once the call's returned -EIOCBQUEUED:
    /// None for a sync one, which the call's return completes.
    ///
    /// # Safety
    /// If it's completed, the call returns -EIOCBQUEUED: the VFS keeps the
    /// kiocb for the completion then, and only then.
    pub unsafe fn queued(&mut self) -> Option<Completion> {
        (!self.is_sync()).then(|| Completion(NonNull::from(&mut *self.0)))
    }
}

/// An async kiocb's completion: what it read or wrote, or its error, given
/// once.
pub struct Completion(NonNull<c::kiocb>);

unsafe impl Send for Completion {}

impl Completion {
    pub fn complete(self, ret: isize) {
        let k = self.0.as_ptr();
        unsafe { ((*k).ki_complete.expect("an async kiocb's completion"))(k, ret) }
    }
}
