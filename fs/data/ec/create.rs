// SPDX-License-Identifier: GPL-2.0

//! Making stripes - ec/create.c's, as it converts: so far, the widen cache,
//! for reconcile's check pass and its stripes scan, and what the stripe
//! trigger calls.

use crate::c;
use crate::errcode::{ret_to_result, BchError};
use crate::fs::Fs;
use crate::util::alloc::darray_exit;

impl Fs {
    /// How many RW members disk label @disk_label has - durable, taking user
    /// data, with buckets of @sectors - that a stripe there could have a
    /// block on: bch2_disk_label_ec_rw_member_devs()'s, counted.
    pub fn ec_rw_member_devs_nr(&self, disk_label: u8, sectors: u16) -> u32 {
        let mut devs = c::bch_devs_mask::default();
        // SAFETY: our mask, which it fills
        unsafe { c::bch2_disk_label_ec_rw_member_devs(self.as_ptr(), disk_label.into(), &mut devs, sectors.into()) };
        devs.d.weight()
    }

    /// Kick the worker that deletes empty stripes: bch2_do_stripe_deletes().
    pub fn do_stripe_deletes(&self) {
        // SAFETY: plain
        unsafe { c::bch2_do_stripe_deletes(self.as_ptr()) }
    }
}

/// How many RW members each (disk_label, sectors) has - the widening target
/// a stripe there has - looked up once each: C's widen_cache, a darray.
pub struct WidenCache(c::widen_cache);

impl WidenCache {
    /// bch2_widen_cache_init().
    pub fn new() -> Result<Self, BchError> {
        let mut cache = WidenCache(c::widen_cache::default());
        // SAFETY: our cache, an empty darray, as _init() takes it
        ret_to_result(unsafe { c::bch2_widen_cache_init(&mut cache.0) })?;
        Ok(cache)
    }

    /// @disk_label's RW members, for a stripe of @sectors:
    /// bch2_widen_cache_lookup().
    pub fn lookup(&mut self, fs: &Fs, disk_label: u8, sectors: u16) -> Result<u32, BchError> {
        let mut nr_devs = 0;
        // SAFETY: our cache, _init()ed
        ret_to_result(unsafe {
            c::bch2_widen_cache_lookup(&mut self.0, fs.as_ptr().cast(), disk_label, sectors, &mut nr_devs)
        })?;
        Ok(nr_devs)
    }

    /// The can_widen stripe @s should have now: how many more data blocks
    /// its disk label's RW members would let it have.
    pub fn can_widen(&mut self, fs: &Fs, s: &c::Stripe) -> Result<u8, BchError> {
        let nr_devs = self.lookup(fs, s.disk_label, s.sectors.get())?;
        Ok(s.want_can_widen(nr_devs, fs.opts().ec_max_data_blocks.into()))
    }
}

impl Drop for WidenCache {
    fn drop(&mut self) {
        // SAFETY: a darray C grew
        unsafe { darray_exit(&mut self.0) };
    }
}
