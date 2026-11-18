// SPDX-License-Identifier: GPL-2.0

//! The VFS's kiocb: a read or a write of a file, at a position - done by the
//! call it's passed to, or for an async one, queued and completed later.

use crate::c;
use crate::errcode::BchError;
use crate::os::file::{Any, File, FsFile};
use crate::os::iov_iter::IovIter;
use core::ffi::c_int;
use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::ptr::NonNull;

/// A kiocb of a file of D's, borrowed from the VFS for a call.
pub struct Kiocb<'a, D = Any>(&'a mut c::kiocb, PhantomData<&'a File<D>>);

impl<'a, D> Kiocb<'a, D> {
    /// # Safety
    /// @k is valid, and used by nothing else, for 'a: the VFS's, for the call
    /// it's passed to; its file is D's.
    pub unsafe fn from_raw(k: *mut c::kiocb) -> Self {
        Self(unsafe { &mut *k }, PhantomData)
    }

    /// A synchronous one of our own, in @slot, of @file from @pos:
    /// init_sync_kiocb().
    pub fn sync(slot: &'a mut MaybeUninit<c::kiocb>, file: &'a File<D>, pos: c::loff_t) -> Self {
        unsafe { c::init_sync_kiocb(slot.as_mut_ptr(), file.as_ptr()) };
        let k = unsafe { slot.assume_init_mut() };
        k.ki_pos = pos;
        Self(k, PhantomData)
    }

    pub fn as_ptr(&mut self) -> *mut c::kiocb {
        self.0
    }

    /// Open for the call, and after: the VFS holds it until the kiocb's done.
    pub fn file(&self) -> &'a File<D> {
        unsafe { &*self.0.ki_filp.cast::<File<D>>() }
    }

    pub fn pos(&self) -> u64 {
        self.0.ki_pos as u64
    }

    pub fn advance(&mut self, bytes: u64) {
        self.0.ki_pos += bytes as i64;
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

    /// iocb_is_dsync(): a write to be durable when it's done - O_DSYNC, or a
    /// sync inode.
    pub fn is_dsync(&self) -> bool {
        unsafe { c::iocb_is_dsync(&*self.0) }
    }

    /// generic_write_checks(): @iter cut to what may be written at pos() -
    /// moved to the end for O_APPEND. How much that is: 0, nothing to write.
    pub fn write_checks(&mut self, iter: &mut IovIter<'_>) -> Result<usize, BchError> {
        let ret = unsafe { c::generic_write_checks(self.0, iter.as_ptr()) };
        if ret < 0 { Err(BchError::from_raw(ret as i32)) } else { Ok(ret as usize) }
    }

    /// The @written bytes just written synced, for O_SYNC and O_DSYNC:
    /// generic_write_sync() - @written, or the error syncing them.
    pub fn write_sync(&mut self, written: usize) -> Result<usize, BchError> {
        let ret = unsafe { c::generic_write_sync(self.0, written as isize) };
        if ret < 0 { Err(BchError::from_raw(ret as i32)) } else { Ok(ret as usize) }
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

impl<'a, D: FsFile> Kiocb<'a, D> {
    pub fn inode(&self) -> &'a D::Inode {
        self.file().inode()
    }
}

/// An async kiocb's completion: what it read or wrote, or its error, given
/// once.
pub struct Completion(NonNull<c::kiocb>);

unsafe impl Send for Completion {}

impl Completion {
    /// Its position moved on @bytes, before it's completed: what was written.
    pub fn advance(&mut self, bytes: u64) {
        unsafe { (*self.0.as_ptr()).ki_pos += bytes as i64 };
    }

    pub fn complete(self, ret: isize) {
        let k = self.0.as_ptr();
        unsafe { ((*k).ki_complete.expect("an async kiocb's completion"))(k, ret) }
    }
}
