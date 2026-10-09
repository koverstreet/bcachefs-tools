// SPDX-License-Identifier: GPL-2.0

//! ioctl numbers, as the kernel's _IO*() macros make them, and the trait each
//! ioctl's marker type implements: its number bound to its argument type, so
//! a call can't pair the wrong two.
//!
//! An ioctl number packs a direction, a type (bcachefs's is 0xbc), a number
//! and the argument's size: asm-generic/ioctl.h's _IOC(), the same packing on
//! every architecture. What varies is five constants - the widths of the size
//! and direction fields, and the direction values - which powerpc, mips,
//! sparc, alpha and parisc override in their asm/ioctl.h. Those come from
//! linux-raw-sys, bindgen's view of each target's own headers; nothing here
//! restates them. A hand-written opcode() that had asm-generic's layout
//! everywhere got ppc64le ENOTTY (#904).
//!
//! Userspace only, for now: the kernel crate's equivalent, kernel::ioctl,
//! has asm-generic's constants on powerpc from 6.4, where it arrived, through
//! 7.2 - its uapi bindings included asm-generic/ioctl.h ahead of the arch's,
//! until cdfcaa36ac93 - and no Rust in the kernel build makes an ioctl call
//! yet.

#![allow(non_snake_case)]

use core::mem::size_of;
use linux_raw_sys::general as ioc;

/// An ioctl: its number, and its argument's type - () for _IO.
pub trait Ioctl {
    const OPCODE: u32;
    type Arg;
}

const fn _IOC(dir: u32, ty: u32, nr: u32, size: usize) -> u32 {
    assert!(ty <= ioc::_IOC_TYPEMASK && nr <= ioc::_IOC_NRMASK);
    assert!(size <= ioc::_IOC_SIZEMASK as usize, "ioctl argument too big for the size field");

    (dir << ioc::_IOC_DIRSHIFT)
        | (ty << ioc::_IOC_TYPESHIFT)
        | (nr << ioc::_IOC_NRSHIFT)
        | ((size as u32) << ioc::_IOC_SIZESHIFT)
}

/// No argument.
pub const fn _IO(ty: u32, nr: u32) -> u32 {
    _IOC(ioc::_IOC_NONE, ty, nr, 0)
}

/// The kernel writes the argument.
pub const fn _IOR<T>(ty: u32, nr: u32) -> u32 {
    _IOC(ioc::_IOC_READ, ty, nr, size_of::<T>())
}

/// The kernel reads the argument.
pub const fn _IOW<T>(ty: u32, nr: u32) -> u32 {
    _IOC(ioc::_IOC_WRITE, ty, nr, size_of::<T>())
}

/// The kernel reads and writes the argument.
pub const fn _IOWR<T>(ty: u32, nr: u32) -> u32 {
    _IOC(ioc::_IOC_READ | ioc::_IOC_WRITE, ty, nr, size_of::<T>())
}
