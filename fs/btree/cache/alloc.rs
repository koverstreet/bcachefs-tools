// SPDX-License-Identifier: GPL-2.0

use super::reclaim::ReclaimFlags;
use super::{cache_node_struct_alloc, cached_node, new_node_lock, NodeBufs};
use crate::btree::bkey::{bkey_val_to_text, BkeyI, BkeySC};
use crate::btree::iter::BtreeTrans;
use crate::btree::node::BtreeNode;
use crate::btree::path::{bch2_btree_path_to_text, PathIdx, Paths};
use crate::btree::read::{bch2_btree_node_wait_on_read, bch2_btree_node_wait_on_write, node_read};
use crate::btree::types::{BtreeCacheGuard, BtreeNodeList, BtreeWriteType};
use crate::btree::write::{node_write, BtreeWriteFlags};
use crate::c;
use crate::data_structures::list::Walk;
use crate::debug::trace::events;
use crate::errcode::{bch_errcode, BchError};
use crate::format::Btree;
use crate::fs::Fs;
use crate::locking::six::SixHold;
use crate::prt;
use crate::util::kernel::{cond_resched, local_clock, unlikely, PAGE_SIZE};
use crate::util::log::LogMsg;
use crate::util::sched::current;
use crate::util::time_stats::TimeStats;
use c::btree_node_cache_state as State;
use c::six_lock_type::{self, SIX_LOCK_read};
use core::ptr::NonNull;
use core::sync::atomic::Ordering::Relaxed;

/* Btree node alloc / lookup / evict */

impl Fs {
    fn btree_node_grab(&self, list: impl for<'l, 'g> Fn(&'l mut BtreeCacheGuard<'g>) -> &'l mut BtreeNodeList,
                       pcpu_read_locks: bool, flags: ReclaimFlags) -> Option<(&BtreeNode, SixHold<'_>)> {
        let mut lists = self.btree.cache.lock.lock();
        let mut walk = unsafe { Walk::new(&mut lists, list) };
        while let Some((lists, b)) = walk.next() {
            let b = unsafe { cached_node(self, b) };
            if pcpu_read_locks != b.c.lock.six().readers.allocated() {
                continue;
            }
            if let Ok(held) = self.btree_node_reclaim(lists, b, flags) {
                self.btree.evicted_size.record(b.hash_val(), unsafe { b.contents() }.nr.live_u64s);
                let _ = self.btree_node_transition_state_locked(lists, b, State::NONE);
                return Some((b, held));
            }
        }

        None
    }

    fn system_memory_usage_high(&self) -> bool {
        let avail = unsafe { c::si_mem_available() } as usize;
        let total = unsafe { c::totalram_pages() } as usize;
        if avail >= total >> 2 {
            return false;
        }

        let bc = &self.btree.cache;
        let pinned_pages = (bc.lock.nr_live() + bc.lock.nr_freeable.get()) * self.opts.btree_node_size.get() as usize
            / PAGE_SIZE;
        pinned_pages > (total - avail) >> 2
    }
}

impl BtreeTrans<'_> {
    fn btree_node_cannibalize(&self, paths: &mut Paths<'_>, pcpu_read_locks: bool) -> (&BtreeNode, SixHold<'_>) {
        let fs = self.fs();

        loop {
            for i in 0..2 {
                if let Some(got) = fs.btree_node_grab(|l| &mut l.live[i].clean, pcpu_read_locks, ReclaimFlags::empty()) {
                    return got;
                }
            }

            for i in 0..2 {
                if let Some((b, held)) = fs.btree_node_grab(|l| &mut l.live[i].dirty, pcpu_read_locks,
                                                            ReclaimFlags::ALLOW_DIRTY) {
                    if b.flags.dirty() {
                        node_write(self, b, BtreeWriteType::cache_reclaim, BtreeWriteFlags::empty());
                    }

                    bch2_btree_node_wait_on_read(self, paths, b);
                    bch2_btree_node_wait_on_write(self, paths, b);
                    return (b, held);
                }
            }

            /*
             * Rare case: all matching-type nodes were intent-locked.
             * Just busy-wait.
             */
            crate::warn_once!(true, fs, "btree cache cannibalize failed");
            cond_resched();
        }
    }

    pub(crate) fn btree_node_mem_alloc(&self, paths: &mut Paths<'_>, pcpu_read_locks: bool)
        -> Result<(NonNull<BtreeNode>, SixHold<'_>), BchError>
    {
        let fs = self.fs();
        let bc = &fs.btree.cache;
        let start_time = local_clock();

        let (b, held) = 'got_mem: {
            if let Some(got) = fs.btree_node_grab(|l| &mut l.freeable, pcpu_read_locks, ReclaimFlags::empty()) {
                break 'got_mem got;
            }

            /*
             * If MM is tight AND we're a meaningful fraction of in-use memory,
             * reclaim from our own cache instead of asking MM. MM's shrinker
             * callbacks haven't been aggressive enough to keep bcachefs out of
             * OOM under heavy load (TiCPU report).
             */
            if unlikely(fs.system_memory_usage_high()) {
                bc.nr_self_reclaim.fetch_add(1, Relaxed);
                let live = pcpu_read_locks as usize;
                if let Some(got) = fs.btree_node_grab(|l| &mut l.live[live].clean, pcpu_read_locks,
                                                      ReclaimFlags::empty()) {
                    break 'got_mem got;
                }
            }

            'err: {
                let mut bufs = NodeBufs::new(fs);
                if bufs.alloc(fs, c::GFP_NOWAIT, true).is_err() {
                    self.unlock_from(paths, crate::ret_ip!());
                    if bufs.alloc(fs, c::GFP_KERNEL | c::__GFP_NOWARN, true).is_err() {
                        bufs.free();
                        break 'err;
                    }
                }

                let freed = fs.btree_node_grab(|l| if pcpu_read_locks { &mut l.freed_pcpu } else { &mut l.freed_nonpcpu },
                                               pcpu_read_locks, ReclaimFlags::empty());
                let (b, held) = match freed {
                    Some(got) => got,
                    None => {
                        let b = match cache_node_struct_alloc(fs, pcpu_read_locks, c::GFP_NOWAIT) {
                            Some(b) => b,
                            None => {
                                self.unlock_from(paths, crate::ret_ip!());
                                match cache_node_struct_alloc(fs, pcpu_read_locks, c::GFP_KERNEL) {
                                    Some(b) => b,
                                    None => {
                                        bufs.free();
                                        break 'err;
                                    }
                                }
                            }
                        };
                        let b = unsafe { cached_node(fs, b.as_ptr()) };
                        (b, new_node_lock(b))
                    }
                };

                unsafe {
                    b.contents_mut().data = bufs.data.cast();
                    (*core::ptr::from_ref(b).cast_mut()).aux_data = bufs.aux_data;
                }
                break 'got_mem (b, held);
            }

            /* Try to cannibalize another cached btree node: */
            if bc.alloc_lock.load(Relaxed) == current() {
                let got = self.btree_node_cannibalize(paths, pcpu_read_locks);
                fs.event_inc_trace(events::btree_cache_cannibalize, |out| prt!(out, "{}", self.fn_name()));
                break 'got_mem got;
            }

            return Err(bch_errcode::BCH_ERR_ENOMEM_btree_node_mem_alloc.into());
        };

        assert!(!BtreeNodeList::linked(b), "a node allocated off a list");
        assert!(b.hash_val() == 0, "a hashed node allocated");
        assert!(!b.flags.dirty(), "a dirty node allocated");
        assert!(!b.flags.write_in_flight(), "a node allocated with a write in flight");
        assert!(!unsafe { b.contents() }.data.is_null(), "a node allocated with no buffer");

        /* @b may have been a node our paths still have cached: */
        self.node_forget(paths, b);

        b.flags.store(Default::default());
        b.written.set(0);
        let contents = unsafe { b.contents_mut() };
        contents.nsets = 0;
        contents.sib_u64s[0].set(0);
        contents.sib_u64s[1].set(0);
        b.whiteout_u64s.set(0);
        b.keys_mut(contents).btree_keys_init();

        fs.times[c::bch_time_stats::btree_node_mem_alloc as usize].update(start_time);

        if let Err(e) = self.relock_from(paths) {
            let _ = fs.btree_node_transition_state(b, State::FREEABLE);
            return Err(e);
        }

        Ok((NonNull::from(b), held))
    }

    /* Slowpath, don't want it inlined into btree_iter_traverse() */
    #[inline(never)]
    pub(super) fn btree_node_fill(&self, paths: &mut Paths<'_>, path: Option<PathIdx>, k: &BkeyI, btree: Btree,
                                  level: usize, lock_type: six_lock_type, sync: bool)
        -> Result<Option<(&BtreeNode, SixHold<'_>)>, BchError>
    {
        let fs = self.fs();

        ebug_assert!(path.is_none_or(|p| level + 1 == paths[p].level() as usize));

        if unlikely(level >= c::BTREE_MAX_DEPTH as usize) {
            let mut msg = LogMsg::new(fs);
            prt!(msg, "attempting to get btree node at level {level}, >= max depth {}", c::BTREE_MAX_DEPTH);
            return Err(fs.topology_error(&mut msg));
        }

        if unlikely(!k.k.is_btree_ptr()) {
            let mut msg = LogMsg::new(fs);
            prt!(msg, "attempting to get btree node with non-btree key ");
            bkey_val_to_text(&mut msg, Some(fs), BkeySC::from(k));
            return Err(fs.topology_error(&mut msg));
        }

        if unlikely(usize::from(k.k.u64s) > c::BKEY_BTREE_PTR_U64s_MAX) {
            let mut msg = LogMsg::new(fs);
            prt!(msg, "attempting to get btree node with too big key ");
            bkey_val_to_text(&mut msg, Some(fs), BkeySC::from(k));
            return Err(fs.topology_error(&mut msg));
        }

        /*
         * Parent node must be locked, else we could read in a btree node that's
         * been freed:
         */
        if let Some(p) = path {
            self.path_relock(paths, p)?;
        }

        let (b, held) = match self.btree_node_mem_alloc(paths, level != 0) {
            Err(e) if e.matches(c::ENOMEM) => {
                let Some(p) = path else { return Err(e) };

                self.set_memory_allocation_failure(true);

                fs.event_inc_trace(events::trans_restart_memory_allocation_failure, |out| {
                    prt!(out, "{}\n", self.fn_name());
                    bch2_btree_path_to_text(out, self, p, &paths[p]);
                });
                return Err(self.restart(bch_errcode::BCH_ERR_transaction_restart_fill_mem_alloc_fail));
            }
            r => r?,
        };

        unsafe {
            let p = b.as_ptr();
            (*p).contents_mut().set_key(k);
            (*p).c.level    = level as u8;
            (*p).c.btree_id = btree.0 as u8;
        }
        let b = unsafe { cached_node(fs, b.as_ptr()) };

        let ret = if fs.btree_node_transition_state(b, State::CLEAN).is_ok() {
            /*
             * Must be set with node write locked - b->data is not stable
             * while node is write locked or read_in_flight is set, read
             * path will do a buffer swap
             *
             * and also cache coherency
             */
            b.flags.set_read_in_flight(true);
            let intent = held.downgrade();

            if path.is_some() {
                let seq = b.c.lock.six().seq.get();

                /* Unlock before doing IO: */
                drop(intent);
                self.unlock_from(paths, crate::ret_ip!());

                node_read(self, b, sync);

                if sync { b.c.lock.relock_as(lock_type, seq) } else { None }
            } else {
                node_read(self, b, sync);
                Some(if lock_type == SIX_LOCK_read { intent.downgrade() } else { intent })
            }
        } else {
            /* raced with another fill: */
            drop(held);
            None
        };

        /*
         * bch2_btree_node_mem_alloc may have unlocked the trans for GFP_KERNEL
         * allocation, and the if (path) { } block above unlocks for IO - ensure
         * we're relocked before returning:
         */
        if let Some(p) = path {
            self.relock_from(paths).and_then(|()| self.path_relock(paths, p))?;
        }

        Ok(ret.map(|held| (b, held)))
    }
}
