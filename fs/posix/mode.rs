// SPDX-License-Identifier: GPL-2.0

//! An inode's mode: its file type and permissions, in POSIX's bits - which
//! bi_mode holds on disk, so they're defined here, not taken from the OS.

#![allow(non_camel_case_types)]

use core::fmt;
use core::ops::{BitAnd, BitAndAssign, BitOr, BitOrAssign, Not};
use cstruct_macros::NoUninit;
use pin_init::Zeroable;

#[repr(transparent)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug, Zeroable, NoUninit)]
pub struct umode_t(pub u16);

pub const S_IFMT:   umode_t = umode_t(0o170000);
pub const S_IFSOCK: umode_t = umode_t(0o140000);
pub const S_IFLNK:  umode_t = umode_t(0o120000);
pub const S_IFREG:  umode_t = umode_t(0o100000);
pub const S_IFBLK:  umode_t = umode_t(0o060000);
pub const S_IFDIR:  umode_t = umode_t(0o040000);
pub const S_IFCHR:  umode_t = umode_t(0o020000);
pub const S_IFIFO:  umode_t = umode_t(0o010000);

pub const S_ISUID:   umode_t = umode_t(0o4000);
pub const S_ISGID:   umode_t = umode_t(0o2000);
pub const S_ISVTX:   umode_t = umode_t(0o1000);
pub const S_IRWXUGO: umode_t = umode_t(0o777);

/// A whiteout's, with S_IFCHR: a device 0:0.
pub const WHITEOUT_MODE: umode_t = umode_t(0);

impl umode_t {
    /// Its S_IFMT bits.
    pub const fn file_type(self) -> umode_t {
        umode_t(self.0 & S_IFMT.0)
    }

    pub const fn is_dir(self) -> bool {
        self.file_type().0 == S_IFDIR.0
    }

    pub const fn is_reg(self) -> bool {
        self.file_type().0 == S_IFREG.0
    }

    pub const fn is_lnk(self) -> bool {
        self.file_type().0 == S_IFLNK.0
    }

    /// The dirent type of its file type: S_DT().
    pub const fn d_type(self) -> u8 {
        (self.file_type().0 >> 12) as u8
    }
}

impl BitOr for umode_t {
    type Output = umode_t;
    fn bitor(self, rhs: umode_t) -> umode_t {
        umode_t(self.0 | rhs.0)
    }
}

impl BitOrAssign for umode_t {
    fn bitor_assign(&mut self, rhs: umode_t) {
        self.0 |= rhs.0
    }
}

impl BitAnd for umode_t {
    type Output = umode_t;
    fn bitand(self, rhs: umode_t) -> umode_t {
        umode_t(self.0 & rhs.0)
    }
}

impl BitAndAssign for umode_t {
    fn bitand_assign(&mut self, rhs: umode_t) {
        self.0 &= rhs.0
    }
}

impl Not for umode_t {
    type Output = umode_t;
    fn not(self) -> umode_t {
        umode_t(!self.0)
    }
}

impl fmt::Octal for umode_t {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Octal::fmt(&self.0, f)
    }
}
