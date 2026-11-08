// SPDX-License-Identifier: GPL-2.0

use super::{bch2_btree_pos_to_text, cached_node};
use crate::btree::node::BtreeNode;
use crate::btree::types::{bch_btree_cache_not_freed_reasons as NotFreed, BtreeCacheGuard, BtreeWriteType};
use crate::btree::write::{node_write, BtreeWriteFlags};
use crate::c;
use crate::data_structures::list::Walk;
use crate::debug::params::btree_shrinker_disabled;
use crate::debug::trace::events;
use crate::errcode::{bch_errcode, BchError};
use crate::fs::Fs;
use crate::locking::six::SixHold;
use crate::prt;
use crate::util::kernel::local_clock;
use crate::util::time_stats::TimeStats;
use bitflags::bitflags;
use c::btree_node_cache_state as State;
use c::six_lock_type::{SIX_LOCK_intent, SIX_LOCK_read};

/*
 * Dirty nodes are journal reclaim's to write - but during journal replay it
 * can't flush the unreplayed entries' pins (journal_get_next_pin()), and replay
 * can dirty more nodes than fit in memory: accounting replay on a filesystem
 * with a big accounting btree dirtied 45k nodes, 11G, and went OOM with nothing
 * ever writing one. So during replay the shrinker counts dirty nodes too, and
 * what it can't free it writes (btree_cache_replay_write()).
 */
fn replay_writeback(fs: &Fs) -> bool {
    !fs.journal.flags.get(c::journal_flags::replay_done) &&
        fs.flags.get(c::bch_fs_flags::rw) &&
        !fs.flags.get(c::bch_fs_flags::going_ro)
}

impl c::bch_fs_btree_cache {
    fn can_free(&self, idx: usize) -> usize {
        let can_free = self.lock.live_nr[idx].nr_clean.get();
        if idx == 0 { can_free.saturating_sub(self.lock.nr_reserve.get()) } else { can_free }
    }
}

bitflags! {
    #[derive(Clone, Copy)]
    pub(super) struct ReclaimFlags: u32 {
        const SHRINKER    = 1 << 0;
        const ALLOW_DIRTY = 1 << 1;
    }
}

impl Fs {
    fn noreclaim(&self, lists: &mut BtreeCacheGuard<'_>, flags: ReclaimFlags, reason: NotFreed) -> BchError {
        lists.not_freed[reason as usize] += flags.contains(ReclaimFlags::SHRINKER) as u64;
        self.err(bch_errcode::BCH_ERR_ENOMEM_btree_node_reclaim)
    }

    fn reclaim_checks(&self, lists: &mut BtreeCacheGuard<'_>, b: &BtreeNode, flags: ReclaimFlags)
        -> Result<(), BchError>
    {
        let f = b.flags.load();

        let reason = if f.permanent() {
            NotFreed::permanent
        } else if f.noevict() {
            NotFreed::noevict
        } else if f.write_blocked() {
            NotFreed::write_blocked
        } else if f.will_make_reachable() {
            NotFreed::will_make_reachable
        } else if flags.contains(ReclaimFlags::ALLOW_DIRTY) {
            return Ok(());
        } else if f.dirty() {
            NotFreed::dirty
        } else if f.read_in_flight() {
            NotFreed::read_in_flight
        } else if f.write_in_flight() {
            NotFreed::write_in_flight
        } else {
            return Ok(());
        };
        Err(self.noreclaim(lists, flags, reason))
    }

    /*
     * this version is for btree nodes that have already been freed (we're not
     * reaping a real btree node)
     */
    pub(super) fn btree_node_reclaim<'b>(&self, lists: &mut BtreeCacheGuard<'_>, b: &'b BtreeNode,
                                         flags: ReclaimFlags) -> Result<SixHold<'b>, BchError> {
        self.reclaim_checks(lists, b, flags)?;

        let Some(intent) = b.c.lock.try_lock_as(SIX_LOCK_intent) else {
            return Err(self.noreclaim(lists, flags, NotFreed::lock_intent));
        };

        let Ok(held) = intent.try_write() else {
            return Err(self.noreclaim(lists, flags, NotFreed::lock_write));
        };

        /* recheck under lock */
        self.reclaim_checks(lists, b, flags)?;

        if b.hash_val() != 0 {
            self.event_inc_trace(events::btree_cache_reap, |out| bch2_btree_pos_to_text(out, self, b));
        }
        Ok(held)
    }

    fn btree_cache_scan_locked(&self, idx: usize, nr_to_scan: usize) -> Option<(usize, usize)> {
        let bc = &self.btree.cache;
        let mut nr = nr_to_scan;
        let mut freed = 0;
        let mut touched = 0;
        let mut writeback = 0;

        if btree_shrinker_disabled() {
            return None;
        }

        let start_time = local_clock();
        let mut lists = bc.lock.lock();

        /*
         * It's _really_ critical that we don't free too many btree nodes - we
         * have to always leave ourselves a reserve. The reserve is how we
         * guarantee that allocating memory for a new btree node can always
         * succeed, so that inserting keys into the btree can always succeed and
         * IO can always make forward progress:
         */
        let can_free = bc.can_free(idx);
        if nr > can_free {
            /* during journal replay, dirty nodes get written instead: */
            if replay_writeback(self) {
                writeback = (nr - can_free).min(bc.lock.live_nr[idx].nr_dirty.get());
            }

            lists.not_freed[NotFreed::cache_reserve as usize] += (nr - can_free - writeback) as u64;
            nr = can_free;
        }

        'out: {
            let mut walk = unsafe { Walk::new(&mut lists, |l| &mut l.freeable) };
            let mut i = 0;
            while let Some((lists, b)) = walk.next() {
                /*
                 * Leave a few nodes on the freeable list, so that a btree split
                 * won't have to hit the system allocator:
                 */
                i += 1;
                if i <= 3 {
                    continue;
                }

                touched += 1;

                if touched >= nr {
                    break 'out;
                }

                if let Ok(held) = self.btree_node_reclaim(lists, b, ReclaimFlags::SHRINKER) {
                    let _ = self.btree_node_transition_state_locked(lists, b, State::FREED);
                    drop(held);
                    freed += 1;
                }
            }

            let mut walk = unsafe { Walk::new(&mut lists, |l| &mut l.live[idx].clean) };
            while let Some((lists, b)) = walk.next() {
                if b.flags.permanent() {
                    continue;
                }

                touched += 1;

                if b.flags.accessed() {
                    b.flags.set_accessed(false);
                    lists.not_freed[NotFreed::access_bit as usize] += 1;
                    touched -= 1;
                } else if let Ok(held) = self.btree_node_reclaim(lists, b, ReclaimFlags::SHRINKER) {
                    self.btree.evicted_size.record(b.hash_val(), unsafe { b.contents() }.nr.live_u64s);
                    let _ = self.btree_node_transition_state_locked(lists, b, State::FREED);

                    freed += 1;

                    drop(held);
                }

                if touched >= nr {
                    walk.rotate();
                    break;
                }
            }
        }

        lists.nr_freed     += freed;
        lists.nr_requested += nr;

        self.times[c::bch_time_stats::btree_node_cache_scan as usize].update(start_time);

        self.event_inc_trace(events::btree_cache_scan, |out| {
            prt!(out, "scanned {nr_to_scan} nodes, can free {can_free}, freed {freed}, writing {writeback}")
        });
        Some((freed, writeback))
    }

    pub(super) fn btree_cache_count(&self, idx: usize) -> usize {
        let bc = &self.btree.cache;

        if btree_shrinker_disabled() {
            return 0;
        }

        bc.can_free(idx) +
            if replay_writeback(self) { bc.lock.live_nr[idx].nr_dirty.get() } else { 0 }
    }

    /*
     * A dirty node we can write for the shrinker, read locked. Round robin: every
     * node looked at goes to the tail, so a pass doesn't keep finding the nodes
     * already written - they stay on the dirty list until their write completes.
     */
    fn btree_cache_replay_write_pick(&self) -> Option<(&BtreeNode, SixHold<'_>)> {
        let bc = &self.btree.cache;
        let mut lists = bc.lock.lock();

        for i in 0..lists.live.len() {
            let dirty = &mut lists.live[i].dirty;

            for _ in 0..bc.lock.live_nr[i].nr_dirty.get() {
                let Some(b) = dirty.first().map(|b| unsafe { cached_node(self, b) }) else { break };

                unsafe {
                    dirty.remove(b);
                    dirty.push_back(b);
                }

                let f = b.flags.load();
                if f.dirty() && !f.will_make_reachable() && !f.write_blocked() {
                    if let Some(held) = b.c.lock.try_lock_as(SIX_LOCK_read) {
                        return Some((b, held));
                    }
                }
            }
        }

        None
    }

    /*
     * Kick writes on @nr dirty nodes the shrinker couldn't free during journal
     * replay: see btree_cache_replay_writeback(). They come back clean when the
     * writes complete, and a later scan frees them.
     *
     * Writes are queued on a btree_trans, and a task can't have two locked ones -
     * fine here, as we never enter memory reclaim with btree locks held (lockdep
     * checks that).
     */
    fn btree_cache_replay_write(&self, nr: usize) {
        if nr == 0 {
            return;
        }

        let trans = crate::btree_trans!(self);

        for _ in 0..nr {
            let Some((b, held)) = self.btree_cache_replay_write_pick() else { break };

            node_write(&trans, b, BtreeWriteType::cache_reclaim, BtreeWriteFlags::empty());
            drop(held);
        }
        /* putting the trans submits the writes queued on it */
    }

    pub(super) fn btree_cache_scan(&self, idx: usize, nr_to_scan: usize) -> Option<usize> {
        let (freed, writeback) = self.btree_cache_scan_locked(idx, nr_to_scan)?;

        /* with bc->lock dropped: */
        self.btree_cache_replay_write(writeback);
        Some(freed)
    }
}
