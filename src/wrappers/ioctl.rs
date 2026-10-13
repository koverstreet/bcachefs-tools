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
use bcachefs_kernel::util::vstructs::{from_bytes, from_bytes_mut, read_past, CountedFfi, FlexArray, Plain, VStructError};

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
/// written through the struct; what follows, through this buffer - never the
/// header's `[T; 0]` field, whose reference covers only the header:
///  - get(): a #[counted] header's value, its array as long as the count says
///    - a count past the end of the buffer an error, not a read past it;
///  - tail_strided(): an array of elements the kernel sized;
///  - trailing_bytes(): a tail of variable-size records.
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

    /// The array of a reply whose elements are @stride bytes apart - the
    /// kernel's size for them, where the header says, not necessarily ours -
    /// each copied: zero past @stride, the kernel's past ours dropped. An
    /// error if the buffer doesn't hold as many as the header's count says.
    pub fn tail_strided(&self, stride: usize) -> Result<Vec<H::Elem>, VStructError> {
        assert!(stride > 0, "{}: elements of no bytes", std::any::type_name::<H>());
        let want = H::TAIL + self.hdr().nr() * stride;
        let have = self.bytes().len();
        let tail = self.bytes().get(H::TAIL..want).ok_or(VStructError::Overrun {
            ty: std::any::type_name::<H>(), want, have,
        })?;
        Ok(tail.chunks_exact(stride).map(|e| read_past(e, 0)).collect())
    }
}

impl<H: CountedFfi> IoctlBuf<H> {
    /// The value, a #[counted] family's - its header, and its array as long
    /// as the header's count says: an error if the buffer doesn't hold that
    /// many, the kernel and us disagreeing. Unvalidated: the kernel's bytes.
    pub fn get(&self) -> Result<&H::Unvalidated, VStructError> {
        from_bytes::<H::Unvalidated>(self.bytes())
    }

    /// get(), to fill in its array, having set the header's count: the
    /// count is the buffer's to set, through hdr_mut().
    pub fn get_mut(&mut self) -> Result<&mut H::Unvalidated, VStructError> {
        // SAFETY: u64s, which any bytes are
        let bytes = unsafe { std::slice::from_raw_parts_mut(self.buf.as_mut_ptr().cast(), self.buf.len() * 8) };
        from_bytes_mut::<H::Unvalidated>(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Query = bch_ioctl_snapshot_tree_query_v2;

    /// A reply of @nr snapshot nodes @stride bytes apart, as a kernel whose
    /// nodes are that size writes it: node i's id i + 1, every other byte of
    /// it 0xff.
    fn reply(nr: u32, stride: usize) -> IoctlBuf<Query> {
        let mut buf = IoctlBuf::<Query>::new(4);
        buf.hdr_mut().nr = nr;
        // SAFETY: u64s, which any bytes are
        let bytes: &mut [u8] = unsafe {
            std::slice::from_raw_parts_mut(buf.buf.as_mut_ptr().cast(), buf.buf.len() * 8)
        };
        for i in 0..nr as usize {
            let at = Query::TAIL + i * stride;
            bytes[at..at + stride].fill(0xff);
            bytes[at..at + 4].copy_from_slice(&(i as u32 + 1).to_ne_bytes());
        }
        buf
    }

    /// Nodes smaller than ours - an older kernel's, 40 bytes, before
    /// nr_keys - are read at their own stride, the fields they lack zero.
    #[test]
    fn tail_strided_shorter_nodes() {
        let nodes = reply(3, 40).tail_strided(40).unwrap();
        assert_eq!(nodes.iter().map(|n| n.id).collect::<Vec<_>>(), [1, 2, 3]);
        assert!(nodes.iter().all(|n| n.sectors == u64::MAX && n.nr_keys == 0 && n.key_bytes == 0));

        // stepping by our size instead, the second is read from the middle of
        // the second and third
        let ours = std::mem::size_of::<bch_ioctl_snapshot_node_v2>();
        assert_ne!(reply(3, 40).tail_strided(ours).unwrap()[1].id, 2);
    }

    /// At our own size, it's the array as it is.
    #[test]
    fn tail_strided_our_nodes() {
        let stride = std::mem::size_of::<bch_ioctl_snapshot_node_v2>();
        let buf = reply(3, stride);
        let ids = |ns: &[bch_ioctl_snapshot_node_v2]| ns.iter().map(|n| n.id).collect::<Vec<_>>();
        assert_eq!(ids(&buf.tail_strided(stride).unwrap()), [1, 2, 3]);
    }

    /// A count past the buffer is an error, not a read past it.
    #[test]
    fn tail_strided_overrun() {
        let stride = std::mem::size_of::<bch_ioctl_snapshot_node_v2>();
        let mut buf = reply(1, stride);
        buf.hdr_mut().nr = 5;
        assert!(matches!(buf.tail_strided(stride), Err(VStructError::Overrun { .. })));
    }

    type Counters = bcachefs_kernel::util::vstructs::Ffi<bch_ioctl_query_counters>;

    /// get() is the array as long as the header's count says, not as long as
    /// the buffer.
    #[test]
    fn get_counted() {
        let mut buf = IoctlBuf::<Counters>::new(4);
        buf.hdr_mut().nr = 3;
        buf.get_mut().unwrap().d.copy_from_slice(&[1, 2, 3]);
        assert_eq!(buf.get().unwrap().d, [1, 2, 3]);

        buf.hdr_mut().nr = 4;
        assert_eq!(buf.get().unwrap().d, [1, 2, 3, 0]);
    }

    /// A count past the buffer is an error, not a read past it.
    #[test]
    fn get_overrun() {
        let mut buf = IoctlBuf::<Counters>::new(4);
        buf.hdr_mut().nr = 5;
        assert!(matches!(buf.get(), Err(VStructError::Overrun { want: 48, have: 40, .. })));
        assert!(matches!(buf.get_mut(), Err(VStructError::Overrun { .. })));
    }
}
