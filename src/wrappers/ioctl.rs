//! Typed ioctl calls over the generated inventory.
//!
//! `bcachefs_kernel::ioctl` (re-exported here) carries one marker type per
//! `_IO*()` define in bcachefs_ioctl.h, binding the opcode to its argument
//! type — the opcode's size bits are computed from the very type these
//! call shapes make you pass, so a call site can't pair the wrong two.
//! This module adds the calls themselves: direction-checked argument
//! passing and one errno-to-io::Error conversion for the whole tree.
//!
//! Positive return values pass through — several bcachefs ioctls return
//! fds (BCH_IOCTL_DATA, BCH_IOCTL_FSCK_*) or counts.

use std::io;
use std::os::fd::{AsFd, AsRawFd};

pub use bcachefs_kernel::ioctl::*;

fn ret(r: libc::c_int) -> io::Result<i32> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

/// _IO: no argument.
pub fn ioctl_none<I: Ioctl<Arg = ()>>(fd: impl AsFd) -> io::Result<i32> {
    ret(unsafe { libc::ioctl(fd.as_fd().as_raw_fd(), I::OPCODE as libc::Ioctl) })
}

/// _IOW: the kernel only reads the argument.
pub fn ioctl_w<I: Ioctl>(fd: impl AsFd, arg: &I::Arg) -> io::Result<i32> {
    const { assert!(I::DIR == 1, "not an _IOW ioctl") };
    ret(unsafe {
        libc::ioctl(fd.as_fd().as_raw_fd(), I::OPCODE as libc::Ioctl, arg as *const I::Arg)
    })
}

/// _IOR/_IOWR: the kernel writes (or updates) the argument.
pub fn ioctl_rw<I: Ioctl>(fd: impl AsFd, arg: &mut I::Arg) -> io::Result<i32> {
    const { assert!(I::DIR & 2 != 0, "not an _IOR/_IOWR ioctl") };
    ret(unsafe {
        libc::ioctl(fd.as_fd().as_raw_fd(), I::OPCODE as libc::Ioctl, arg as *mut I::Arg)
    })
}

/// Argument in a caller-managed allocation — for the ioctls whose argument
/// struct ends in a flexible array member: the caller allocates header +
/// entries and passes a pointer to the header. The opcode's size bits only
/// cover the header (sizeof a FAM struct), matching the C definition.
///
/// # Safety
/// `arg` must point to a live, writable allocation laid out as the kernel
/// expects for `I`: at least `I::Arg`, plus whatever trailing entries its
/// header fields promise.
pub unsafe fn ioctl_ptr<I: Ioctl>(fd: impl AsFd, arg: *mut I::Arg) -> io::Result<i32> {
    ret(libc::ioctl(fd.as_fd().as_raw_fd(), I::OPCODE as libc::Ioctl, arg))
}
