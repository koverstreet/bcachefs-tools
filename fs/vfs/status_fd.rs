// SPDX-License-Identifier: GPL-2.0

//! The status fd: mount.bcachefs's channel to the filesystem coming up, which
//! it asks for with the "status_fd" fsconfig parameter. The filesystem's
//! questions and hard errors come out of it, its answers go in, and its
//! ioctl answers what the filesystem would - recovery's progress, notably,
//! for a caller drawing it.
//!
//! A stdio file with no thread: the mounting task does the work, printing
//! as it goes. The channel outlives the fs_context - userspace still holds
//! the fd - so the mount holds a ref on the file for as long as anything may
//! print there, and the file frees the channel when both are gone.

use crate::c;
use crate::bcachefs_ioctl::{IoctlRecoveryStatus, BCH_IOCTL_RECOVERY_STATUS};
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::init::chardev::{fs_ioctl, ioctl_ret};
use crate::util::{loan::Loan, vstructs::plain_bytes, ioctl::Ioctl};
use crate::os::file::File;
use crate::util::{log::Only, thread_with_file::{Inner, Stdio, StdioFile, ThreadWithFile}};
use kernel::sync::aref::ARef;
use core::sync::atomic::Ordering::Relaxed;
use core::ops::ControlFlow;
use kernel::uaccess::{UserPtr, UserSlice};
use pin_init::{pin_data, pin_init};

#[pin_data]
struct StatusFd {
    /// The filesystem coming up on this channel. The mounting thread lends it
    /// for Fs::start(), because that is the only window where we know it's
    /// alive - the fd outlives the mount either way.
    ///
    /// The loan's lock is a mutex, because the ioctl hands the filesystem to
    /// fs_ioctl(), which sleeps: the lock has to be held across that
    /// call, or the mounting thread could return from Fs::start() and
    /// tear it down underneath it. The only waiter is that thread, once, at
    /// the end of the mount.
    #[pin]
    fs: Loan<Fs>,
}

impl StdioFile for StatusFd {
    fn ioctl(&self, cmd: u32, arg: usize) -> isize {
        let status = self.fs.with(|fs| {
            let Some(fs) = fs else {
                return ControlFlow::Break(-(c::ENODEV as isize));
            };
            let mut status = IoctlRecoveryStatus::default();

            // Everything else this filesystem answers, it answers here: the
            // chardev's ioctl exists to turn a minor back into a bch_fs, and
            // we already have one. This is the only handle on a filesystem
            // that hasn't finished mounting, so it's the only way to ask it
            // anything - bcachefs<N>-ctl exists by now, but nothing knows the
            // minor.
            //
            // Safe to pass everything through: the dispatch answers the
            // read-only queries and then refuses anything else until
            // BCH_FS_started, which isn't set until recovery is done.
            if cmd != <BCH_IOCTL_RECOVERY_STATUS as Ioctl>::OPCODE {
                return ControlFlow::Break(ioctl_ret(fs_ioctl(fs, cmd, UserPtr::from_addr(arg as usize))));
            }

            // Whoever's asking is drawing it: keep it off the console.
            fs.stdio.progress_reader.store(true, Relaxed);

            let r = &fs.recovery;
            {
                let _runner = r.runner.lock();
                status.passes_scheduled_ephemeral.v[0] = r.runner.scheduled_passes_ephemeral.get();
                status.passes_complete.v[0]            = r.runner.passes_complete.get();
                status.passes_remaining.v[0]           = r.runner.current_passes.get();
                status.pass                            = r.runner.current_pass.get() as u32;
            }

            status.passes_scheduled_sb.v[0] = fs.sb.recovery_passes_required.get();
            let (units, seen, total) = r.progress.status();
            (status.units, status.seen, status.total) = (units.0, seen, total);
            ControlFlow::Continue(status)
        });
        let status = match status {
            ControlFlow::Continue(status) => status,
            ControlFlow::Break(ret) => return ret,
        };

        match UserSlice::new(UserPtr::from_addr(arg as usize), size_of_val(&status)).writer()
            .write_slice(plain_bytes(&status)) {
            Ok(()) => 0,
            Err(_) => -(c::EFAULT as isize),
        }
    }
}

/// The mount's ref on a status channel's file: userspace holds the other, by
/// its fd, and the file frees the channel when both are gone. Dropped when
/// the mount's over, however it went: the channel is closed, so a reader gets
/// EOF rather than waiting for a filesystem that isn't coming.
pub(crate) struct StatusFile(ARef<File<Inner<Stdio<StatusFd>>>>);

impl StatusFile {
    /// A new status channel: its fd, for userspace, and the mount's ref.
    pub(crate) fn open() -> Result<(i32, StatusFile), BchError> {
        let status = ThreadWithFile::new_stdio(pin_init!(StatusFd {
            fs <- Loan::new(),
        }? BchError))?;
        let (fd, file) = status.get_fd()?;
        Ok((fd, StatusFile(file)))
    }

    fn status(&self) -> &Stdio<StatusFd> {
        self.0.ops()
    }

    /// @f - @fs starting up - with the channel: what @fs prints for the user,
    /// and its questions, go to it, and its ioctl answers for @fs.
    pub(crate) fn attached<R>(&self, fs: &Fs, f: impl FnOnce() -> R) -> R {
        let _attached = fs.stdio.attach(&self.status().redirect, Only::User);

        // The loan over before the guard drops is what makes the reader flag
        // safe to clear as it does: the ioctl only sets it having seen a
        // filesystem lent, so no further ioctl can set it.
        self.status().data.fs.lend(fs, f)
    }
}

impl Drop for StatusFile {
    fn drop(&mut self) {
        self.status().redirect.set_done();
    }
}
