//! The userspace end of a kernel `thread_with_stdio` file descriptor.
//!
//! The kernel hands out one of these whenever an operation needs to talk to a
//! person while it runs: `BCH_IOCTL_FSCK_ONLINE` returns one, and so does the
//! `status_fd` fsconfig(2) parameter on the mount path. Both ends are the same
//! shape - the fd is readable for whatever the filesystem is saying and writable
//! for the answers - so both callers want the same relay, differing only in
//! where the filesystem's side is written.
//!
//! Bytes pass through unchanged in both directions. The kernel writes its
//! questions without a trailing newline, because the answer belongs on the same
//! line, and it reads them back with a readline that waits for one - so neither
//! direction survives being tidied into lines here.
//!
//! Both fds are put in non-blocking mode so that neither direction can stall the
//! other: a question written while nobody is draining must not stop us reading
//! the answer, and vice versa. stdin's original flags are restored on the way
//! out, because it does not belong to us - the mount path has its own prompts to
//! put on it afterwards.
//!
//! A caller may also keep a live display below the conversation - mount draws
//! recovery progress there. The relay drives it rather than the other way round
//! because the relay is what knows when the filesystem is about to print, and
//! two writers taking turns with one cursor is the whole problem.

use std::{
    io,
    os::fd::{AsFd, BorrowedFd},
    time::Duration,
};

use rustix::{
    event::{poll, PollFd, PollFlags, Timespec},
    fs::{fcntl_getfl, fcntl_setfl, OFlags},
};

/// A block of text kept below the conversation and refreshed as it runs.
///
/// The relay owns the cursor: it takes the block down before anything the
/// filesystem says is written, so the message lands on a clean line and ends up
/// above the block rather than through it, and puts the block back afterwards.
/// `draw` is also called every `interval` so a live display keeps moving while
/// there is nothing being said.
pub trait StatusDisplay {
    /// How long to wait for either end to speak before redrawing anyway.
    fn interval(&self) -> Duration;

    /// Take the block down, leaving the cursor where it started.
    fn erase(&mut self) -> io::Result<()>;

    /// Put it back, with current contents.
    fn draw(&mut self) -> io::Result<()>;
}

fn set_nonblocking(fd: BorrowedFd<'_>) -> io::Result<OFlags> {
    let flags = fcntl_getfl(fd)?;
    fcntl_setfl(fd, flags | OFlags::NONBLOCK)?;
    Ok(flags)
}

/// Move what's readable on `rfd` to `wfd`.
///
/// `Ok(true)` on end of file, `Ok(false)` when something moved or there was
/// nothing to move yet.
fn splice(rfd: BorrowedFd<'_>, wfd: BorrowedFd<'_>) -> io::Result<bool> {
    let mut buf = [0u8; 4096];
    let n = match rustix::io::read(rfd, &mut buf) {
        Ok(0) => return Ok(true),
        Ok(n) => n,
        Err(rustix::io::Errno::AGAIN) => return Ok(false),
        Err(e) => return Err(e.into()),
    };

    let mut off = 0;
    while off < n {
        match rustix::io::write(wfd, &buf[off..n]) {
            Ok(w) => off += w,
            Err(rustix::io::Errno::AGAIN) => {
                poll(&mut [PollFd::new(&wfd, PollFlags::OUT)], None)?;
            }
            Err(e) => return Err(e.into()),
        }
    }

    Ok(false)
}

/// Carry the conversation on `fd` until the kernel side is finished with it:
/// what the filesystem says goes to `out`, and stdin goes back to the
/// filesystem.
///
/// Returns when the kernel marks the channel done, which it does on every path
/// out of the operation - so this ends on its own without the caller arranging
/// it.
pub fn relay(
    fd: BorrowedFd<'_>,
    out: BorrowedFd<'_>,
    display: Option<&mut dyn StatusDisplay>,
) -> io::Result<()> {
    let stdin = io::stdin();
    let stdin_flags = set_nonblocking(stdin.as_fd())?;

    let ret = relay_locked(fd, out, stdin.as_fd(), display);

    let _ = fcntl_setfl(stdin.as_fd(), stdin_flags);
    ret
}

fn relay_locked(
    fd: BorrowedFd<'_>,
    out: BorrowedFd<'_>,
    stdin: BorrowedFd<'_>,
    mut display: Option<&mut dyn StatusDisplay>,
) -> io::Result<()> {
    set_nonblocking(fd)?;

    let mut stdin_closed = false;

    loop {
        let mut pollfds = vec![PollFd::new(&fd, PollFlags::IN)];
        if !stdin_closed {
            pollfds.push(PollFd::new(&stdin, PollFlags::IN));
        }

        let timeout = display.as_deref().map(|d| {
            let i = d.interval();
            Timespec { tv_sec: i.as_secs() as _, tv_nsec: i.subsec_nanos() as _ }
        });
        let _ = poll(&mut pollfds, timeout.as_ref());

        if let Some(d) = display.as_deref_mut() {
            d.erase()?;
        }

        let eof = splice(fd, out)?;

        // Our own end running dry is not the end of the conversation: the
        // filesystem may have a great deal left to say.
        if !stdin_closed && splice(stdin, fd)? {
            stdin_closed = true;
        }

        // Leave the terminal as we found it - the caller has its own result to
        // print, and it doesn't belong under a stale progress block.
        if eof {
            return Ok(());
        }

        if let Some(d) = display.as_deref_mut() {
            d.draw()?;
        }
    }
}
