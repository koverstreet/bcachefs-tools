// SPDX-License-Identifier: GPL-2.0

//! The VFS's kiocb: a read or a write of a file, at a position - done by the
//! call it's passed to, or for an async one, queued and completed later.

use crate::c;
use crate::errcode::BchError;
use crate::os::file::{Any, File, FsFile};
use crate::os::iov_iter::IovIter;
use crate::os::ops::IntoRaw;
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
        self.0.ki_flags & c::IOCB_DIRECT as i32 != 0
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
        BchError::from_ret(ret as i64).map_or(Ok(ret as usize), Err)
    }

    /// The @written bytes just written synced, for O_SYNC and O_DSYNC:
    /// generic_write_sync() - @written, or the error syncing them.
    pub fn write_sync(&mut self, written: usize) -> Result<usize, BchError> {
        let ret = unsafe { c::generic_write_sync(self.0, written as isize) };
        BchError::from_ret(ret as i64).map_or(Ok(ret as usize), Err)
    }

    /// What's owed it, once the call's taken it on: an async kiocb's
    /// completion - or @sync, the end of a caller that waits for it, which a
    /// sync kiocb's always is.
    ///
    /// # Safety
    /// Without @sync, the call returns -EIOCBQUEUED: the VFS keeps the kiocb,
    /// and its file, for the completion then, and only then. With @sync, the
    /// caller waits for it before it returns. Either way the file's held till
    /// the Owed's completed.
    pub unsafe fn owed<S>(&mut self, sync: Option<S>) -> Owed<S, D> {
        assert!(sync.is_some() || !self.is_sync(), "a sync kiocb, owed a completion");
        Owed { kiocb: NonNull::from(&mut *self.0), sync, _d: PhantomData }
    }
}

impl<'a, D: FsFile> Kiocb<'a, D> {
    pub fn inode(&self) -> &'a D::Inode {
        self.file().inode()
    }
}

/// What a call owes its kiocb, once it's taken it on: an async kiocb's
/// completion, or S, the end of a caller that waits for it - what it read or
/// wrote, or its error, given once. While it's owed, the kiocb's file is held
/// - the aio or io_uring request's, put once it's completed, or the
/// caller's, which waits - so file() and inode() lend it, and complete() ends
/// the loan.
pub struct Owed<S, D = Any> {
    kiocb: NonNull<c::kiocb>,
    /// The caller's end, if it waits; else the kiocb's ki_complete
    sync:  Option<S>,
    _d:    PhantomData<fn() -> D>,
}

/// The end of a call whose caller waits for it: what an Owed's completion
/// is, for a kiocb that's done once the call returns.
pub trait SyncEnd<R> {
    fn complete(self, ret: R);
}

// completed by whichever thread finishes the IO
unsafe impl<S: Send, D> Send for Owed<S, D> {}

impl<S, D> Owed<S, D> {
    pub fn file(&self) -> &File<D> {
        unsafe { &*(*self.kiocb.as_ptr()).ki_filp.cast::<File<D>>() }
    }

    /// An async kiocb's position moved on @bytes, before it's completed:
    /// what was written. A sync one's caller moves its own.
    pub fn advance(&mut self, bytes: u64) {
        if self.sync.is_none() {
            unsafe { (*self.kiocb.as_ptr()).ki_pos += bytes as i64 };
        }
    }

    pub fn complete<R: IntoRaw<isize>>(self, ret: R)
    where
        S: SyncEnd<R>,
    {
        match self.sync {
            Some(s) => s.complete(ret),
            None => {
                let k = self.kiocb.as_ptr();
                unsafe { ((*k).ki_complete.expect("an async kiocb's completion"))(k, ret.into_raw()) }
            }
        }
    }
}

impl<S, D: FsFile> Owed<S, D> {
    pub fn inode(&self) -> &D::Inode {
        self.file().inode()
    }
}
