// SPDX-License-Identifier: GPL-2.0

use crate::btree::bkey::{POS_MIN, SPOS_MAX};
use crate::btree::iter::{BtreeIterFlags, BtreeNodeIter, BtreeTrans};
use crate::btree::node::BtreeNode;
use crate::c;
use crate::errcode::BchError;
use crate::format::Btree;
use crate::fs::Fs;
use crate::util::kernel::totalram_bytes;
use core::ops::{ControlFlow, RangeInclusive};

/// What's pinned stays in memory, shrinker-exempt, until this drops. There's one
/// set of pins, the filesystem's: pin() replaces what was.
#[must_use = "everything is unpinned as soon as this drops"]
pub struct BtreeCachePins<'f>(&'f Fs);

impl Fs {
    pub fn btree_cache_pins(&self) -> BtreeCachePins<'_> {
        BtreeCachePins(self)
    }
}

impl BtreeCachePins<'_> {
    /*
     * The memory the fsck passes may spend on pinned btree nodes. They prefetch
     * and pin a keyspace range so that the random-order lookups they're about to
     * make against it become cache hits, and stop once they've spent this much -
     * pinning is always best effort, the passes are correct either way.
     */
    pub fn budget(&self) -> u64 {
        totalram_bytes() as u64 * u64::from(self.0.opts.fsck_memory_usage_percent.get()) / 100
    }

    /*
     * Prefetch and pin [start, end]: the btrees of @interior_mask down to their
     * interior nodes, and of @leaf_mask down to their leaves. Replaces any
     * previously pinned window (there is only one), walks it with prefetch so
     * the nodes are faulted in with async parallel reads rather than synchronous
     * misses, and marks them shrinker-exempt - and nodes read into the cache
     * inside the window later are pinned as they come in.
     *
     * Best-effort: pinning stops at the budget, and the window ends there; the
     * return is where - @end if it all fit. Lookups beyond it still work,
     * they're just slow paths.
     */
    pub fn pin(&self, trans: &BtreeTrans<'_>, leaf_mask: u64, interior_mask: u64,
               range: RangeInclusive<c::bbpos>) -> Result<c::bbpos, BchError> {
        let fs = self.0;
        let (start, end) = range.into_inner();
        let mut mem_may_pin = self.budget() as i64;

        self.unpin();

        let interior_mask = interior_mask | leaf_mask;
        let mut pinned_end = end;
        {
            let mut lists = fs.btree.cache.lock.lock();
            lists.pinned_nodes_mask  = [leaf_mask, interior_mask];
            lists.pinned_nodes_start = start;
            lists.pinned_nodes_end   = end;
        }

        for btree in start.btree.0..=end.btree.0 {
            if interior_mask & (1 << btree) == 0 {
                continue;
            }

            let depth = if leaf_mask & (1 << btree) != 0 { 0 } else { 1 };
            let btree_start = if btree == start.btree.0 { start.pos } else { POS_MIN };
            let btree_end   = if btree == end.btree.0 { end.pos } else { SPOS_MAX };

            let mut iter = BtreeNodeIter::new(trans, btree, btree_start, 0, depth, BtreeIterFlags::PREFETCH);
            iter.for_each(trans, |b| {
                if b.contents().data().min_key > btree_end {
                    return Ok(ControlFlow::Break(()));
                }
                mem_may_pin -= b.buf_bytes() as i64;
                if mem_may_pin <= 0 {
                    pinned_end = c::bbpos { btree: Btree(btree), pos: b.contents().key.k.p };
                    fs.btree.cache.lock.lock().pinned_nodes_end = pinned_end;
                    return Ok(ControlFlow::Break(()));
                }
                fs.btree_node_pin(&b);
                Ok(ControlFlow::Continue(()))
            })?;

            if mem_may_pin <= 0 {
                break;
            }
        }

        Ok(pinned_end)
    }

    /// Outside any window.
    pub fn node(&self, b: &BtreeNode) {
        self.0.btree_node_pin(b)
    }

    pub fn unpin(&self) {
        let mut lists = self.0.btree.cache.lock.lock();
        lists.pinned_nodes_mask = [0; 2];

        let [unpinned, pinned] = &mut lists.live;
        for (from, to) in [(&mut pinned.clean, &mut unpinned.clean), (&mut pinned.dirty, &mut unpinned.dirty)] {
            from.iter().for_each(|b| b.flags.set_pinned(false));
            unsafe { to.append(from) };
        }

        let nr = &lists.published().live_nr;
        nr[0].nr_clean.set(nr[0].nr_clean.get() + nr[1].nr_clean.get());
        nr[0].nr_dirty.set(nr[0].nr_dirty.get() + nr[1].nr_dirty.get());
        nr[1].nr_clean.set(0);
        nr[1].nr_dirty.set(0);
    }
}

impl Fs {
    pub(crate) fn btree_node_pin(&self, b: &BtreeNode) {
        let mut lists = self.btree.cache.lock.lock();
        let is_root = self.btree_id_root(b.c.btree_id.into()).is_some_and(|r| core::ptr::eq(r, b));

        if is_root || b.flags.pinned() {
            return;
        }
        b.flags.set_pinned(true);

        let dirty = match b.cache_state(&lists) {
            c::btree_node_cache_state::CLEAN => false,
            c::btree_node_cache_state::DIRTY => true,
            _ => return,
        };

        let [unpinned, pinned] = &mut lists.live;
        let (from, to) = if dirty {
            (&mut unpinned.dirty, &mut pinned.dirty)
        } else {
            (&mut unpinned.clean, &mut pinned.clean)
        };
        unsafe {
            from.remove(b);
            to.push_back(b);
        }

        let [n0, n1] = &lists.published().live_nr;
        let (n0, n1) = if dirty { (&n0.nr_dirty, &n1.nr_dirty) } else { (&n0.nr_clean, &n1.nr_clean) };
        n0.set(n0.get() - 1);
        n1.set(n1.get() + 1);
    }
}

impl Drop for BtreeCachePins<'_> {
    fn drop(&mut self) {
        self.unpin()
    }
}
