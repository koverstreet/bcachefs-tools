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
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::init::chardev::{fs_ioctl, ioctl_ret};
use crate::locking::Lock;
use crate::new_lock;
use crate::util::ioctl::Ioctl;
use crate::util::thread_with_file::{Stdio, StdioFile, ThreadWithFile};
use core::ffi::c_void;
use core::mem::size_of;
use core::ptr::NonNull;
use kernel::ffi::{c_int, c_long, c_uint, c_ulong};
use kernel::uaccess::UserPtr;
use pin_init::{pin_data, pin_init};

#[pin_data]
struct StatusFd {
    /// The filesystem coming up on this channel. The mounting thread sets it
    /// around bch2_fs_start() and clears it after, because that is the only
    /// window where we know it's alive - the fd outlives the mount either way.
    ///
    /// A mutex, not a spinlock, because the ioctl hands the filesystem to
    /// bch2_fs_ioctl(), which sleeps: the lock has to be held across that
    /// call, or the mounting thread could return from bch2_fs_start() and
    /// tear it down underneath it. The only waiter is that thread, once, at
    /// the end of the mount.
    #[pin]
    fs: Lock<c::mutex, Option<NonNull<Fs>>>,
}

// SAFETY: the filesystem's every thread's, and only touched under @fs's lock
unsafe impl Send for StatusFd {}
unsafe impl Sync for StatusFd {}

impl StdioFile for StatusFd {
    fn ioctl(&self, cmd: c_uint, arg: c_ulong) -> c_long {
        let mut status = c::bch_ioctl_recovery_status::default();
        {
            let fs = self.fs.lock();
            let Some(fs) = *fs else {
                return -(c::ENODEV as c_long);
            };
            // SAFETY: set only while it's alive, and we hold the lock
            let fs = unsafe { fs.as_ref() };

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
            if cmd != <c::BCH_IOCTL_RECOVERY_STATUS as Ioctl>::OPCODE {
                return ioctl_ret(fs_ioctl(fs, cmd, UserPtr::from_addr(arg as usize)));
            }

            // Whoever's asking is drawing it: keep it off the console.
            // SAFETY: a flag C reads as READ_ONCE()
            unsafe { core::ptr::write_volatile(&raw mut (*fs.as_ptr()).stdio_progress_reader, true) };

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
        }

        // SAFETY: copy_to_user() checks the caller's buffer is theirs
        if unsafe {
            c::copy_to_user(arg as *mut c_void, (&raw const status).cast(),
                            size_of::<c::bch_ioctl_recovery_status>() as _)
        } != 0 {
            -(c::EFAULT as c_long)
        } else {
            0
        }
    }
}

/// The mount's ref on a status channel's file: userspace holds the other, by
/// its fd, and the file frees the channel when both are gone. Dropped when
/// the mount's over, however it went: the channel is closed, so a reader gets
/// EOF rather than waiting for a filesystem that isn't coming.
pub(crate) struct StatusFile(NonNull<c::file>);

impl StatusFile {
    /// A new status channel: its fd, for userspace, and the mount's ref.
    pub(crate) fn open() -> Result<(c_int, StatusFile), BchError> {
        let status = ThreadWithFile::new_stdio(pin_init!(StatusFd {
            fs <- new_lock!(None),
        }? BchError))?;
        let (fd, file) = status.get_fd()?;
        Ok((fd, StatusFile(file)))
    }

    fn status(&self) -> &Stdio<StatusFd> {
        // SAFETY: open()'s file, which this holds a ref on
        unsafe { ThreadWithFile::<Stdio<StatusFd>>::from_file(self.0.as_ptr()) }
    }

    /// @fs starts up with the channel - what it prints for the user, and its
    /// questions, go to it, and its ioctl answers for @fs - until the guard
    /// returned drops: recovery's over, however it went.
    pub(crate) fn attach<'a>(&'a self, fs: &'a Fs) -> Attached<'a> {
        // SAFETY: as C's: set while the mount's in recovery, which is the
        // mounting thread
        unsafe {
            (*fs.as_ptr()).stdio = self.status().redirect.as_c();
            (*fs.as_ptr()).stdio_user_only = true;
        }
        *self.status().data.fs.lock() = Some(NonNull::from(fs));
        Attached { file: self, fs }
    }
}

impl Drop for StatusFile {
    fn drop(&mut self) {
        self.status().redirect.set_done();
        unsafe { c::fput(self.0.as_ptr()) };
    }
}

/// A filesystem the channel is for: StatusFile::attach()'s.
pub(crate) struct Attached<'a> {
    file: &'a StatusFile,
    fs:   &'a Fs,
}

impl Drop for Attached<'_> {
    fn drop(&mut self) {
        // Clearing the channel's filesystem first is what makes the reader
        // flag safe to clear after it: the ioctl only sets it having seen a
        // filesystem under this lock, so no further ioctl can set it.
        *self.file.status().data.fs.lock() = None;

        let fs = self.fs.as_ptr();
        unsafe {
            (*fs).stdio = core::ptr::null_mut();
            (*fs).stdio_user_only = false;
            core::ptr::write_volatile(&raw mut (*fs).stdio_progress_reader, false);
        }
    }
}
