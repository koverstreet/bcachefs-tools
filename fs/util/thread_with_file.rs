// SPDX-License-Identifier: GPL-2.0

//! A kthread whose handle is a file descriptor: userspace talks to it by
//! reading and writing the fd, and closing the fd stops it.
//!
//! ThreadWithFile<T> is a kthread running on a T, behind a file whose ops are
//! T's (file.rs). Stdio<T> makes the file a stream, a StdioRedirect: fsck's,
//! and the mount's status fd - a stdio file with no thread, which the
//! mounting task prints to itself, holding the Stdio while it may.
//!
//! The thread drops first: release() closes the redirect, waking a thread
//! blocked on it, then stops the thread - kthread_stop() waits for it - so a
//! close returns once the thread's gone. The fd is installed last, the thread
//! already woken: from then userspace may close it.
//!
//! The redirect, as C's:
//!  - The thread is input's only reader: a line it's seen stays till it takes
//!    it. Input may grow past STDIO_REDIRECT_BUFSIZE while it holds no whole
//!    line. Output takes a message whole while it's below it; past it, a
//!    writer waits for the reader if it may sleep, or the message is dropped -
//!    and the filesystem's are, unless its Reader is waited for.
//!  - done - the thread's finished, or the file's closing - closes both ways:
//!    reads drain what's left, then EOF; writes get EPIPE. What the thread
//!    returned is what close(2) returns.
//!  - A buffer grows under its spinlock, and is copied to and from userspace
//!    there, copy_*_user_nofault() after faulting in - a copy only of what was
//!    faulted in, or it could fail every time.
//!  - A wait's condition takes the lock - readline's finds its line there, so
//!    one wait carries the whole timeout - in the sleep's task state, which a
//!    spinlock keeps and an rtlock saves. readline sleeps TASK_IDLE: someone
//!    typing is no hung task.

use crate::c;
use crate::data_structures::fifo::FifoRead;
use crate::errcode::{BchError, Errcode};
use crate::locking::{Locked, SpinLockIrq};
use crate::new_lock;
use crate::types::Fifo;
use crate::util::alloc::{kbox_pin_init, Arc, KBox, Vec};
use kernel::alloc::flags::GFP_KERNEL;
use kernel::init::InPlaceInit;
use crate::util::ffi::c_char;
use crate::util::rcu::{FreeAnytime, NothingToDetach};
use crate::os::file::{file_data, Any, File, FileData, FileOps, NewFile, PollQueue, PollTable, UserBuf};
use crate::util::futex::EventCount;
use crate::os::kernel::{current_comm, HZ};
use core::time::Duration;
use crate::os::kthread::Kthread;
use crate::os::sched::may_sleep;
use crate::util::sleep::Sleep;
use core::pin::Pin;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use cstruct_macros::vtable;
use crate::util::ffi::{c_long, c_ulong};
use pin_init::{pin_data, pin_init, pinned_drop, PinInit};

/* ThreadWithFile */

pub trait ThreadFile: FileOps<Data = Inner<Self>, Inode = Any> + FreeAnytime + NothingToDetach {
    /// At close, before the thread's stopped: wake it from whatever it's
    /// blocked on that only the file would end.
    fn closing(&self) {}
}

#[pin_data(PinnedDrop)]
pub struct Inner<T: ThreadFile> {
    thread: Option<Kthread>,
    data:   Arc<T>,
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
        let data = Arc::try_pin_init(data, GFP_KERNEL)?;
        Ok(ThreadWithFile(kbox_pin_init(pin_init!(Inner::<T> { thread: None, data }? BchError))?))
    }

    /// The kthread's named for the caller.
    pub fn run(mut self, f: impl FnOnce(&T) + Send + FreeAnytime + NothingToDetach + 'static) -> Result<i32, BchError> {
        let data = self.0.data.clone();
        let thread = Kthread::create(format_args!("{}", current_comm()), move |_| f(&data))?;
        *self.0.as_mut().project().thread = Some(thread);

        let file = NewFile::new(self.0)?;
        if let Some(t) = &file.data().thread {
            t.wake();
        }
        Ok(file.install())
    }
}

/* StdioRedirect */

const STDIO_REDIRECT_BUFSIZE: usize = 4096;

#[derive(Default)]
struct Buf {
    data:  Fifo<u8>,
    /// The whole lines in @data, input's: its newlines.
    lines: usize,
}

#[pin_data]
struct StdioBuf {
    #[pin]
    buf:  Locked<SpinLockIrq, Buf>,
    events: EventCount,
    #[pin]
    poll: PollQueue,
}

impl StdioBuf {
    fn new() -> impl PinInit<Self> {
        pin_init!(StdioBuf {
            buf  <- new_lock!(Buf::default()),
            events: EventCount::new(),
            poll <- PollQueue::new(),
        })
    }

    fn len(&self) -> usize {
        self.buf.lock().data.len()
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

/// Whether the filesystem's messages wait for room in output: fsck's tool reads
/// all it prints; the status fd's reader, a boot splash, may never.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Reader { Waited, Optional }

#[pin_data]
pub struct StdioRedirect {
    #[pin]
    input:  StdioBuf,
    #[pin]
    output: StdioBuf,
    done:   AtomicBool,
    reader: Reader,
}

impl StdioRedirect {
    fn new(reader: Reader) -> Result<Arc<Self>, BchError> {
        Arc::try_pin_init(pin_init!(StdioRedirect {
            input  <- StdioBuf::new(),
            output <- StdioBuf::new(),
            done:     AtomicBool::new(false),
            reader,
        }? BchError), GFP_KERNEL)
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

    /// has_room(), for input: which may grow past the limit for a line.
    fn input_has_room(&self) -> bool {
        let b = self.input.buf.lock();
        b.data.len() < STDIO_REDIRECT_BUFSIZE || b.lines == 0 || self.done()
    }

    /// The thread's own output, or a question: @msg in whole, once output's
    /// below the limit - waiting for the reader to make room if this context
    /// may sleep, EAGAIN if not. EPIPE if the file's closed.
    pub fn write(&self, msg: &[u8]) -> Result<(), BchError> {
        self.put(msg, may_sleep())
    }

    /// A message of the filesystem's: as write(), but waiting only for a
    /// Reader::Waited.
    pub fn print(&self, msg: &[u8]) -> Result<(), BchError> {
        self.put(msg, self.reader == Reader::Waited && may_sleep())
    }

    fn put(&self, msg: &[u8], wait: bool) -> Result<(), BchError> {
        let out = &self.output;
        loop {
            if self.done() {
                return Err(Errcode::EPIPE_thread_with_file_write.into());
            }

            let wrote = self.has_room(out) && {
                let mut b = out.buf.lock();
                b.data.reserve(msg.len()).is_ok() &&
                    b.data.extend_from_slice(msg).is_ok()
            };

            if wrote {
                out.wake();
                return Ok(());
            }

            if !wait {
                return Err(Errcode::EAGAIN_thread_with_file_write.into());
            }

            out.wait_interruptible(|| self.has_room(out))?;
        }
    }

    /// A line of input, newline and all, into @line - waiting at most @timeout,
    /// None being forever: ETIME if it runs out, EPIPE if the file's closed.
    pub fn readline(&self, line: &mut Vec<u8>, timeout: Option<Duration>) -> Result<(), BchError> {
        let input = &self.input;
        let jiffies = timeout.map_or(usize::MAX, |t| (t.as_millis() as usize).saturating_mul(HZ) / 1000);
        loop {
            let n = input.events.wait_until(Sleep::IDLE.timeout(jiffies), || {
                if self.done() {
                    return Some(None);
                }
                let b = input.buf.lock();
                (b.lines != 0).then(|| {
                    let nl = b.data.iter().position(|&ch| ch == b'\n').expect("a line counted, and no newline");
                    Some(nl + 1)
                })
            }).map_err(|_| BchError::from(Errcode::ETIME_thread_with_file_readline))?
              .ok_or(BchError::from(Errcode::EPIPE_thread_with_file_readline))?;

            if n > line.capacity() {
                line.clear();
                line.reserve(n)?;
                continue;
            }

            line.clear();
            let mut b = input.buf.lock();
            b.data.drain_with(n, |s| line.extend_from_slice(s).is_ok());
            b.lines -= 1;
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
            return Err(Errcode::EAGAIN_thread_with_file_read_user.into());
        }

        while u.len() != 0 && out.len() != 0 {
            let writeable = u.fault_in_writeable();
            if writeable == 0 {
                return Err(Errcode::EFAULT_thread_with_file_read_user.into());
            }

            let n = out.buf.lock().data.drain_with(writeable, |s| u.copy_to_nofault(s));

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
                return Err(Errcode::EPIPE_thread_with_file_write_user.into());
            }

            let readable = u.fault_in_readable();
            if readable == 0 {
                return Err(Errcode::EFAULT_thread_with_file_write_user.into());
            }

            let n = {
                let mut b = input.buf.lock();
                let mut makeroom = readable;
                if b.lines != 0 {
                    makeroom = makeroom.min(STDIO_REDIRECT_BUFSIZE.saturating_sub(b.data.len()));
                }
                let _ = b.data.reserve(makeroom);

                let (before, n) = (b.data.len(), readable.min(b.data.free()));
                b.data.fill_with(n, |d| u.copy_from_nofault(d));
                b.lines += b.data.iter().skip(before).filter(|&&ch| ch == b'\n').count();
                n
            };

            if n != 0 {
                input.wake();
            } else if nonblock {
                return Err(Errcode::EAGAIN_thread_with_file_write_user.into());
            } else {
                input.wait_interruptible(|| self.input_has_room())?;
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
        if self.input_has_room() {
            mask |= c::EPOLLOUT;
        }
        if self.done() {
            mask |= c::EPOLLHUP | c::EPOLLERR;
        }
        mask
    }
}

/* Stdio: a ThreadWithFile on a StdioRedirect */

pub trait StdioFile: Send + Sync + FreeAnytime + NothingToDetach + 'static {
    fn ioctl(&self, _cmd: u32, _arg: c_ulong) -> c_long {
        -(c::ENOTTY as c_long)
    }
}

impl StdioFile for () {}

#[pin_data]
pub struct Stdio<T> {
    pub redirect: Arc<StdioRedirect>,
    ret:          AtomicI32,
    #[pin]
    pub data:     T,
}

#[vtable]
impl<T: StdioFile> FileOps for Stdio<T> {
    type Data = Inner<Self>;
    type Inode = Any;

    fn read(&self, file: &File<Self::Data>, buf: &mut UserBuf) -> Result<(), BchError> {
        self.redirect.read_user(buf, file.nonblock())
    }

    fn write(&self, file: &File<Self::Data>, buf: &mut UserBuf) -> Result<(), BchError> {
        self.redirect.write_user(buf, file.nonblock())
    }

    fn poll(&self, file: &File<Self::Data>, pt: PollTable<'_>) -> u32 {
        self.redirect.poll(file, &pt)
    }

    /// What close(2) returns: the thread's status once it's done - stored before
    /// done is set - and EINTR for a close that comes first, stopping it.
    fn flush(&self, _file: &File<Self::Data>) -> i32 {
        if self.redirect.done() { self.ret.load(Ordering::Relaxed) } else { -(c::EINTR as i32) }
    }

    fn unlocked_ioctl(&self, _file: &File<Self::Data>, cmd: u32, arg: c_ulong) -> c_long {
        self.data.ioctl(cmd, arg)
    }
}

impl<T: StdioFile> ThreadFile for Stdio<T> {
    fn closing(&self) {
        self.redirect.set_done();
    }
}

impl<T: StdioFile> ThreadWithFile<Stdio<T>> {
    pub fn new_stdio(data: impl PinInit<T, BchError>, reader: Reader) -> Result<Self, BchError> {
        let redirect = StdioRedirect::new(reader)?;
        Self::new(pin_init!(Stdio::<T> {
            redirect,
            ret:  AtomicI32::new(0),
            data <- data,
        }? BchError))
    }

    pub fn run_stdio(self, f: impl FnOnce(&T, &Arc<StdioRedirect>) -> i32 + Send + FreeAnytime + NothingToDetach + 'static)
        -> Result<i32, BchError>
    {
        self.run(move |s: &Stdio<T>| {
            s.ret.store(f(&s.data, &s.redirect), Ordering::Relaxed);
            s.redirect.set_done();
        })
    }

    /// No thread: the caller prints, and sets the redirect done when it's
    /// finished. The fd, and the Stdio, to hold while it may print.
    pub fn get_fd(self) -> Result<(i32, Arc<Stdio<T>>), BchError> {
        let data = self.0.data.clone();
        Ok((NewFile::new(self.0)?.install(), data))
    }
}
