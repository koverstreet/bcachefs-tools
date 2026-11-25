// SPDX-License-Identifier: GPL-2.0

//! debugfs files, read a piece at a time: a file's text is an async fn
//! printing into a printbuf, copied out as the reader's buffer has room -
//! flush().await suspends it while the buffer's full, until the next read. So
//! it's never all in memory at once, and where a read left off is wherever
//! the body was.
//!
//! The body's object is good only while it's being read: debugfs_remove()
//! waits out reads in progress and fails later ones, and an open file can be
//! released after. So what's held across an .await is plain data: no lock,
//! no transaction, nothing whose drop touches the object.
//!
//! A read's buffer reaches the body through the waker of the poll the read
//! makes - nothing else touches the body's state while it's suspended.
//!
//! What a reader's buffer is, and how files are made, found and removed, are
//! the port's debugfs.rs, which has this module too.

use core::ffi::CStr;
use core::future::Future;
use core::marker::PhantomData;
use core::ops::{ControlFlow, Deref, DerefMut};
use core::pin::Pin;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicPtr, Ordering};
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use crate::c;
use crate::errcode::BchError;
use crate::os::module::ModuleExit;
use crate::util::alloc::KBox;
use crate::util::rcu::FreeAnytime;
use crate::util::Printbuf;
use super::{create_dir, create_file, remove_recursive, Dentry, UserBuf};

/// A debugfs file: show() is what it reads as - an async fn, in an impl.
pub trait DebugFile: 'static {
    /// What the file is of: what Dir::file() was given.
    type Obj: 'static;

    fn show(obj: &Self::Obj, out: &mut DebugOut) -> impl Future<Output = Result<(), BchError>> + FreeAnytime;
}

/* Directories */

/// A directory, removed with everything in it when it's dropped.
pub struct DebugDir(NonNull<Dentry>);

unsafe impl Send for DebugDir {}
unsafe impl Sync for DebugDir {}
unsafe impl pin_init::ZeroableOption for DebugDir {}

/// A directory to make files and directories in: one a DebugDir has.
#[derive(Clone, Copy)]
pub struct Dir<'a>(NonNull<Dentry>, PhantomData<&'a DebugDir>);

impl DebugDir {
    pub fn dir(&self) -> Dir<'_> {
        Dir(self.0, PhantomData)
    }
}

impl Drop for DebugDir {
    fn drop(&mut self) {
        remove_recursive(self.0)
    }
}

impl<'a> Dir<'a> {
    pub fn subdir(self, name: &CStr) -> Option<Dir<'a>> {
        create_dir(name, Some(self.0)).map(|d| Dir(d, PhantomData))
    }

    /// Read-only: a DebugFile has no write. Failure is ignored, as in C.
    ///
    /// # Safety
    /// @obj outlives the DebugDir this is in.
    pub unsafe fn file<F: DebugFile>(self, name: &CStr, obj: &F::Obj) {
        unsafe { create_file::<F>(name, self.0, obj) }
    }
}

/// The module's directory, debugfs's bcachefs/: made at module init, removed
/// at exit, when there's no filesystem left to have a directory in it.
pub struct DebugRoot(AtomicPtr<Dentry>);

impl DebugRoot {
    pub const fn new() -> Self {
        DebugRoot(AtomicPtr::new(ptr::null_mut()))
    }

    pub fn init(&self, name: &CStr) {
        let dir = create_dir(name, None).map_or(ptr::null_mut(), NonNull::as_ptr);
        self.0.store(dir, Ordering::Release);
    }

    /// None if it couldn't be made, or init() couldn't make the root.
    pub fn subdir(&self, name: &CStr) -> Option<DebugDir> {
        let root = NonNull::new(self.0.load(Ordering::Acquire))?;
        create_dir(name, Some(root)).map(DebugDir)
    }

    /// Dir::file(), in the root: of something that lasts, as the root does
    /// till module exit.
    pub fn file<F: DebugFile>(&self, name: &CStr, obj: &'static F::Obj) {
        if let Some(root) = NonNull::new(self.0.load(Ordering::Acquire)) {
            unsafe { create_file::<F>(name, root, obj) }
        }
    }

    /// At module exit: every DebugDir subdir() made - a filesystem's - is gone
    /// already, and they're removed with it.
    pub fn exit(&self, _: &ModuleExit) {
        if let Some(dir) = NonNull::new(self.0.swap(ptr::null_mut(), Ordering::AcqRel)) {
            remove_recursive(dir)
        }
    }
}

/* Reading */

/// What a file's show() prints into: derefs to its printbuf.
pub struct DebugOut {
    buf: Printbuf,
    /// The buffer of the read that's polling: set as each poll starts.
    u:   *mut UserBuf,
}

impl DebugOut {
    /// Copy out what's printed, as much as fits; Break once the reader's
    /// buffer is full. For a print_each() that fills the buffer under a lock.
    pub fn try_flush(&mut self) -> Result<ControlFlow<()>, BchError> {
        if self.buf.allocation_failure() {
            return Err(c::ENOMEM.into());
        }

        let u = unsafe { &mut *self.u };
        let pending = self.buf.as_bytes();
        if !pending.is_empty() {
            let want = pending.len().min(u.len());
            let copied = u.copy_to(pending);
            self.buf.drop_front(copied);

            if copied != want {
                return Err(c::EFAULT.into());
            }
        }

        Ok(if u.len() == 0 { ControlFlow::Break(()) } else { ControlFlow::Continue(()) })
    }

    /// Copy out everything printed, suspended until the next read each time
    /// the reader's buffer is full.
    pub async fn flush(&mut self) -> Result<(), BchError> {
        while self.try_flush()?.is_break() {
            suspend().await;
            self.attach().await;
        }
        Ok(())
    }

    /// @print, then a flush, until it returns false. What it locks - a lock,
    /// a transaction - it drops before it returns: nothing is held across
    /// the flush.
    pub async fn print_each(&mut self, mut print: impl FnMut(&mut Self) -> Result<bool, BchError>)
        -> Result<(), BchError>
    {
        while print(self)? {
            self.flush().await?;
        }
        Ok(())
    }

    /// The buffer of the read polling this, from its waker.
    async fn attach(&mut self) {
        core::future::poll_fn(|cx| {
            let waker = cx.waker();
            assert!(ptr::eq(waker.vtable(), &READER), "a debugfs file polled by something other than a read");
            self.u = waker.data().cast_mut().cast();
            Poll::Ready(())
        }).await
    }
}

/// Back to the read, until the next one polls again.
async fn suspend() {
    let mut suspended = false;
    core::future::poll_fn(|_| match core::mem::replace(&mut suspended, true) {
        false => Poll::Pending,
        true  => Poll::Ready(()),
    }).await
}

impl Deref for DebugOut {
    type Target = Printbuf;
    fn deref(&self) -> &Printbuf { &self.buf }
}

impl DerefMut for DebugOut {
    fn deref_mut(&mut self) -> &mut Printbuf { &mut self.buf }
}

/// A read's waker: its data is the read's buffer, and it's never woken - a
/// suspended file is polled again by the next read.
static READER: RawWakerVTable = RawWakerVTable::new(
    |data| RawWaker::new(data, &READER),
    |_| {},
    |_| {},
    |_| {},
);

type ReadFuture<'a> = dyn Future<Output = Result<(), BchError>> + FreeAnytime + 'a;

/// An open file being read: none once it's been read to the end. Pinned,
/// though it isn't a Pin<KBox>: nothing moves it out of its box.
pub(super) type Reading = Option<KBox<ReadFuture<'static>>>;

/// @obj's file, from its start: show(), then the rest of what it printed.
pub(super) async fn read<F: DebugFile>(obj: &F::Obj) -> Result<(), BchError> {
    let mut out = DebugOut { buf: Printbuf::new(), u: ptr::null_mut() };
    out.attach().await;

    F::show(obj, &mut out).await?;
    out.flush().await
}

/// read::<F>() of an object, boxed, as an open file has it.
///
/// # Safety
/// The object is only dereferenced by read_into(), while it's live.
pub(super) unsafe fn reading(r: KBox<ReadFuture<'_>>) -> Reading {
    Some(unsafe { core::mem::transmute::<KBox<ReadFuture<'_>>, KBox<ReadFuture<'static>>>(r) })
}

/// The next of @r's text that fits in @u.
pub(super) fn read_into(r: &mut Reading, u: &mut UserBuf) -> Result<(), BchError> {
    let Some(fut) = r else { return Ok(()) };

    let waker = unsafe { Waker::new(ptr::from_mut(u).cast_const().cast(), &READER) };
    let fut = unsafe { Pin::new_unchecked(&mut **fut) };
    match fut.poll(&mut Context::from_waker(&waker)) {
        Poll::Pending    => Ok(()),
        Poll::Ready(ret) => {
            *r = None;
            ret
        }
    }
}
