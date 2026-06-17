// SPDX-License-Identifier: GPL-2.0

use crate::c;
use crate::errcode::{ret_to_result_void as ret_to_result, BchError};
use crate::fs::Fs;

pub struct DiskReservation<'f> {
    fs:  &'f Fs,
    raw: c::disk_reservation,
}

impl<'f> DiskReservation<'f> {
    pub fn new(fs: &'f Fs) -> Self {
        DiskReservation {
            fs,
            raw: Default::default(),
        }
    }

    pub fn init(fs: &'f Fs, nr_replicas: u32) -> Self {
        DiskReservation {
            fs,
            raw: unsafe { c::bch2_disk_reservation_init(fs.raw, nr_replicas) },
        }
    }

    pub fn get(
        fs:          &'f Fs,
        sectors:     u64,
        nr_replicas: u32,
        flags:       c::bch_reservation_flags,
    ) -> Result<Self, BchError> {
        let mut ret = Self::new(fs);
        ret_to_result(unsafe {
            c::bch2_disk_reservation_get(
                fs.raw,
                &mut ret.raw,
                sectors,
                nr_replicas,
                flags.0 as i32,
            )
        })?;
        Ok(ret)
    }

    pub fn add(&mut self, sectors: u64, flags: c::bch_reservation_flags) -> Result<(), BchError> {
        ret_to_result(unsafe {
            c::bch2_disk_reservation_add(self.fs.raw, &mut self.raw, sectors, flags)
        })
    }

    pub fn as_ptr(&self) -> *const c::disk_reservation {
        &self.raw
    }

    pub fn as_mut_ptr(&mut self) -> *mut c::disk_reservation {
        &mut self.raw
    }

    pub fn sectors(&self) -> u64 {
        self.raw.sectors
    }
}

impl Drop for DiskReservation<'_> {
    fn drop(&mut self) {
        unsafe { c::bch2_disk_reservation_put(self.fs.raw, &mut self.raw) };
    }
}
