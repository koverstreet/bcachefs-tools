// SPDX-License-Identifier: GPL-2.0

use super::{btree_ptr_hash_val, cached_node, node_data_free, node_key_hash_val};
use crate::btree::bkey::{BkeyI, BkeySC};
use crate::btree::node::BtreeNode;
use crate::btree::types::{BtreeCacheGuard, BtreeCacheLists, BtreeNodeList};
use crate::c;
use crate::data_structures::cuckoo::{Cuckoo, Entry};
use crate::errcode::{Errcode, BchError};
use crate::format::Btree;
use crate::fs::Fs;
use crate::locking::six::{six_lock_counts, six_lock_wakeup_all};
use crate::locking::Guard as _;
use crate::prt;
use crate::util::closure::closure_wake_up;
use crate::util::hash::GoldenRatio;
use crate::os::kernel::is_vmalloc_addr;
use crate::util::log::LogMsg;
use crate::util::rcu::read_lock;
use c::BtreeNodeCacheState as State;
use core::ptr::NonNull;
use core::sync::atomic::{fence, Ordering::SeqCst};

pub(crate) type NodeTable = Cuckoo<Entry<BtreeNode>, GoldenRatio>;

// set_hash_val() writes it only while the node isn't in the table
crate::keyed_by!(BtreeNode, |b| unsafe { &*b.hash_val.get() } => u64);

// what's shared is the node lock's, bc->lock's, or atomic
unsafe impl Sync for BtreeNode {}

impl BtreeNode {
    pub fn hash_val(&self) -> u64 {
        unsafe { self.hash_val.get().read_volatile() }
    }

    fn set_hash_val(&self, _lists: &mut BtreeCacheGuard<'_>, v: u64) {
        unsafe { self.hash_val.get().write_volatile(v) }
    }

    pub fn cache_state(&self, lists: &BtreeCacheGuard<'_>) -> State {
        *lists.get_of(&self.cache_state)
    }

    fn set_cache_state(&self, lists: &mut BtreeCacheGuard<'_>, s: State) {
        *lists.get_mut_of(&self.cache_state) = s
    }
}

/* Pinning */

fn node_pinned(lists: &BtreeCacheLists, b: &BtreeNode) -> bool {
    let pos = c::bbpos { btree: Btree(b.c.btree_id.into()), pos: unsafe { b.contents() }.key_i().k.p };

    let mask = lists.pinned_nodes_mask[(b.c.level != 0) as usize];

    mask & (1 << b.c.btree_id) != 0 &&
        lists.pinned_nodes_start < pos &&
        lists.pinned_nodes_end >= pos
}

/* Cache state transitions — see DOC(btree-node-cache). */

pub(super) fn state_hashed(state: State) -> bool {
    matches!(state, State::CLEAN | State::DIRTY)
}

fn state_has_buffer(state: State) -> bool {
    state_hashed(state) || state == State::FREEABLE
}

impl Fs {
    pub(crate) fn btree_node_transition_state_locked(&self, lists: &mut BtreeCacheGuard<'_>, b: &BtreeNode,
                                                     mut new: State) -> Result<(), BchError> {
        let bc = &self.btree.cache;
        let old = b.cache_state(lists);
        let mut pinned = b.flags.pinned() as usize;
        let mut ret = Ok(());
        let data = unsafe { (*b.contents.data_ptr()).data };

        /*
         * Write lock required for transitions that touch the data buffer or
         * hash table; CLEAN↔DIRTY swap only moves the node between live[]
         * lists and is safe under bc->lock alone.
         */
        ebug_assert!((state_hashed(old) && state_hashed(new)) ||
                     six_lock_counts(b.c.lock.six()).n[c::SixLockType::SIX_LOCK_write as usize] != 0,
                     "cache state {old:?} -> {new:?} without the node write locked");
        assert!(!(state_has_buffer(old) || state_has_buffer(new)) || !data.is_null(),
                "cache state {old:?} -> {new:?} of a node with no buffer");
        /*
         * BTREE_NODE_dirty is meaningful only while a node is hashed (CLEAN /
         * DIRTY); once unhashed (FREEABLE / FREED) it must already be clear.
         * The dirty bit is what makes the cache_state DIRTY, so a FREEABLE
         * node with the bit set is an inconsistent state — and consumers that
         * need to scrub dirty nodes can rely on walking only live[].dirty.
         * Warn with the node dumped — the flags identify which path leaked
         * the dirty bit (write_blocked/never_write/noevict/need_write) — then
         * clear it and carry on: the state bookkeeping below handles the
         * DIRTY exit either way.
         */
        if crate::warn_once!(!state_hashed(new) && b.flags.dirty(), self,
                             "dirty node in unhashed state transition") {
            let mut msg = LogMsg::new(self);
            prt!(msg, "dirty node in unhashed state transition {} -> {}\n", old as u32, new as u32);
            prt!(msg, "btree={} level={} cache_state={} flags=0x{:x} hash_val=0x{:x}\n",
                 b.c.btree_id, b.c.level, old as u32, b.flags.load().into_bits(), b.hash_val());
            b.flags.set_dirty(false);
        }

        if old == new {
            return Ok(());
        }

        let hashed_delta = state_hashed(new) as i32 - state_hashed(old) as i32;
        if hashed_delta > 0 {
            let p = node_pinned(lists, b);
            b.flags.set_pinned(p);
            pinned = p as usize;

            b.set_hash_val(lists, node_key_hash_val(&unsafe { b.contents() }.key));
            ret = match bc.table.insert_new(unsafe { Entry::new(NonNull::from(b)) }) {
                Ok(Ok(()))  => Ok(()),
                Ok(Err(_))  => Err(Errcode::BCH_ERR_EEXIST_btree_node_cache_insert.into()),
                Err(_)      => Err(Errcode::BCH_ERR_ENOMEM_btree_node_cache_insert.into()),
            };
            if ret.is_err() {
                b.set_hash_val(lists, 0);
                new = State::FREEABLE;
            } else {
                lists.nr_vmalloc += is_vmalloc_addr(data) as usize;
                if let Some(nr) = lists.nr_by_btree.get_mut(b.c.btree_id as usize) {
                    *nr += 1;
                }
            }
        }
        if hashed_delta < 0 {
            assert!(bc.table.remove_entry(&b.hash_val(), NonNull::from(b)),
                    "a hashed node, hash_val 0x{:x}, not in the table", b.hash_val());
            b.set_hash_val(lists, 0);
            b.flags.set_just_written(false);

            /*
             * The node identity is now gone. Any waiters parked on b->c.lock
             * were waiting on the *old* identity and have no business
             * waiting now — wake them all so they re-run the cycle detector
             * (and the upcoming identity check) and bail out of any wedge
             * caused by an invisible off-path intent holder (e.g. via
             * six_trylock_intent from btree_node_reclaim, which doesn't
             * publish itself on any btree_path the detector walks).
             *
             * Memory barrier pairs with the smp_mb() in
             * bch2_six_check_for_deadlock — without it, the lockless walker
             * can read a stale hash_val after wake and still think this is
             * the node it was looking for.
             */
            fence(SeqCst);
            six_lock_wakeup_all(b.c.lock.six());

            if let Some(nr) = lists.nr_by_btree.get_mut(b.c.btree_id as usize) {
                *nr -= 1;
            }
            lists.nr_vmalloc -= is_vmalloc_addr(data) as usize;
        }

        /* Undo current state's bookkeeping. */
        match old {
            State::CLEAN => {
                let nr = &lists.published().live_nr[pinned].nr_clean;
                nr.set(nr.get() - 1);
            }
            State::DIRTY => {
                let nr = &lists.published().live_nr[pinned].nr_dirty;
                nr.set(nr.get() - 1);
            }
            State::FREEABLE => {
                let nr = &lists.published().nr_freeable;
                nr.set(nr.get() - 1);
            }
            State::FREED | State::NONE => {}
        }

        unsafe { BtreeNodeList::unlink(b) };

        /* Attach to target state. */
        match new {
            State::NONE => {}
            State::CLEAN => {
                let nr = &lists.published().live_nr[pinned].nr_clean;
                nr.set(nr.get() + 1);
                unsafe { lists.live[pinned].clean.push_back(b) };
            }
            State::DIRTY => {
                let nr = &lists.published().live_nr[pinned].nr_dirty;
                nr.set(nr.get() + 1);
                unsafe { lists.live[pinned].dirty.push_back(b) };
            }
            State::FREEABLE => {
                assert!(!b.flags.permanent(), "a permanent node made freeable");
                assert!(!data.is_null(), "a node with no buffer made freeable");
                let nr = &lists.published().nr_freeable;
                nr.set(nr.get() + 1);
                unsafe { lists.freeable.push_front(b) };
            }
            State::FREED => {
                assert!(!b.flags.permanent(), "a permanent node freed");
                unsafe { node_data_free(NonNull::from(b)) };
                let freed = if b.c.lock.six().readers.allocated() {
                    &mut lists.freed_pcpu
                } else {
                    &mut lists.freed_nonpcpu
                };
                unsafe { freed.push_front(b) };
            }
        }

        bc.update_throttle();
        closure_wake_up(&bc.nr_in_flight_wait);

        b.set_cache_state(lists, new);
        ret
    }

    /*
     * Mark a btree node dirty: set the flag and, if the node is already
     * hashed, transition cache_state CLEAN → DIRTY. For nodes that aren't
     * yet hashed (e.g. fresh alloc out of prealloc_nodes — set_dirty fires
     * before hash insert there), only the flag is set; the eventual
     * hash-insert transition picks DIRTY via btree_node_live_state(b).
     *
     * Caller holds write lock on @b (commit / new-alloc path).
     * Idempotent if @b is already dirty.
     */
    pub fn btree_node_set_dirty(&self, b: &BtreeNode) {
        let mut lists = self.btree.cache.lock.lock();
        if b.flags.swap_dirty(true) {
            return;
        }
        if state_hashed(b.cache_state(&lists)) {
            let _ = self.btree_node_transition_state_locked(&mut lists, b, State::DIRTY);
        }
    }

    /*
     * Settle a hashed node's cache_state from its current flag bits. Called from
     * the !rearm branch of __btree_node_write_done after the cmpxchg has cleared
     * write_in_flight; if the dirty bit is also clear the node moves to CLEAN,
     * otherwise btree_node_live_state returns DIRTY and the transition is a
     * no-op.
     *
     * Skipped when the node isn't in a hashed state — e.g. a node can sit on the
     * freeable list with a write still in flight, and we don't want to drag it
     * back into the live lists.
     */
    pub fn btree_node_write_done_clean(&self, b: &BtreeNode) {
        let mut lists = self.btree.cache.lock.lock();
        if state_hashed(b.cache_state(&lists)) {
            let _ = self.btree_node_transition_state_locked(&mut lists, b, b.live_state());
        }
    }

    pub fn btree_node_transition_state(&self, b: &BtreeNode, target: State) -> Result<(), BchError> {
        let mut lists = self.btree.cache.lock.lock();
        self.btree_node_transition_state_locked(&mut lists, b, target)
    }

    #[inline(always)]
    pub(super) fn btree_cache_find(&self, k: &BkeyI) -> Option<&BtreeNode> {
        let v = btree_ptr_hash_val(BkeySC::from(k));

        self.btree.cache.table.get_entry(&v, &read_lock()).map(|b| unsafe { cached_node(self, b) })
    }
}
