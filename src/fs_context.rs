//! The Linux fs_context mount API: fsopen(2), fsconfig(2), fsmount(2),
//! move_mount(2).
//!
//! Nothing here is bcachefs-specific; it's the generic new-mount-API plumbing
//! that mount.bcachefs sits on top of.
//!
//! # Why this exists at all
//!
//! mount(2) can only fail with an errno. A filesystem that has something
//! specific to say about why a mount failed has nowhere to put it, so the user
//! gets "Invalid argument" and the real explanation goes to dmesg at best. The
//! fs_context API replaces the single syscall with an object: you open a
//! context, feed it parameters one at a time, ask it to create a superblock,
//! and turn the result into a mount. The context carries a message log, which
//! is the whole point - [`FsContext::drain_log`] is where a filesystem's own
//! account of the failure comes back.
//!
//! # Shape of a mount
//!
//! ```text
//! FsContext::open("bcachefs")?      // fsopen
//!   .set("source", Some(devs))?     // fsconfig, once per parameter
//!   .set("errors", Some("panic"))?
//!   .create()?                      // fsconfig(FSCONFIG_CMD_CREATE)
//!   .fsmount(attrs)?                // -> detached mount fd
//! move_mount(&mnt, target)?         // attach it to the tree
//! ```
//!
//! Setting parameters individually rather than as one comma-joined string is
//! not just tidier: the kernel reports a rejected parameter against the
//! parameter, so errors name the option that was wrong.
//!
//! # Two kinds of mount flag
//!
//! The old MS_* namespace conflates two things the new API separates, and
//! getting this wrong loses flags silently:
//!
//! - **Per-mount** (rdonly, nosuid, nodev, noexec, atime behaviour) become
//!   MOUNT_ATTR_* passed to fsmount(). See [`mount_attrs`].
//! - **Per-superblock** (sync, dirsync, mand, lazytime) have no MOUNT_ATTR_*
//!   equivalent and travel as named fsconfig parameters, which the VFS consumes
//!   in vfs_parse_sb_flag() before the filesystem ever sees them. See
//!   [`sb_flag_params`].
//!
//! # Availability
//!
//! These syscalls landed in Linux 5.2. [`FsContext::open`] returns `Ok(None)`
//! on ENOSYS so callers can fall back to mount(2) rather than refusing to work
//! on an older kernel.
//!
//! Reconfiguring an already-mounted filesystem (MS_REMOUNT) is a different
//! entry point - fspick(2) plus FSCONFIG_CMD_RECONFIGURE - and isn't
//! implemented here.
//!
//! Constants are from uapi linux/mount.h. libc has the syscall numbers but not
//! these; they're stable ABI.

use std::{
    ffi::{CStr, CString},
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
};

const FSOPEN_CLOEXEC:  libc::c_uint = 0x0000_0001;
const FSMOUNT_CLOEXEC: libc::c_uint = 0x0000_0001;

const FSCONFIG_SET_FLAG:   libc::c_uint = 0;
const FSCONFIG_SET_STRING: libc::c_uint = 1;
const FSCONFIG_CMD_CREATE: libc::c_uint = 6;

/// The most a single fsconfig(2) parameter value can carry.
///
/// The kernel copies it with `strndup_user(_value, 256)` (fs/fsopen.c), and
/// strndup_user() returns -EINVAL when the string doesn't fit rather than
/// truncating it - so 255 bytes of value, and no way to send more.
///
/// mount(2) has no equivalent limit: it takes the source through
/// copy_mount_string(), which allows PATH_MAX. So the new API cannot express
/// every mount the old one could, and a caller with a long value has to fall
/// back rather than adapt.
///
/// The refusal also arrives with an empty fs_context log, because it happens in
/// the syscall wrapper before the VFS sees the parameter - worth knowing, since
/// everything else here reports failures through that log.
pub const PARAM_VALUE_MAX: usize = 255;

pub const MOUNT_ATTR_RDONLY:      libc::c_uint = 0x0000_0001;
pub const MOUNT_ATTR_NOSUID:      libc::c_uint = 0x0000_0002;
pub const MOUNT_ATTR_NODEV:       libc::c_uint = 0x0000_0004;
pub const MOUNT_ATTR_NOEXEC:      libc::c_uint = 0x0000_0008;
pub const MOUNT_ATTR_NOATIME:     libc::c_uint = 0x0000_0010;
pub const MOUNT_ATTR_STRICTATIME: libc::c_uint = 0x0000_0020;
pub const MOUNT_ATTR_NODIRATIME:  libc::c_uint = 0x0000_0080;

/// MS_LAZYTIME, which libc doesn't define.
pub const MS_LAZYTIME: libc::c_ulong = 1 << 25;

/// Translate the per-mount MS_* flags to the MOUNT_ATTR_* fsmount() takes.
///
/// Flags with no per-mount equivalent are dropped here on purpose; see
/// [`sb_flag_params`] for where the superblock ones go instead. The two sets
/// are not disjoint: MS_RDONLY is both, and has to be sent both ways.
pub fn mount_attrs(mountflags: libc::c_ulong) -> libc::c_uint {
    const MAP: &[(libc::c_ulong, libc::c_uint)] = &[
        (libc::MS_RDONLY,      MOUNT_ATTR_RDONLY),
        (libc::MS_NOSUID,      MOUNT_ATTR_NOSUID),
        (libc::MS_NODEV,       MOUNT_ATTR_NODEV),
        (libc::MS_NOEXEC,      MOUNT_ATTR_NOEXEC),
        (libc::MS_NOATIME,     MOUNT_ATTR_NOATIME),
        (libc::MS_STRICTATIME, MOUNT_ATTR_STRICTATIME),
        (libc::MS_NODIRATIME,  MOUNT_ATTR_NODIRATIME),
    ];

    MAP.iter()
        .filter(|(ms, _)| mountflags & ms != 0)
        .fold(0, |attrs, (_, attr)| attrs | attr)
}

/// The MS_* flags that are superblock state, as the fsconfig() parameter names
/// the VFS knows them by.
///
/// MS_RDONLY is here *and* in [`mount_attrs`], because mount(2) conflated two
/// things the new API separates: MOUNT_ATTR_RDONLY makes this mount read-only,
/// while fsconfig("ro") sets fc->sb_flags |= SB_RDONLY, which is what tells the
/// filesystem to open its devices read-only. Send only the former and a
/// write-protected device fails the mount outright - the filesystem asks for
/// O_RDWR and gets EACCES, and mount(8)'s read-only retry has nothing to change.
pub fn sb_flag_params(mountflags: libc::c_ulong) -> Vec<&'static str> {
    const MAP: &[(libc::c_ulong, &str)] = &[
        (libc::MS_RDONLY,      "ro"),
        (libc::MS_SYNCHRONOUS, "sync"),
        (libc::MS_DIRSYNC,     "dirsync"),
        (libc::MS_MANDLOCK,    "mand"),
        (MS_LAZYTIME,          "lazytime"),
    ];

    MAP.iter()
        .filter(|(ms, _)| mountflags & ms != 0)
        .map(|(_, name)| *name)
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Error,
    Warning,
    Notice,
}

/// One line from the fs_context message log.
#[derive(Debug, Clone)]
pub struct Message {
    pub level: Level,
    pub text:  String,
}

fn invalid_input(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, what.to_string())
}

/// An open filesystem configuration context: the fd from fsopen(2).
pub struct FsContext {
    fd: OwnedFd,
}

impl FsContext {
    /// fsopen(2).
    ///
    /// `Ok(None)` means the kernel predates the new mount API (ENOSYS) and the
    /// caller should fall back to mount(2). ENODEV means the filesystem type
    /// isn't registered - typically the module isn't loaded.
    pub fn open(fstype: &str) -> io::Result<Option<Self>> {
        let fstype = CString::new(fstype).map_err(|_| invalid_input("filesystem type"))?;

        let fd = unsafe {
            libc::syscall(libc::SYS_fsopen, fstype.as_ptr(), FSOPEN_CLOEXEC)
        } as libc::c_int;

        if fd < 0 {
            let err = io::Error::last_os_error();
            return match err.raw_os_error() {
                Some(libc::ENOSYS) => Ok(None),
                _ => Err(err),
            };
        }

        Ok(Some(Self { fd: unsafe { OwnedFd::from_raw_fd(fd) } }))
    }

    /// fsconfig(2): set one parameter, with a value (`Some`) or as a bare flag
    /// (`None`).
    pub fn set(&self, key: &str, value: Option<&str>) -> io::Result<()> {
        let c_key = CString::new(key).map_err(|_| invalid_input("parameter name"))?;
        let c_value = value
            .map(|v| CString::new(v).map_err(|_| invalid_input("parameter value")))
            .transpose()?;

        let (cmd, value_ptr) = match &c_value {
            Some(v) => (FSCONFIG_SET_STRING, v.as_ptr()),
            None    => (FSCONFIG_SET_FLAG,   std::ptr::null()),
        };

        self.fsconfig(cmd, c_key.as_ptr(), value_ptr)
    }

    /// Ask bcachefs for a status channel, and get the file descriptor back.
    ///
    /// Not a mount option: bcachefs's `parse_param` creates the descriptor and
    /// returns its number as this fsconfig(2) call's return value, which
    /// `vfs_parse_fs_param()` passes back untouched. Must come before
    /// [`create`](Self::create).
    ///
    /// `None` on any kernel that doesn't know the parameter - mounting without
    /// a channel is how it has always worked, so absence isn't a failure.
    pub fn status_fd(&self) -> Option<OwnedFd> {
        let key = CString::new("status_fd").unwrap();

        let ret = unsafe {
            libc::syscall(
                libc::SYS_fsconfig,
                self.fd.as_raw_fd(),
                FSCONFIG_SET_FLAG,
                key.as_ptr(),
                std::ptr::null::<libc::c_char>(),
                0,
            )
        };

        if ret > 0 {
            return Some(unsafe { OwnedFd::from_raw_fd(ret as RawFd) });
        }

        // Zero would mean the parameter was accepted without producing a
        // descriptor, which nothing does - don't invent an fd 0 out of it.
        // Negative is an older module rejecting the name, which also leaves an
        // "Unknown parameter" in the context log; drain it so it isn't reported
        // later as though the mount had complained.
        let _ = self.drain_log();
        None
    }

    /// fsconfig(FSCONFIG_CMD_CREATE): actually create the superblock. This is
    /// where a filesystem does its real work, and where it has the most to say
    /// if it fails.
    pub fn create(&self) -> io::Result<()> {
        self.fsconfig(FSCONFIG_CMD_CREATE, std::ptr::null(), std::ptr::null())
    }

    /// fsmount(2): turn the created superblock into a detached mount. Attach it
    /// with [`move_mount`].
    pub fn fsmount(&self, attrs: libc::c_uint) -> io::Result<OwnedFd> {
        let fd = unsafe {
            libc::syscall(libc::SYS_fsmount, self.fd.as_raw_fd(), FSMOUNT_CLOEXEC, attrs)
        } as libc::c_int;

        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    /// Read out everything the kernel has logged against this context.
    ///
    /// Reading is destructive - each message is returned exactly once - so call
    /// this at the point the messages are wanted, and before dropping the
    /// context. An empty result just means the filesystem didn't say anything,
    /// which is common.
    pub fn drain_log(&self) -> Vec<Message> {
        let mut msgs = Vec::new();
        let mut buf = [0u8; 4096];

        // Each read() yields one message, "<level> <text>\n", and ENODATA once
        // the log is empty. Any other error also ends the loop; there's nothing
        // useful to do about it while already reporting a failure.
        loop {
            let n = unsafe {
                libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len())
            };
            if n <= 0 {
                break;
            }

            let line = String::from_utf8_lossy(&buf[..n as usize]);
            let line = line.trim_end_matches('\n');

            let (level, text) = match line.split_once(' ') {
                Some(("e", text)) => (Level::Error,   text),
                Some(("w", text)) => (Level::Warning, text),
                Some(("i", text)) => (Level::Notice,  text),
                // Unrecognised prefix: keep the line whole rather than eat it.
                _ => (Level::Error, line),
            };

            msgs.push(Message { level, text: text.to_string() });
        }

        msgs
    }

    fn fsconfig(
        &self,
        cmd: libc::c_uint,
        key: *const libc::c_char,
        value: *const libc::c_char,
    ) -> io::Result<()> {
        let ret = unsafe {
            libc::syscall(libc::SYS_fsconfig, self.fd.as_raw_fd(), cmd, key, value, 0)
        };

        if ret < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(())
    }
}

/// move_mount(2): attach a detached mount from [`FsContext::fsmount`] to a path.
///
/// Dropping the mount fd without doing this discards the mount and releases the
/// superblock.
pub fn move_mount(mnt: &OwnedFd, target: &CStr) -> io::Result<()> {
    let ret = unsafe {
        libc::syscall(
            libc::SYS_move_mount,
            mnt.as_raw_fd(), c"".as_ptr(),
            libc::AT_FDCWD,  target.as_ptr(),
            libc::MOVE_MOUNT_F_EMPTY_PATH,
        )
    };

    if ret < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_mount_flags_become_mount_attrs() {
        let attrs = mount_attrs(libc::MS_RDONLY | libc::MS_NOEXEC | libc::MS_NODEV);

        assert_eq!(attrs, MOUNT_ATTR_RDONLY | MOUNT_ATTR_NOEXEC | MOUNT_ATTR_NODEV);
        assert_eq!(mount_attrs(0), 0);
    }

    /// Superblock flags have no MOUNT_ATTR_* equivalent; if they were dropped
    /// here instead of becoming fsconfig parameters they'd vanish silently.
    #[test]
    fn superblock_flags_become_fsconfig_params() {
        let flags = libc::MS_SYNCHRONOUS | libc::MS_DIRSYNC;

        assert_eq!(mount_attrs(flags), 0);
        assert_eq!(sb_flag_params(flags), vec!["sync", "dirsync"]);
    }

    /// Genuinely per-mount flags must not be sent as fsconfig parameters - the
    /// VFS would reject the name and fail the mount.
    #[test]
    fn per_mount_only_flags_are_not_fsconfig_params() {
        assert!(sb_flag_params(libc::MS_NOEXEC | libc::MS_NODEV).is_empty());
    }

    /// MS_RDONLY is both, and the superblock half is the one that matters for a
    /// write-protected device: without fsconfig("ro") the filesystem opens its
    /// devices O_RDWR and the mount fails EACCES, which is generic/050.
    #[test]
    fn rdonly_is_both_a_mount_attr_and_an_sb_param() {
        assert_eq!(mount_attrs(libc::MS_RDONLY), MOUNT_ATTR_RDONLY);
        assert_eq!(sb_flag_params(libc::MS_RDONLY), vec!["ro"]);
    }

    #[test]
    fn interior_nul_is_rejected_not_truncated() {
        let Some(fc) = FsContext::open("bcachefs").ok().flatten() else {
            return; // no new mount API, or no bcachefs module: nothing to test
        };

        assert_eq!(
            fc.set("errors\0panic", None).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
    }
}
