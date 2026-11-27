// SPDX-License-Identifier: GPL-2.0

//! A device number: what a device inode's bi_dev holds on disk, and what the
//! filesystem knows its block devices by - a 12 bit major above a 20 bit
//! minor. That's the kernel's in-memory layout too, but the kernel is free to
//! change its own: the ports convert by major and minor, never by the bits.

#![allow(non_camel_case_types)]

use core::fmt;
use cstruct_macros::NoUninit;
use pin_init::Zeroable;

#[repr(transparent)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug, Zeroable, NoUninit)]
pub struct dev_t(u32);

const MINORBITS: u32 = 20;
const MINORMASK: u32 = (1 << MINORBITS) - 1;

impl dev_t {
    /// No device: a file's, or a non-device inode's.
    pub const NONE: dev_t = dev_t(0);

    pub const fn new(major: u32, minor: u32) -> dev_t {
        dev_t(major << MINORBITS | minor)
    }

    pub const fn major(self) -> u32 {
        self.0 >> MINORBITS
    }

    pub const fn minor(self) -> u32 {
        self.0 & MINORMASK
    }

    /// As bi_dev holds it.
    pub const fn from_disk(v: u32) -> dev_t {
        dev_t(v)
    }

    pub const fn to_disk(self) -> u32 {
        self.0
    }

    /// One as userspace has it - stat(), mknod(), our ioctls: new_encode_dev()'s
    /// layout, the minor's low byte, the major, then the rest of the minor, so
    /// that old 8:8 numbers kept their values. glibc's 64 bit dev_t adds bits
    /// above 32, which a 12:20 number never reaches.
    pub const fn from_user(dev: u64) -> dev_t {
        let dev = dev as u32;
        dev_t::new((dev & 0xfff00) >> 8, (dev & 0xff) | ((dev >> 12) & 0xfff00))
    }

    pub const fn to_user(self) -> u64 {
        let (major, minor) = (self.major(), self.minor());
        ((minor & 0xff) | (major << 8) | ((minor & !0xff) << 12)) as u64
    }
}

impl fmt::Display for dev_t {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.major(), self.minor())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Against glibc's encoding, which userspace's numbers are in: (1,3) is
    /// /dev/null, and the rest reach each field's bits above the old 8:8.
    #[test]
    fn user_encoding() {
        for (major, minor) in [(0, 0), (1, 3), (8, 17), (259, 0x12345), (4095, 0xfffff)] {
            let user = rustix::fs::makedev(major, minor);
            let dev = dev_t::from_user(user);
            assert_eq!((dev.major(), dev.minor()), (major, minor), "{user:#x}");
            assert_eq!(dev.to_user(), user, "{major}:{minor}");
        }
        assert_eq!(dev_t::from_user(0x103).to_disk(), 0x100003);
    }
}
