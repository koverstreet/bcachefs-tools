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

/// Owned, zeroed allocation for a flexible-array-member ioctl argument:
/// header `H` followed by trailing entries. The header is read and
/// written through the real struct, and trailing entries through its
/// `__IncompleteArrayField` accessors - the one place that knows the
/// layout is the C definition.
///
/// `H` must be a plain-old-data bindgen ioctl struct (zero-initialized
/// is valid, align <= 8 - asserted at compile time).
pub struct IoctlBuf<H> {
    buf:  Vec<u64>,
    _arg: std::marker::PhantomData<H>,
}

impl<H> IoctlBuf<H> {
    /// Room for the header plus `nr` trailing elements of `T`.
    pub fn new<T>(nr: usize) -> Self {
        const { assert!(std::mem::align_of::<H>() <= 8) };
        let bytes = std::mem::size_of::<H>() + nr * std::mem::size_of::<T>();
        IoctlBuf { buf: vec![0u64; bytes.div_ceil(8)], _arg: std::marker::PhantomData }
    }

    pub fn hdr(&self) -> &H {
        unsafe { &*(self.buf.as_ptr() as *const H) }
    }

    pub fn hdr_mut(&mut self) -> &mut H {
        unsafe { &mut *(self.buf.as_mut_ptr() as *mut H) }
    }

    pub fn as_mut_ptr(&mut self) -> *mut H {
        self.buf.as_mut_ptr() as *mut H
    }

    /// The trailing region as raw bytes, for arguments whose trailing
    /// records are variable-size. Panics if `bytes` overruns the
    /// allocation (a kernel echoing back more than it was given).
    pub fn trailing_bytes(&self, bytes: usize) -> &[u8] {
        let off = std::mem::size_of::<H>();
        assert!(off + bytes <= self.buf.len() * 8);
        unsafe {
            std::slice::from_raw_parts((self.buf.as_ptr() as *const u8).add(off), bytes)
        }
    }
}
