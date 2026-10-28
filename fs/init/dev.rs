// SPDX-License-Identifier: GPL-2.0

//! Devices: dev.c's, as it converts.
//!
//! So far devs_list_to_text(), and a bch_dev's memory: Rust allocates it, as
//! bch_fs's, so its
//! Rust-typed fields have initializers - the bucket bitmaps' Locks - and it's
//! dropped whole when it's freed: after C's teardown, which leaves what it
//! freed nulled for those fields' Drops to find.

use crate::c;
use crate::fs::Fs;
use crate::util::rcu::read_lock;
use crate::util::Printbuf;
use cstruct_macros::c_export;

/// The memory of a new bch_dev of @c: bch_dev::init()'s - its Rust-typed
/// fields' initializers run, everything else zeroed - for __bch2_dev_alloc()
/// to go on with. NULL if out of memory.
#[c_export]
pub fn bch2_dev_alloc_mem(c: &crate::util::ffi::Opaque<c::bch_fs>) -> *mut c::bch_dev {
    crate::util::alloc::alloc_pinned(c::bch_dev::init(crate::fs::Fs::from_c(c)))
        .unwrap_or(core::ptr::null_mut())
}

/// Drop @ca and free it, from its kobject's release.
///
/// # Safety
/// @ca is from bch2_dev_alloc_mem(), and nothing uses it after.
#[c_export]
pub unsafe fn bch2_dev_free_mem(ca: *mut c::bch_dev) {
    unsafe { crate::util::alloc::free_pinned(ca) }
}

impl Fs {
    /// @devs by name, space separated; one there isn't, by its number.
    #[cold]
    pub fn devs_list_to_text(&self, out: &mut Printbuf, devs: &c::bch_devs_list) {
        // room first: under RCU it can only allocate atomically
        out.make_room_gfp(1024, c::GFP_KERNEL);
        let mut out = out.atomic();
        let rcu = read_lock();

        for (i, &dev) in devs.as_slice().iter().enumerate() {
            if i != 0 {
                write!(out, " ");
            }
            match self.dev_rcu(dev.into(), &rcu) {
                Some(ca) => out.write_bytes(ca.name().to_bytes()),
                None     => write!(out, "(invalid device {dev})"),
            }
        }
    }
}

#[cold]
#[c_export]
pub fn bch2_devs_list_to_text(out: &mut Printbuf, c: &Fs, d: &c::bch_devs_list) {
    c.devs_list_to_text(out, d)
}
