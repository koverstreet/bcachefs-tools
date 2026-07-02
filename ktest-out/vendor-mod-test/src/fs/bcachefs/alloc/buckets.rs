// SPDX-License-Identifier: GPL-2.0

use crate::c;
use crate::errcode::{ret_to_result_void as ret_to_result, BchError};
use crate::fs::Fs;
use core::cell::UnsafeCell;

/// A disk space reservation.
///
/// The raw reservation lives behind an `UnsafeCell`: the commit path mutates it
/// (in C) through the pointer installed on the transaction, while the holder may
/// still inspect or grow it — shared, aliased, mutated-by-the-callee. `&mut`
/// would falsely claim exclusivity the commit doesn't honor, so the mutating
/// methods take `&self` and a reservation can be lent to `commit_do` (by shared
/// ref) across many commits while the caller retains it.
pub struct DiskReservation<'f> {
    fs:  &'f Fs,
    raw: UnsafeCell<c::disk_reservation>,
}

impl<'f> DiskReservation<'f> {
    pub fn new(fs: &'f Fs) -> Self {
        DiskReservation {
            fs,
            raw: UnsafeCell::new(Default::default()),
        }
    }

    pub fn init(fs: &'f Fs, nr_replicas: u32) -> Self {
        DiskReservation {
            fs,
            raw: UnsafeCell::new(unsafe { c::bch2_disk_reservation_init(fs.raw, nr_replicas) }),
        }
    }

    pub fn get(
        fs:          &'f Fs,
        sectors:     u64,
        nr_replicas: u32,
        flags:       c::bch_reservation_flags,
    ) -> Result<Self, BchError> {
        let ret = Self::new(fs);
        ret_to_result(unsafe {
            c::bch2_disk_reservation_get(
                fs.raw,
                ret.raw.get(),
                sectors,
                nr_replicas,
                flags.0 as i32,
            )
        })?;
        Ok(ret)
    }

    pub fn add(&self, sectors: u64, flags: c::bch_reservation_flags) -> Result<(), BchError> {
        ret_to_result(unsafe {
            c::bch2_disk_reservation_add(self.fs.raw, self.raw.get(), sectors, flags)
        })
    }

    pub fn as_ptr(&self) -> *const c::disk_reservation {
        self.raw.get()
    }

    pub fn as_mut_ptr(&self) -> *mut c::disk_reservation {
        self.raw.get()
    }

    pub fn sectors(&self) -> u64 {
        unsafe { (*self.raw.get()).sectors }
    }
}

impl Drop for DiskReservation<'_> {
    fn drop(&mut self) {
        unsafe { c::bch2_disk_reservation_put(self.fs.raw, self.raw.get()) };
    }
}
