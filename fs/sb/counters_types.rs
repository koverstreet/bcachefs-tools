// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::fs::Fs;
use crate::util::Printbuf;
use crate::types::c_default;
use core::ffi::c_uint;
use cstruct_macros::{CStruct, c_const, c_extern, TypeInfo};

c_const! {
    #[c_int]
    pub const NR_RECENT_COUNTERS: u32 = 20;
}

#[repr(C)]
#[derive(Clone, Copy, CStruct, TypeInfo)]
pub struct bch_fs_counters {
    pub mount: [u64; c::BCH_COUNTER_NR as usize],
    #[c("u64 __percpu *now")]
    pub now: *mut u64,

    pub recent: [[u64; c::BCH_COUNTER_NR as usize]; c::NR_RECENT_COUNTERS as usize],
    pub work: c::delayed_work,
}
c_default!(bch_fs_counters);

c_extern! {
    pub fn bch2_counter_read(c: *mut c::bch_fs, idx: c_uint) -> u64;
    pub fn bch2_counter_reset(c: *mut c::bch_fs, idx: c_uint);
    pub fn bch2_sb_recent_counters_to_text(out: *mut c::printbuf, c: *mut c::bch_fs_counters);
}

impl Fs {
    /// Persistent counter @idx (BCH_COUNTER_*), every CPU's count summed.
    pub fn counter_read(&self, idx: usize) -> u64 {
        assert!(idx < c::BCH_COUNTER_NR as usize);
        unsafe { bch2_counter_read(self.as_ptr(), idx as c_uint) }
    }

    /// Zero persistent counter @idx.
    pub fn counter_reset(&self, idx: usize) {
        unsafe { bch2_counter_reset(self.as_ptr(), idx as c_uint) }
    }
}

impl bch_fs_counters {
    /// The counters that have changed recently.
    pub fn recent_to_text(&self, out: &mut Printbuf) {
        unsafe { bch2_sb_recent_counters_to_text(out.as_raw(), core::ptr::from_ref(self).cast_mut()) }
    }
}
