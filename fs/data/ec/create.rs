// SPDX-License-Identifier: GPL-2.0

//! Making stripes - ec/create.c's, as it converts: so far, the widen cache,
//! for reconcile's check pass and its stripes scan.

use crate::c;
use crate::data::ec::trigger::{stripe_widen_target_nr_data, stripe_widen_value};
use crate::errcode::{ret_to_result, BchError};
use crate::fs::Fs;
use crate::util::alloc::darray_exit;

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
        // unsigned, as C's - nr_redundant past nr_blocks is damage
        let nr_data = u32::from(s.nr_blocks).wrapping_sub(s.nr_redundant.into());
        Ok(stripe_widen_value(
            stripe_widen_target_nr_data(nr_devs, s.nr_redundant.into(), fs.opts().ec_max_data_blocks.into()),
            nr_data))
    }
}

impl Drop for WidenCache {
    fn drop(&mut self) {
        // SAFETY: a darray C grew
        unsafe { darray_exit(&mut self.0) };
    }
}
