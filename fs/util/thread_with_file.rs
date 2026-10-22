// SPDX-License-Identifier: GPL-2.0

//! A kthread whose handle is a file descriptor: userspace talks to it by
//! reading and writing the fd, and closing the fd stops it.
//!
//! Two layers. ThreadWithFile<T> is a kthread running on a T, behind a file
//! whose ops are T's own (util/file.rs) - the data job's progress file.
//! Stdio<T> makes the file a stream: a StdioRedirect between file and
//! thread, for fsck - and for mounting's status fd, a stdio file with no
//! thread, the mounting task printing to it itself.
//!
//! Lifetimes. The file owns everything, in Inner: the thread first, so it
//! drops first. release() closes the redirect, which wakes a thread blocked
//! on it, stops the thread - kthread_stop() waits for it to exit - and only
//! then drops what it ran on. What the thread's function owns, a filesystem
//! it opened or a ref, it drops when it returns; if the file couldn't be
//! made, the function is dropped unrun. A file with no thread has nothing
//! for release() to wait on, so whoever prints to it holds a ref on the
//! file for as long as they may.
//!
//! Startup: the thread's made stopped, the fd reserved and the file built,
//! the thread woken, and only then the fd installed - from which moment
//! userspace may close it, so nothing's touched after.
//!
//! The redirect, keeping C's semantics:
//!
//!  - Two buffers, input and output. Userspace writes input and the thread
//!    reads it; the thread writes output and userspace reads it. The
//!    thread is input's only reader: a line it's seen stays till it takes it.
//!  - done: the thread's finished, or the file's closing. It closes both
//!    ways: reads drain what's left, then get EOF; writes get EPIPE. What
//!    the thread returned is what close(2) returns.
//!  - The soft limit, STDIO_REDIRECT_BUFSIZE: a write may go past it rather
//!    than split a line the thread is waiting for - waiting_for_line - and
//!    the thread's own messages go in whole or not at all.
//!  - Under the lock, a spinlock: each buffer is a Fifo<u8>, grown with
//!    GFP_NOWAIT, or reserved before the lock where it mustn't fail. Copies
//!    to and from userspace are made there too, copy_*_user_nofault() after
//!    faulting in: what doesn't copy stays where it was, and a copy is only
//!    of what was faulted in - past that, it could fail every time.
//!  - The published length: each buffer's, set by its guard, BufGuard, as
//!    the lock's released, whatever was done to it. Waits and poll() read it
//!    without the lock.
//!  - Waits and wakes: every change to a buffer - data in, room made, done -
//!    wakes its EventCount, and its PollQueue for poll(). A wait's condition
//!    reads the published length; readline's looks for a whole line under
//!    the lock instead, so one wait carries the whole timeout. A condition
//!    runs in the sleep's task state, which a spinlock keeps and an rtlock
//!    saves. readline sleeps TASK_IDLE: someone typing is no hung task.

use crate::c;
use crate::errcode::BchError;
use crate::locking::{Guard, Lock};
use crate::new_lock;
use crate::types::Fifo;
use crate::util::alloc::{flags, kbox_pin_init, KBox, KVVec};
use crate::util::ffi::c_char;
use crate::util::file::{file_data, File, FileData, FileOps, NewFile, PollQueue, PollTable, UserBuf};
use crate::util::futex::EventCount;
use crate::util::kernel::current_comm;
use crate::util::kthread::Kthread;
use crate::util::projections::Volatile;
use crate::util::sleep::Sleep;
use core::ops::{Deref, DerefMut};
use core::pin::Pin;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use cstruct_macros::vtable;
use kernel::ffi::{c_int, c_long, c_uint, c_ulong};
use pin_init::{pin_data, pin_init, pinned_drop, PinInit};

/* ThreadWithFile */

pub trait ThreadFile: FileOps {
    /// At close, before the thread's stopped: wake it from whatever it's
    /// blocked on that only the file would end.
    fn closing(&self) {}
}

#[pin_data(PinnedDrop)]
pub struct Inner<T: ThreadFile> {
    thread: Option<Kthread>,
    #[pin]
    data:   T,
}

#[pinned_drop]
impl<T: ThreadFile> PinnedDrop for Inner<T> {
    fn drop(self: Pin<&mut Self>) {
        self.data.closing();
    }
}

impl<T: ThreadFile> FileData for Inner<T> {
    type Ops = T;

    fn ops(&self) -> &T {
        &self.data
    }
}

/// Freed if dropped before run() or get_fd() gives it to a file.
pub struct ThreadWithFile<T: ThreadFile>(Pin<KBox<Inner<T>>>);

impl<T: ThreadFile> ThreadWithFile<T> {
    pub fn new(data: impl PinInit<T, BchError>) -> Result<Self, BchError> {
        let init = pin_init!(Inner::<T> { thread: None, data <- data }? BchError);
        Ok(ThreadWithFile(kbox_pin_init(init)?))
    }

    pub fn data(&self) -> &T {
        &self.0.data
    }

    /// The kthread's named for the caller.
    pub fn run(mut self, f: impl FnOnce(&T) + Send + 'static) -> Result<c_int, BchError> {
        // SAFETY: the thread runs only while the file lives - release()
        // stops it before the data's dropped - or not at all: if there's no
        // file, it's stopped unrun, as the data is dropped
        let data: &'static T = unsafe { &*(&raw const self.0.data) };

        let thread = Kthread::create(format_args!("{}", current_comm()), move || f(data))?;
        *self.0.as_mut().project().thread = Some(thread);

        let file = NewFile::new(self.0)?;
        if let Some(t) = &file.data().thread {
            t.wake();
        }
        Ok(file.install())
    }

    /// # Safety
    /// @file is an open ThreadWithFile<T>'s.
    pub unsafe fn from_file<'a>(file: *mut c::file) -> &'a T {
        unsafe { file_data::<Inner<T>>(file) }.ops()
    }
}

/* StdioRedirect */

const STDIO_REDIRECT_BUFSIZE: usize = 4096;

#[derive(Default)]
struct Buf {
    data:             Fifo<u8>,
    waiting_for_line: bool,
}

#[pin_data]
struct StdioBuf {
    #[pin]
    buf:  Lock<c::spinlock_t, Buf, Volatile<usize>>,
    events: EventCount,
    #[pin]
    poll: PollQueue,
}

impl StdioBuf {
    fn new() -> impl PinInit<Self> {
        pin_init!(StdioBuf {
            buf  <- new_lock!(Buf::default(), ::pin_init::zeroed()),
            events: EventCount::new(),
            poll <- PollQueue::new(),
        })
    }

    fn len(&self) -> usize {
        self.buf.get()
    }

    fn lock(&self) -> BufGuard<'_> {
        BufGuard(self.buf.lock())
    }

    fn wake(&self) {
        self.events.wake();
        self.poll.wake();
    }

    fn wait_interruptible(&self, mut cond: impl FnMut() -> bool) -> Result<(), BchError> {
        self.events.wait_until(Sleep::INTERRUPTIBLE, || cond().then_some(()))
            .map_err(|_| BchError::from(c::ERESTARTSYS))
    }
}

struct BufGuard<'a>(Guard<'a, c::spinlock_t, Buf, Volatile<usize>>);

impl Deref for BufGuard<'_> {
    type Target = Buf;

    fn deref(&self) -> &Buf {
        &self.0
    }
}

impl DerefMut for BufGuard<'_> {
    fn deref_mut(&mut self) -> &mut Buf {
        &mut self.0
    }
}

impl Drop for BufGuard<'_> {
    fn drop(&mut self) {
        let len = self.0.data.len();
        self.0.published().set(len);
    }
}

#[pin_data]
pub struct StdioRedirect {
    #[pin]
    input:  StdioBuf,
    #[pin]
    output: StdioBuf,
    done:   AtomicBool,
}

impl StdioRedirect {
    fn new() -> impl PinInit<Self> {
        pin_init!(StdioRedirect {
            input  <- StdioBuf::new(),
            output <- StdioBuf::new(),
            done:     AtomicBool::new(false),
        })
    }

    pub fn as_c(&self) -> *mut c::stdio_redirect {
        core::ptr::from_ref(self).cast_mut().cast()
    }

    /// # Safety
    /// @p is as_c()'s, of a redirect that's live for 'a.
    pub unsafe fn from_c<'a>(p: *mut c::stdio_redirect) -> &'a Self {
        unsafe { &*p.cast::<Self>() }
    }

    fn done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    pub fn set_done(&self) {
        self.done.store(true, Ordering::Release);
        self.input.wake();
        self.output.wake();
    }

    fn has_data(&self, buf: &StdioBuf) -> bool {
        buf.len() != 0 || self.done()
    }

    fn has_room(&self, buf: &StdioBuf) -> bool {
        buf.len() < STDIO_REDIRECT_BUFSIZE || self.done()
    }

    /// The thread's output, written: @msg in whole, or an error - EPIPE if
    /// the file's closed, and if @nonblocking, EAGAIN rather than wait for
    /// the reader to make room.
    pub fn write(&self, nonblocking: bool, msg: &[u8]) -> Result<(), BchError> {
        let out = &self.output;
        loop {
            if self.done() {
                return Err(c::EPIPE.into());
            }

            let wrote = {
                let mut b = out.lock();
                b.data.reserve(msg.len(), flags::GFP_NOWAIT).is_ok() &&
                    b.data.extend_from_slice(msg).is_ok()
            };

            if wrote {
                out.wake();
                return Ok(());
            }

            if nonblocking {
                return Err(c::EAGAIN.into());
            }

            out.wait_interruptible(|| self.has_room(out))?;
        }
    }

    /// A line of input, newline and all, into @line - waiting at most
    /// @timeout jiffies, MAX_SCHEDULE_TIMEOUT being forever: ETIME if it runs
    /// out, EPIPE if the file's closed.
    pub fn readline_timeout(&self, line: &mut KVVec<u8>, timeout: c_ulong) -> Result<(), BchError> {
        let input = &self.input;
        loop {
            let n = input.events.wait_until(Sleep::IDLE.timeout(timeout as _), || {
                if self.done() {
                    return Some(None);
                }
                let mut b = input.lock();
                let nl = b.data.iter().position(|&ch| ch == b'\n');
                if nl.is_none() {
                    b.waiting_for_line = true;
                }
                nl.map(|nl| Some(nl + 1))
            }).map_err(|_| BchError::from(c::ETIME))?.ok_or(BchError::from(c::EPIPE))?;

            if n > line.capacity() {
                line.clear();
                line.reserve(n, flags::GFP_KERNEL)?;
                continue;
            }

            line.clear();
            let mut b = input.lock();
            b.data.drain_with(n, |s| line.extend_from_slice(s, flags::GFP_NOWAIT).is_ok());
            b.waiting_for_line = false;
            drop(b);

            input.wake();
            return Ok(());
        }
    }

    fn read_user(&self, u: &mut UserBuf, nonblock: bool) -> Result<(), BchError> {
        let out = &self.output;

        if !nonblock {
            out.wait_interruptible(|| self.has_data(out))?;
        } else if !self.has_data(out) {
            return Err(c::EAGAIN.into());
        }

        while u.len() != 0 && out.len() != 0 {
            let writeable = u.fault_in_writeable();
            if writeable == 0 {
                return Err(c::EFAULT.into());
            }

            let n = out.lock().data.drain_with(writeable, |s| u.copy_to_nofault(s));

            if n != 0 {
                out.wake();
            }
        }
        Ok(())
    }

    fn write_user(&self, u: &mut UserBuf, nonblock: bool) -> Result<(), BchError> {
        let input = &self.input;

        while u.len() != 0 {
            if self.done() {
                return Err(c::EPIPE.into());
            }

            let readable = u.fault_in_readable();
            if readable == 0 {
                return Err(c::EFAULT.into());
            }

            let n = {
                let mut b = input.lock();
                let mut makeroom = readable;
                if !b.waiting_for_line || b.data.iter().any(|&ch| ch == b'\n') {
                    makeroom = makeroom.min(STDIO_REDIRECT_BUFSIZE.saturating_sub(b.data.len()));
                }
                let _ = b.data.reserve(makeroom, flags::GFP_NOWAIT);

                let n = readable.min(b.data.free());
                // SAFETY: a stretch copied is filled
                unsafe { b.data.fill_with(n, |d| u.copy_from_nofault(d)) };
                n
            };

            if n != 0 {
                input.wake();
            } else if nonblock {
                return Err(c::EAGAIN.into());
            } else {
                input.wait_interruptible(|| self.has_room(input))?;
            }
        }
        Ok(())
    }

    fn poll(&self, file: &File, pt: &PollTable<'_>) -> u32 {
        pt.wait(file, &self.output.poll);
        pt.wait(file, &self.input.poll);

        let mut mask = 0;
        if self.has_data(&self.output) {
            mask |= c::EPOLLIN;
        }
        if self.has_room(&self.input) {
            mask |= c::EPOLLOUT;
        }
        if self.done() {
            mask |= c::EPOLLHUP | c::EPOLLERR;
        }
        mask
    }
}

/* Stdio: a ThreadWithFile on a StdioRedirect */

pub trait StdioFile: Send + Sync + 'static {
    fn ioctl(&self, _cmd: c_uint, _arg: c_ulong) -> c_long {
        -(c::ENOTTY as c_long)
    }
}

impl StdioFile for () {}

#[pin_data]
pub struct Stdio<T> {
    #[pin]
    pub redirect: StdioRedirect,
    ret:          AtomicI32,
    #[pin]
    pub data:     T,
}

#[vtable]
impl<T: StdioFile> FileOps for Stdio<T> {
    fn read(&self, file: &File, buf: &mut UserBuf) -> Result<(), BchError> {
        self.redirect.read_user(buf, file.nonblock())
    }

    fn write(&self, file: &File, buf: &mut UserBuf) -> Result<(), BchError> {
        self.redirect.write_user(buf, file.nonblock())
    }

    fn poll(&self, file: &File, pt: &PollTable<'_>) -> u32 {
        self.redirect.poll(file, pt)
    }

    fn flush(&self, _file: &File) -> c_int {
        self.ret.load(Ordering::Relaxed)
    }

    fn ioctl(&self, _file: &File, cmd: c_uint, arg: c_ulong) -> c_long {
        self.data.ioctl(cmd, arg)
    }
}

impl<T: StdioFile> ThreadFile for Stdio<T> {
    fn closing(&self) {
        self.redirect.set_done();
    }
}

impl<T: StdioFile> ThreadWithFile<Stdio<T>> {
    pub fn new_stdio(data: impl PinInit<T, BchError>) -> Result<Self, BchError> {
        Self::new(pin_init!(Stdio::<T> {
            redirect <- StdioRedirect::new(),
            ret:        AtomicI32::new(0),
            data     <- data,
        }? BchError))
    }

    pub fn stdio(&self) -> &StdioRedirect {
        &self.data().redirect
    }

    pub fn run_stdio(self, f: impl FnOnce(&T, &StdioRedirect) -> c_int + Send + 'static)
        -> Result<c_int, BchError>
    {
        self.run(move |s: &Stdio<T>| {
            s.ret.store(f(&s.data, &s.redirect), Ordering::Relaxed);
            s.redirect.set_done();
        })
    }

    /// No thread: the caller prints, and calls stdio().set_done() when it's
    /// finished. The fd, and the file ref to hold while it may print.
    pub fn get_fd(self) -> Result<(c_int, NonNull<c::file>), BchError> {
        let file = NewFile::new(self.0)?;
        let r = file.get_file();
        Ok((file.install(), r))
    }
}

/* For C's prints */

/// # Safety
/// @stdio is a live redirect's, @buf @len bytes.
#[cstruct_macros::c_export]
pub unsafe extern "C" fn bch2_stdio_redirect_write(stdio: *mut c::stdio_redirect, nonblocking: bool,
                                                   buf: *const c_char, len: usize) -> isize {
    let stdio = unsafe { StdioRedirect::from_c(stdio) };
    let msg = unsafe { core::slice::from_raw_parts(buf.cast::<u8>(), len) };
    match stdio.write(nonblocking, msg) {
        Ok(())  => len as isize,
        Err(e)  => -(e.raw() as isize),
    }
}

/// The fsck question's answer, a line into @line: 0, or a -errcode - ETIME
/// if @timeout jiffies passed first.
///
/// # Safety
/// @stdio is a live redirect's; @line a darray.
#[cstruct_macros::c_export]
pub unsafe extern "C" fn bch2_stdio_redirect_readline_timeout(stdio: *mut c::stdio_redirect,
                                                              line: *mut c::darray_char,
                                                              timeout: c_ulong) -> c_int {
    let stdio = unsafe { StdioRedirect::from_c(stdio) };
    // SAFETY: C's darray, its buffer kvmalloc()'d
    let mut line = unsafe { crate::util::alloc::DArrayLoan::new(&mut *line) };
    crate::errcode::ret_to_c(stdio.readline_timeout(&mut line, timeout))
}
