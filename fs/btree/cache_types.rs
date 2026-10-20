// SPDX-License-Identifier: GPL-2.0

//! What Rust calls of btree/cache.h - C gets it as prototypes, in
//! btree/cache_gen.h, which is generated from this file: see fs/types/lib.rs.

#![allow(non_camel_case_types)]

use crate::cstructs::c;
use crate::fs::Fs;
use crate::util::ffi::c_char;
use crate::util::{Printbuf, ToText};
use core::ffi::c_uint;
use cstruct_macros::c_extern;

c_extern! {
    pub fn bch2_btree_id_str(btree: c::btree_id) -> *const c_char;
    pub fn bch2_btree_id_to_text(out: *mut c::printbuf, btree: c::btree_id);
    pub fn bch2_btree_id_level_to_text(out: *mut c::printbuf, btree: c::btree_id, level: c_uint);
    pub fn bch2_btree_node_to_text(out: *mut c::printbuf, c: *mut c::bch_fs, b: *const c::btree);
    pub fn bch2_btree_cache_to_text(out: *mut c::printbuf, bc: *const c::bch_fs_btree_cache);
    pub fn bch2_btree_cache_unpin(c: *mut c::bch_fs);
    pub fn bch2_btree_cache_pin(trans: *mut c::btree_trans, leaf_mask: u64, interior_mask: u64,
                                start: c::bbpos, end: c::bbpos, pinned_end: *mut c::bbpos)
                                -> core::ffi::c_int;
    pub static bch2_btree_node_flags: [*const c_char; 0];
}

impl ToText for c::bch_fs_btree_cache {
    fn to_text(&self, out: &mut Printbuf) {
        unsafe { bch2_btree_cache_to_text(out.as_raw(), self) }
    }
}

impl c::bch_fs_btree_cache {
    /// Ask the shrinker to free @nr nodes, as memory pressure would.
    pub fn shrink(&self, nr: core::ffi::c_ulong) {
        unsafe { crate::util::kernel::shrinker_scan(self.live[0].shrink, nr) }
    }
}

impl c::btree_cache_list {
    pub fn nr(&self) -> usize {
        self.nr_clean + self.nr_dirty
    }
}

impl Fs {
    /// Bytes of btree nodes cached.
    pub fn btree_cache_size(&self) -> u64 {
        let bc = &self.btree.cache;
        ((bc.live[0].nr() + bc.live[1].nr()) * self.opts.btree_node_size as usize) as u64
    }
}
