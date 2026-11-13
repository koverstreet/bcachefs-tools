// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use crate::btree::key_cache::shrink_table;
use crate::cstructs::c;
use crate::util::shrinker::Shrinker;
use crate::util::workqueue::ReffedWork;
use core::sync::atomic::{AtomicIsize, AtomicUsize};
use core::sync::atomic::Ordering::Relaxed;
use cstruct_macros::{placed, MaybeZeroable, Init, TypeInfo};
use pin_init::pin_data;
use crate::format::Bpos;

/// The key cache's shrinker, Sync - a shrinker's owner is - as the key cache,
/// with its per-CPU counter and fast_list, isn't.
#[placed(c::bch_fs_btree_key_cache.shrinker)]
#[pin_data]
#[repr(C)]
#[derive(MaybeZeroable, Init)]
pub struct btree_key_cache_shrinker {
    pub(crate) shrink: Shrinker<Self>,
    /* the table shrunk once scan() has emptied it: its allocation, off reclaim */
    #[pin]
    pub(crate) shrink_table_work: ReffedWork<shrink_table, crate::field!(super.super.super.ro_ref)>,
}

#[placed(c::bch_fs_btree.key_cache)]
#[pin_data]
#[repr(C)]
#[derive(MaybeZeroable, TypeInfo, Init)]
pub struct bch_fs_btree_key_cache {
    #[pin]
    #[init(::pin_init::init_zeroed())]
    pub(crate) table: crate::btree::key_cache::KeyTable,
    /* what's in the table, to walk: an entry's slot is from insert to evict */
    #[pin]
    #[init(::pin_init::init_zeroed())]
    pub(crate) keys: c::fast_list,

    #[pin]
    pub(crate) shrinker: c::btree_key_cache_shrinker,
    #[pin]
    #[init(AtomicUsize::new(0))]
    pub(crate) shrink_iter: AtomicUsize,

    /* 0: non pcpu reader locks, 1: pcpu reader locks */
    #[pin]
    #[init(::pin_init::init_zeroed())]
    /* set up and exited through the struct shared, in place: setup(), exit() */
    pub(crate) pending: core::cell::UnsafeCell<[crate::btree::key_cache::Pending; 2]>,

    pub nr_keys: AtomicIsize,
    pub nr_dirty: AtomicIsize,

    /* shrinker stats */
    #[pin]
    #[init(crate::btree::key_cache::ShrinkStats::new())]
    pub(crate) stats: crate::btree::key_cache::ShrinkStats,
}

impl bch_fs_btree_key_cache {
    pub(crate) fn pending(&self) -> &[crate::btree::key_cache::Pending; 2] {
        unsafe { &*self.pending.get() }
    }

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

#[repr(C, align(4))]
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash, MaybeZeroable)]
pub struct bkey_cached_key {
    pub btree_id: u32,
    pub pos: Bpos,
}
