// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::fs::Fs;
use crate::cstructs::c;
use crate::types::c_default;
use crate::util::{Printbuf, ToText};
use core::ffi::{c_int, c_uint, c_ulong};
use core::sync::atomic::AtomicIsize;
use core::sync::atomic::Ordering::Relaxed;
use cstruct_macros::{CStruct, TypeInfo, c_export, c_extern};
use crate::format::Bpos;

#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_fs_btree_key_cache {
    pub table: c::rhashtable,
    pub table_init_done: bool,

    pub shrink: *mut c::shrinker,
    pub shrink_iter: c_uint,

    /* 0: non pcpu reader locks, 1: pcpu reader locks */
    pub pending: [c::rcu_pending; 2],
    #[c("size_t __percpu *nr_pending")]
    pub nr_pending: *mut usize,

    pub nr_keys: AtomicIsize,
    pub nr_dirty: AtomicIsize,

    /* shrinker stats */
    pub requested_to_free: c_ulong,
    pub freed: c_ulong,
    pub skipped_dirty: c_ulong,
    pub skipped_accessed: c_ulong,
    pub skipped_lock_fail: c_ulong,
}
c_default!(bch_fs_btree_key_cache);

impl bch_fs_btree_key_cache {
    pub fn nr_keys_need_flush(&self) -> usize {
        let nr_dirty = self.nr_dirty.load(Relaxed);
        let nr_keys = self.nr_keys.load(Relaxed);
        let max_dirty = 1024 + nr_keys / 2;

        nr_dirty.saturating_sub(max_dirty).max(0) as usize
    }

    pub fn dirty_over_max(&self) -> isize {
        let nr_dirty = self.nr_dirty.load(Relaxed);
        let nr_keys = self.nr_keys.load(Relaxed);
        let max_dirty = 4096 + (nr_keys * 3) / 4;

        nr_dirty - max_dirty
    }

    pub fn must_wait(&self) -> bool {
        self.dirty_over_max() > 0
    }

    pub fn wait_done(&self) -> bool {
        let nr_dirty = self.nr_dirty.load(Relaxed);
        let nr_keys = self.nr_keys.load(Relaxed);
        let max_dirty = 2048 + (nr_keys * 5) / 8;

        nr_dirty <= max_dirty
    }
}

#[c_export]
pub fn bch2_nr_btree_keys_need_flush(c: &Fs) -> usize {
    c.btree.key_cache.nr_keys_need_flush()
}

#[c_export]
pub fn __bch2_btree_key_cache_must_wait(c: &Fs) -> isize {
    c.btree.key_cache.dirty_over_max()
}

#[c_export]
pub fn bch2_btree_key_cache_must_wait(c: &Fs) -> bool {
    c.btree.key_cache.must_wait()
}

#[c_export]
pub fn bch2_btree_key_cache_wait_done(c: &Fs) -> bool {
    c.btree.key_cache.wait_done()
}

#[repr(C, align(4))]
#[derive(Clone, Copy, Default, CStruct)]
#[c_packed]
pub struct bkey_cached_key {
    pub btree_id: u32,
    pub pos: Bpos,
}

c_extern! {
    pub fn bch2_btree_key_cache_to_text(out: *mut Printbuf, bc: *mut c::bch_fs_btree_key_cache);
    pub fn bch2_btree_key_cache_flush_going_ro(c: *mut Fs) -> c_int;
}

impl ToText for c::bch_fs_btree_key_cache {
    fn to_text(&self, out: &mut Printbuf) {
        unsafe { bch2_btree_key_cache_to_text(out.as_raw(), core::ptr::from_ref(self).cast_mut()) }
    }
}

impl c::bch_fs_btree_key_cache {
    /// Ask the shrinker to free @nr keys, as memory pressure would.
    pub fn shrink(&self, nr: c_ulong) {
        unsafe { crate::util::kernel::shrinker_scan(self.shrink, nr) }
    }
}
