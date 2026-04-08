use std::mem;

/// Compute a bcachefs _IOW ioctl number.
///
/// Equivalent to `_IOW(0xbc, nr, T)` — write direction, bcachefs magic,
/// size encoded from the type parameter.
pub const fn bch_ioc_w<T>(nr: u32) -> libc::Ioctl {
    ((1u32 << 30) | ((mem::size_of::<T>() as u32) << 16) | (0xbcu32 << 8) | nr) as libc::Ioctl
}

/// Compute a bcachefs _IOWR ioctl number.
pub const fn bch_ioc_wr<T>(nr: u32) -> libc::Ioctl {
    ((3u32 << 30) | ((mem::size_of::<T>() as u32) << 16) | (0xbcu32 << 8) | nr) as libc::Ioctl
}
