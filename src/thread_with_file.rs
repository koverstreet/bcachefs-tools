//! The userspace end of a kernel `thread_with_stdio` file descriptor.
//!
//! The kernel hands out one of these whenever an operation needs to talk to a
//! person while it runs: `BCH_IOCTL_FSCK_ONLINE` returns one, and so does the
//! `status_fd` fsconfig(2) parameter on the mount path. Both ends are the same
//! shape - the fd is readable for whatever the filesystem is saying and writable
//! for the answers - so both callers want the same relay, differing only in
//! where the filesystem's side is written.
//!
//! What the filesystem says passes through unchanged, but for its questions:
//! those come framed (fs/util/prompt.rs), and are put here as any of ours are -
//! on the terminal, or through systemd's agents - the answer written back as a
//! line. stdin goes to the filesystem only as those answers: forwarded as it
//! came, a line typed ahead would answer the next question before it was
//! shown, and every answer after it would be one question late.
//!
//! A caller may also keep a live display below the conversation - mount draws
//! recovery progress there. The relay drives it rather than the other way round
//! because the relay is what knows when the filesystem is about to print, and
//! two writers taking turns with one cursor is the whole problem.

use std::{
    io,
    os::fd::BorrowedFd,
    time::Duration,
};

use bcachefs_kernel::util::prompt::{self, RECORD_END, RECORD_START};
use log::warn;
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

/// All of @buf to @fd, waiting for room where it's non-blocking.
fn write_all(fd: BorrowedFd<'_>, buf: &[u8]) -> io::Result<()> {
    let mut off = 0;
    while off < buf.len() {
        match rustix::io::write(fd, &buf[off..]) {
            Ok(w) => off += w,
            Err(rustix::io::Errno::AGAIN) => {
                poll(&mut [PollFd::new(&fd, PollFlags::OUT)], None)?;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// The filesystem's side: where what it says goes, and what it's said so far
/// of a question it's part way through.
struct Conversation<'a> {
    fd:       BorrowedFd<'a>,
    out:      BorrowedFd<'a>,
    question: Option<Vec<u8>>,
}

impl Conversation<'_> {
    /// What's readable on the fd, passed on - and a question in it, answered.
    /// `Ok(true)` on end of file.
    fn hear(&mut self) -> io::Result<bool> {
        let mut buf = [0u8; 4096];
        let n = match rustix::io::read(self.fd, &mut buf) {
            Ok(0) => return Ok(true),
            Ok(n) => n,
            Err(rustix::io::Errno::AGAIN) => return Ok(false),
            Err(e) => return Err(e.into()),
        };

        for chunk in buf[..n].split_inclusive(|&b| b == RECORD_START || b == RECORD_END) {
            let (text, mark) = match chunk.split_last() {
                Some((&m, text)) if m == RECORD_START || m == RECORD_END => (text, Some(m)),
                _ => (chunk, None),
            };
            match &mut self.question {
                Some(q) => q.extend_from_slice(text),
                None    => write_all(self.out, text)?,
            }
            match mark {
                Some(RECORD_START) => self.question = Some(Vec::new()),
                Some(RECORD_END)   => if let Some(q) = self.question.take() { self.answer(&q)? },
                _ => {}
            }
        }
        Ok(false)
    }

    /// A question that couldn't be put still gets its reply - none - or the
    /// filesystem would wait on it forever.
    fn answer(&self, q: &[u8]) -> io::Result<()> {
        let mut reply = prompt::answer_record(q).unwrap_or_else(|e| {
            warn!("putting the filesystem's question: {e}");
            prompt::UNANSWERED.to_string()
        });
        reply.push('\n');
        write_all(self.fd, reply.as_bytes())
    }
}

/// Carry the conversation on `fd` until the kernel side is finished with it:
/// what the filesystem says goes to `out`, and its questions are put, their
/// answers going back.
///
/// Returns when the kernel marks the channel done, which it does on every path
/// out of the operation - so this ends on its own without the caller arranging
/// it.
pub fn relay(
    fd: BorrowedFd<'_>,
    out: BorrowedFd<'_>,
    mut display: Option<&mut dyn StatusDisplay>,
) -> io::Result<()> {
    // Non-blocking: the poll below times out for the display, and reading
    // after it mustn't wait
    fcntl_setfl(fd, fcntl_getfl(fd)? | OFlags::NONBLOCK)?;

    let mut c = Conversation { fd, out, question: None };

    loop {
        let timeout = display.as_deref().map(|d| {
            let i = d.interval();
            Timespec { tv_sec: i.as_secs() as _, tv_nsec: i.subsec_nanos() as _ }
        });
        let _ = poll(&mut [PollFd::new(&fd, PollFlags::IN)], timeout.as_ref());

        if let Some(d) = display.as_deref_mut() {
            d.erase()?;
        }

        // Leave the terminal as we found it - the caller has its own result to
        // print, and it doesn't belong under a stale progress block.
        if c.hear()? {
            return Ok(());
        }

        if let Some(d) = display.as_deref_mut() {
            d.draw()?;
        }
    }
}
