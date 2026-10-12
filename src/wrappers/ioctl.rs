//! Typed ioctl calls over the ioctls bcachefs_ioctl.rs defines.
//!
//! Its c_ioctl! lists (re-exported here, with the argument structs) give
//! each ioctl a marker type, binding the opcode to its argument type so a
//! call site can't pair the wrong two. This module adds the calls
//! themselves: the three argument shapes, and one errno-to-io::Error
//! conversion for the whole tree.
//!
//! Positive return values pass through — several bcachefs ioctls return
//! fds (BCH_IOCTL_DATA, BCH_IOCTL_FSCK_*) or counts.

use std::io;
use std::os::fd::{AsFd, AsRawFd};

pub use bcachefs_kernel::cstructs::bcachefs_ioctl::*;
pub use bcachefs_kernel::util::ioctl::Ioctl;
use bcachefs_kernel::util::vstructs::{FlexArray, Plain, UnvalidatedRef, UnvalidatedMut, VStructError};

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
    ret(unsafe {
        libc::ioctl(fd.as_fd().as_raw_fd(), I::OPCODE as libc::Ioctl, arg as *const I::Arg)
    })
}

/// _IOR/_IOWR: the kernel writes (or updates) the argument.
pub fn ioctl_rw<I: Ioctl>(fd: impl AsFd, arg: &mut I::Arg) -> io::Result<i32> {
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

/// Owned, zeroed allocation for an ioctl argument that ends in a flexible
/// array member: header `H`, then what follows it. The header is read and
/// written through the struct. What follows is reached through this buffer,
/// never through the header's `[T; 0]` field, whose reference covers only the
/// header: as an UnvalidatedRef (util/vstructs.rs) for an array the header
/// counts - so a count past the end of the buffer is an error, not a read
/// past it - or as bytes, for a tail of variable-size records.
///
/// `H` must be valid zeroed, and align <= 8 (asserted at compile time).
pub struct IoctlBuf<H> {
    buf:  Vec<u64>,
    _arg: std::marker::PhantomData<H>,
}

impl<H> IoctlBuf<H> {
    /// Room for @bytes, the header's included: at least the header.
    fn alloc(bytes: usize) -> Self {
        const { assert!(std::mem::align_of::<H>() <= 8) };
        let bytes = bytes.max(std::mem::size_of::<H>());
        IoctlBuf { buf: vec![0u64; bytes.div_ceil(8)], _arg: std::marker::PhantomData }
    }

    /// Room for the header and @bytes after it: a tail of variable-size
    /// records.
    pub fn with_trailing_bytes(bytes: usize) -> Self {
        Self::alloc(std::mem::size_of::<H>() + bytes)
    }

    /// The whole buffer.
    fn bytes(&self) -> &[u8] {
        // SAFETY: u64s, which any bytes are
        unsafe { std::slice::from_raw_parts(self.buf.as_ptr().cast(), self.buf.len() * 8) }
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

    /// @bytes after the header, for a tail of variable-size records. Panics
    /// if that's past the end of the buffer: a kernel echoing back more than
    /// it was given.
    pub fn trailing_bytes(&self, bytes: usize) -> &[u8] {
        let off = std::mem::size_of::<H>();
        let have = self.buf.len() * 8;
        assert!(off + bytes <= have, "ioctl tail of {bytes} bytes past {off} overruns its {have} byte buffer");
        &self.bytes()[off..off + bytes]
    }
}

impl<H: FlexArray + Plain> IoctlBuf<H> {
    /// Room for the header and @nr elements of its array.
    pub fn new(nr: usize) -> Self {
        Self::alloc(H::TAIL + nr * std::mem::size_of::<H::Elem>())
    }

    /// The array, as long as the header's count says: an error if the
    /// buffer doesn't hold that many - the kernel and us disagreeing.
    pub fn tail(&self) -> Result<&[H::Elem], VStructError> {
        Ok(UnvalidatedRef::<H>::new(self.bytes())?.tail())
    }

    /// The array, as long as the header's count says - to fill in, having
    /// set that count.
    pub fn tail_mut(&mut self) -> Result<&mut [H::Elem], VStructError> {
        // SAFETY: u64s, which any bytes are
        let bytes = unsafe { std::slice::from_raw_parts_mut(self.buf.as_mut_ptr().cast(), self.buf.len() * 8) };
        Ok(UnvalidatedMut::<H>::new(bytes)?.into_tail())
    }
}
