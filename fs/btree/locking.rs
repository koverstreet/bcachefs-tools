// SPDX-License-Identifier: GPL-2.0

//! Btree node locking: the locks a transaction's paths hold, taking, relocking,
//! upgrading and dropping them - and taking another lock inside a transaction
//! attempt without blocking on it while holding them.

#![allow(non_camel_case_types)]

use crate::btree::iter::{BtreeTrans, TransAttempt};
use crate::btree::lock_graph::bch2_six_check_for_deadlock;
use crate::btree::node::BtreeNode;
use crate::btree::path::{btree_path_poison, BtreePath, NodePtr, PathIdx, PathNode, PathSnapshot, Paths};
use crate::c;
use crate::cstructs::c::BtreeNodeFlags;
use crate::debug::params::debug_check_btree_locking;
use crate::errcode::{bch_errcode, BchError};
use crate::format::{Bpos, Btree};
use crate::fs::Fs;
use crate::locking::six::{__six_lock_init, six_lock_counts, six_trylock_ip, SixHold};
use crate::locking::{memalloc_flags_restore, memalloc_flags_save, rust_lockdep_set_notrack_class, AsLocked, Exclusive,
                     ExclusiveGuard, Locked, RawTimedLock, PF_MEMALLOC_NOIO};
use crate::util::place::Placed;
use crate::prt;
use crate::debug::trace::{events, TraceEvent};
use crate::util::backtrace::{prt_current_backtrace, prt_symbol};
use crate::util::closure::closure_wait_event_timeout_by;
use crate::util::ffi::Opaque;
use crate::util::futex::EventCount;
use crate::util::kernel::{jiffies, likely, msecs_to_jiffies, race_fault, time_after, HZ, READ};
use crate::util::percpu::{bch2_migrate_disable, bch2_migrate_enable, bch2_raw_smp_processor_id};
use crate::util::rcu::SrcuReadGuard;
use crate::util::sleep::Sleep;
#[cfg(CONFIG_BCACHEFS_LOCK_TIME_STATS)]
use crate::util::{kernel::local_clock, time_stats::TimeStats};
use crate::util::Printbuf;
use core::sync::atomic::{fence, Ordering::{Acquire, Relaxed}};
use core::ffi::{c_int, c_long, c_uint, c_ulong};
use core::ptr::{self, NonNull};
use cstruct_macros::c_enum;

/* DOC_LATEX(btree-locking)
 *
 * Bcachefs uses SIX locks (shared, intent, exclusive) for btree nodes rather
 * than traditional read/write locks. The three states are:
 *
 * \begin{itemize}
 * \item \textbf{Shared}: Does not conflict with other shared locks (like a read lock)
 * \item \textbf{Intent}: Conflicts with other intent locks but not shared locks
 * \item \textbf{Exclusive}: Conflicts with everything (like a write lock)
 * \end{itemize}
 *
 * \paragraph{Why intent locks?}
 *
 * With a regular read/write lock, a read lock cannot be upgraded to a write
 * lock---that leads to deadlock when multiple threads with read locks try to
 * upgrade simultaneously. With complicated data structures like btrees, updates
 * often need to hold write locks for exclusion with other updates for much
 * longer than the part where they actually modify data that needs exclusion
 * from readers.
 *
 * Consider a btree node split. The update starts at a leaf node and discovers
 * it needs to split. Before starting the split, it must acquire a write lock
 * on the parent node---primarily to avoid deadlocking with other splits. It
 * needs at least a read lock on the parent to lock the path to the child node,
 * but it cannot upgrade that read lock to a write lock (to update the parent
 * with pointers to the new children) because that would deadlock with threads
 * splitting sibling leaf nodes.
 *
 * Intent locks solve this. When doing a split, we acquire an intent lock on
 * the parent---exclusive locks (for the actual in-memory modification) are
 * only ever held while modifying in-memory btree contents, which is a much
 * shorter duration than the entire split operation (which requires waiting for
 * new nodes to be written to disk). Readers can continue accessing the parent
 * throughout the split; only the final pointer update requires exclusive
 * access.
 *
 * \paragraph{Parent-child ordering}
 *
 * Intent locks with only three states do introduce another potential deadlock:
 *
 * \begin{verbatim}
 *     Thread A                        Thread B
 *     read            | Parent |      intent
 *     intent          | Child  |      intent
 * \end{verbatim}
 *
 * Thread B is splitting the child node: it has allocated new nodes and written
 * them out, and now needs an exclusive lock on the parent to add the new
 * pointers (after which it will free the old child). Thread A just wants to
 * insert into the child---it has a read lock on the parent, has looked up the
 * child node, and is waiting on thread B to get an intent lock on the child.
 *
 * But thread A has blocked thread B from taking its exclusive lock on the
 * parent, and thread B cannot drop its intent lock on the child until after
 * the new nodes are visible and the old child is freed.
 *
 * The solution: we drop read locks on parent nodes \emph{before} taking intent
 * locks on child nodes. This might cause us to race with the node being freed,
 * so after grabbing the intent lock we verify the node is still valid and redo
 * the traversal if necessary.
 *
 * \paragraph{Sequence numbers and optimistic relocking}
 *
 * SIX locks include embedded sequence numbers, incremented when taking and
 * releasing exclusive locks (much like seqlocks). This allows us to
 * aggressively drop locks---we can usually retake the lock by checking the
 * sequence number rather than redoing the full btree traversal. We also use
 * this for \texttt{try\_upgrade()}: if we discover we need an intent lock (e.g.
 * for a split, or because the caller is inserting into a leaf node they did
 * not get an intent lock for), we can often upgrade without unwinding and
 * redoing the traversal.
 *
 * \paragraph{Cycle detection}
 *
 * Bcachefs uses database-style cycle detection to avoid deadlocks entirely.
 * Before a transaction sleeps waiting on a contended lock, it invokes
 * \texttt{bch2\_check\_for\_deadlock()}, which walks the graph of transactions
 * waiting on locks. The algorithm follows the chain of dependencies: for each
 * lock a transaction holds, check if any other transaction is waiting on that
 * lock; if so, recursively check what locks \emph{that} transaction holds, and
 * so on.
 *
 * If the walk returns to the original transaction, a cycle exists. One
 * transaction in the cycle is selected to abort: it releases all its locks and
 * restarts from the beginning. The transaction layer is designed so that all
 * operations are idempotent and can be safely restarted at any point.
 *
 * This approach eliminates deadlocks entirely and keeps worst-case latency
 * bounded, at the cost of requiring restartable transactions. The same
 * restart infrastructure also provides crash resilience: since every operation
 * can be interrupted and restarted, the filesystem is inherently resilient to
 * interruption at any point---including during recovery itself.
 *
 * The cycle detector runs only when a transaction would block, so it adds no
 * overhead to the fast path. When cycles are detected, they are broken
 * immediately rather than timing out, keeping latency predictable.
 */

/* path lock state */

/* matches six lock types */
c_enum! {
    #[closed]
    pub enum btree_node_locked_type: i32 {
        BTREE_NODE_UNLOCKED = -1,
        BTREE_NODE_READ_LOCKED = c::SIX_LOCK_read as i32,
        BTREE_NODE_INTENT_LOCKED = c::SIX_LOCK_intent as i32,
        BTREE_NODE_WRITE_LOCKED = c::SIX_LOCK_write as i32,
    }
}

/* trans locked state */

impl BtreeTrans<'_> {
    #[cfg(CONFIG_BCACHEFS_LOCK_TIME_STATS)]
    pub(crate) fn stats(&self) -> Option<&c::btree_transaction_stats> {
        self.fs().btree.trans.stats.get(self.fn_idx.get() as usize)
    }

    pub(crate) fn lock_hold_time_update(&self, path: &BtreePath, level: usize) {
        #[cfg(CONFIG_BCACHEFS_LOCK_TIME_STATS)]
        if let Some(s) = self.stats() {
            s.lock_hold_times.update_at(path.l[level].lock_taken_time, local_clock());
        }
        #[cfg(not(CONFIG_BCACHEFS_LOCK_TIME_STATS))]
        let _ = (path, level);
    }

    pub(crate) fn maybe_disable_migrate(&self) {
        /*
         * Pin to CPU while btree locks are held: keeps cache footprint
         * hot, and per-CPU cursors (e.g. inode allocation) stable
         * across transaction restarts. Released in trans_set_unlocked,
         * so any wait that goes through bch2_trans_unlock(_long)
         * happens with migration enabled - including the cond_resched
         * in bch2_trans_begin and the freezer-visible window during
         * suspend.
         */
        let shard_cpu = self.shard_cpu.get();
        if !self.migrate_disabled() &&
           shard_cpu >= 0 &&
           shard_cpu as c_uint == unsafe { bch2_raw_smp_processor_id() } {
            self.set_migrate_disabled(true);
            unsafe { bch2_migrate_disable() };
        }
    }

    pub(crate) fn enable_migrate(&self) {
        if self.migrate_disabled() {
            self.set_migrate_disabled(false);
            unsafe { bch2_migrate_enable() };
        }
    }

    /// Lockdep gets no caller ip, which Rust can't name.
    pub(crate) fn mark_locked(&self, trylock: bool) {
        if self.locked() {
            return;
        }

        self.set_locked(true);
        self.last_unlock_ip.set(0);
        #[cfg(CONFIG_DEBUG_LOCK_ALLOC)]
        self.dep_map.acquire(trylock, false, 0);
        #[cfg(not(CONFIG_DEBUG_LOCK_ALLOC))]
        let _ = trylock;

        self.set_pf_memalloc_noio(memalloc_flags_save(PF_MEMALLOC_NOIO) != 0);

        self.maybe_disable_migrate();
    }

    /// @ret_ip is C's _RET_IP_ - ret_ip!() in the entry point - for
    /// bch2_trans_unlocked_or_in_restart_error().
    pub(crate) fn mark_unlocked(&self, ret_ip: c_ulong) {
        if !self.locked() {
            return;
        }

        self.set_locked(false);
        self.last_unlock_ip.set(ret_ip);
        #[cfg(CONFIG_DEBUG_LOCK_ALLOC)]
        self.dep_map.release(0);

        memalloc_flags_restore(PF_MEMALLOC_NOIO, if self.pf_memalloc_noio() { PF_MEMALLOC_NOIO } else { 0 });
    }
}

pub(crate) fn btree_node_pos(b: &c::btree_bkey_cached_common) -> Bpos {
    match BtreeNode::from_common(b) {
        Some(b) => unsafe { b.contents() }.key.k.p,
        None    => c::bkey_cached::from_common(b).expect("a path node, neither btree node nor key cache entry").key.pos,
    }
}

struct GetLocksFail {
    l:    usize,
    /// the node, and its lock's seq and counts when it failed - the level
    /// loses its node right after - or why there was none
    node: Result<Option<(NonNull<c::btree_bkey_cached_common>, u32, c::six_lock_count)>, BchError>,
}

/* Btree node locking: */

impl BtreeTrans<'_> {
    /// How the transaction's paths - but @skip - hold @b, at @level.
    pub(crate) fn node_lock_counts(&self, paths: &Paths<'_>, skip: Option<PathIdx>,
                                   b: NonNull<c::btree_bkey_cached_common>, level: usize) -> c::six_lock_count {
        let mut ret = c::six_lock_count::default();

        for i in self.path_idxs().filter(|&i| Some(i) != skip) {
            let path = &paths[i];
            if path.l[level].b.is(b.as_ptr()) && path.locked(level) {
                ret.n[path.locked_type(level) as usize] += 1;
            }
        }

        ret
    }

    /* unlock */

    /*
     * Updates the saved lock sequence number, so that bch2_btree_node_relock() will
     * succeed:
     */
    #[inline(never)]
    pub(crate) fn node_unlock_write(&self, paths: &mut Paths<'_>, idx: PathIdx, level: usize) {
        self.node_unlock_write_inlined(paths, idx, level)
    }

    /// node_unlock_write(), for the commit's hot path.
    #[inline(always)]
    pub(crate) fn node_unlock_write_inlined(&self, paths: &mut Paths<'_>, idx: PathIdx, level: usize) {
        let b = {
            let path = &paths[idx];
            ebug_assert!(path.locked_node(level).is_some() && path.write_locked(level),
                         "unlocking a node for write that the path hasn't write locked");
            ebug_assert!(path.lock_seq_matches(level), "unlocking a node for write: path's lock seq stale");
            path.node(level).as_ptr()
        };

        self.node_write_unlock_seqs(paths, unsafe { &*b });
        paths[idx].unlock_write_level(level);
    }

    #[inline]
    fn node_write_unlock_seqs(&self, paths: &mut Paths<'_>, b: &c::btree_bkey_cached_common) {
        let (level, seq) = (b.level as usize, b.lock.six().seq.get());

        if b.lock.six().write_lock_recurse.get() == 0 {
            for i in self.path_idxs() {
                let linked = &mut paths[i];
                if linked.has_node(level, b, seq) {
                    linked.l[level].lock_seq += 1;
                }
            }
        }
    }

    /// A write lock taken outside any path released: __bch2_btree_node_unlock_write().
    pub(crate) fn node_unlock_write_pathless(&self, paths: &mut Paths<'_>, b: &BtreeNode) {
        self.node_write_unlock_seqs(paths, &b.c);
        unsafe { crate::locking::six::six_unlock_ip(b.c.lock.six(), c::six_lock_type::SIX_LOCK_write, crate::ret_ip!()) };
    }

    pub(crate) fn node_unlock(&self, paths: &mut Paths<'_>, idx: PathIdx, level: usize) {
        ebug_assert!(level < c::BTREE_MAX_DEPTH as usize, "unlocking level {level}");

        if paths[idx].write_locked(level) {
            self.node_unlock_write(paths, idx, level);
        }

        let path = &mut paths[idx];
        if path.unlock_level(level) {
            self.lock_hold_time_update(path, level);
        }
    }

    pub(crate) fn path_unlock(&self, paths: &mut Paths<'_>, idx: PathIdx) {
        while paths[idx].nodes_locked != 0 {
            self.node_unlock(paths, idx, paths[idx].lowest_level_locked());
        }
    }

    /* lock */

    /*
     * Lock a btree node if we already have it locked on one of our linked
     * iterators:
     */
    fn node_lock_holder(&self, paths: &Paths<'_>, idx: PathIdx, b: *const c::btree_bkey_cached_common, level: usize,
                        want: c::six_lock_type) -> Option<PathIdx> {
        self.path_idxs().find(|&i| {
            i != idx && paths[i].l[level].b.is(b) && paths[i].locked_type(level) as i32 >= want as i32
        })
    }

    /// Path @idx's level, locked as @want by another hold on @b from a path of
    /// ours that has it: what the level held before dropped.
    fn level_lock_from(&self, paths: &mut Paths<'_>, idx: PathIdx, level: usize, b: *const c::btree_bkey_cached_common,
                       want: c::six_lock_type) -> bool {
        let Some(i) = self.node_lock_holder(paths, idx, b, level, want) else { return false };
        let (src, dst) = paths.pair(i, idx);
        let (Some(node), Some(hold)) = (src.locked_node(level), src.increment_level(level, want)) else { return false };

        ebug_assert!(!dst.write_locked(level), "a level held for write, its lock replaced");
        if dst.unlock_level(level) {
            self.lock_hold_time_update(dst, level);
        }
        dst.put_level(level, &node, hold);
        true
    }

    /// level_lock_from(), of the level's own node, if its lock seq says it's
    /// still the path's.
    fn level_relock_from(&self, paths: &mut Paths<'_>, idx: PathIdx, level: usize, want: c::six_lock_type) -> bool {
        let b = paths[idx].l[level].b.as_ptr();
        paths[idx].lock_seq_matches(level) && self.level_lock_from(paths, idx, level, b, want)
    }

    /// @b's lock, by @lock: given the transaction's waiter, which the cycle
    /// detector reads, and the cycle detector to call.
    fn node_lock_nopath<H>(&self, b: &c::btree_bkey_cached_common, lock_may_not_fail: bool,
                           lock: impl FnOnce(&c::six_lock_waiter, &mut dyn FnMut() -> c_int) -> Result<H, c_int>)
                           -> Result<H, BchError> {
        self.set_lock_may_not_fail(lock_may_not_fail);
        self.lock_must_abort.store(0, Relaxed);
        self.locking.set(ptr::from_ref(b).cast_mut());

        /* trans->locking_hash_val is set by the caller; it must be the
         * hash of the key used to look up this node (not the node's
         * current hash_val), so that bch2_six_check_for_deadlock catches
         * the case where the node was reclaimed AND re-hashed to a new
         * identity. 0 disables the check (lock_root, cached, relock). */

        let ret = lock(&self.locking_wait, &mut || bch2_six_check_for_deadlock(self, b)).map_err(|e| {
            assert!(!lock_may_not_fail, "a lock that may not fail failed: {}", BchError::from_raw(e).msg());
            if e == -(c::ENOMEM as c_int) {
                self.restart(bch_errcode::BCH_ERR_transaction_restart_lock_waitlist_alloc)
            } else {
                BchError::from_raw(e)
            }
        });

        self.locking.set(ptr::null_mut());

        self.maybe_disable_migrate();

        #[cfg(CONFIG_BCACHEFS_DEBUG)]
        self.fs().event_trace(events::btree_path_lock, |out| {
            prt!(out, "{} ret {}\nbtree {} level {} lock seq {} node {:p}",
                 self.fn_name(), ret.as_ref().err().map_or("(No error)", |e| e.msg()),
                 Btree(b.btree_id.into()), b.level, b.lock.six().seq.get(), ptr::from_ref(b));
        });
        ret
    }

    pub(crate) fn node_lock<'b>(&self, paths: &Paths<'_>, idx: PathIdx, b: &'b c::btree_bkey_cached_common,
                                level: usize, ty: c::six_lock_type) -> Result<SixHold<'b>, BchError> {
        ebug_assert!(level < c::BTREE_MAX_DEPTH as usize, "locking level {level}");
        self.verify_not_unlocked_or_in_restart();

        match b.lock.try_lock_as(ty) {
            Some(held) => Ok(held),
            None       => self.node_lock_slowpath(paths, idx, b, level, ty),
        }
    }

    /// @b's lock, past the trylock: another hold from a path of ours that has
    /// it, or waited for.
    fn node_lock_slowpath<'b>(&self, paths: &Paths<'_>, idx: PathIdx, b: &'b c::btree_bkey_cached_common,
                              level: usize, ty: c::six_lock_type) -> Result<SixHold<'b>, BchError> {
        match self.node_lock_holder(paths, idx, b, level, ty).and_then(|i| paths[i].increment_level(level, ty)) {
            Some(held) => {
                let ty = held.into_raw();
                Ok(unsafe { SixHold::from_raw(&b.lock, ty) })
            }
            None => self.node_lock_contended(b, ty, paths[idx].ip_allocated()),
        }
    }

    /// @ip is lockdep's: the path's ip_allocated().
    fn node_lock_contended<'b>(&self, b: &'b c::btree_bkey_cached_common, ty: c::six_lock_type,
                               ip: c_ulong) -> Result<SixHold<'b>, BchError> {
        #[cfg(CONFIG_BCACHEFS_LOCK_TIME_STATS)]
        let contended_start = local_clock();
        let ret = self.node_lock_nopath(b, false, |wait, should_sleep_fn| {
            b.lock.lock_waiting_as(ty, wait, should_sleep_fn, ip, true)
        });
        #[cfg(CONFIG_BCACHEFS_LOCK_TIME_STATS)]
        if let Some(s) = self.stats() {
            s.lock_wait_times.update_at(contended_start, local_clock());
        }
        ret
    }

    /// Path @idx's node at @level, which it holds for intent, for write too:
    /// bch2_btree_node_lock_write().
    #[inline]
    pub(crate) fn node_lock_write(&self, paths: &mut Paths<'_>, idx: PathIdx, level: usize)
        -> Result<(), BchError>
    {
        self.node_lock_write_may_fail(paths, idx, level, false)
    }

    /// node_lock_write(), for a caller that can't take a restart: the cycle
    /// detector waits rather than fail it - bch2_btree_node_lock_write_nofail().
    pub(crate) fn node_lock_write_nofail(&self, paths: &mut Paths<'_>, idx: PathIdx, level: usize) {
        self.node_lock_write_may_fail(paths, idx, level, true).expect("a write lock that may not fail, failed");
    }

    fn node_lock_write_may_fail(&self, paths: &mut Paths<'_>, idx: PathIdx, level: usize, lock_may_not_fail: bool)
        -> Result<(), BchError>
    {
        let b = paths[idx].locked_node(level).expect("write locking a node the path doesn't hold");
        let b = unsafe { &*ptr::from_ref(b.node()) };
        ebug_assert!(paths[idx].l[level].lock_seq == b.lock.six().seq.get());
        ebug_assert!(paths[idx].intent_locked(level));

        /*
         * six locks are unfair, and read locks block while a thread wants a
         * write lock: thus, we need to tell the cycle detector we have a write
         * lock _before_ taking the lock:
         */
        paths[idx].mark_locked_noreset(level, btree_node_locked_type::BTREE_NODE_WRITE_LOCKED);

        if six_trylock_ip(b.lock.six(), c::six_lock_type::SIX_LOCK_write, crate::ret_ip!()) {
            Ok(())
        } else {
            self.node_lock_write_contended(paths, idx, b, lock_may_not_fail, crate::ret_ip!())
        }
    }

    /// @ip is lockdep's: C's _RET_IP_.
    #[inline(never)]
    fn node_lock_write_contended(&self, paths: &mut Paths<'_>, idx: PathIdx, b: &c::btree_bkey_cached_common,
                                 lock_may_not_fail: bool, ip: c_ulong) -> Result<(), BchError> {
        self.locking_hash_val.set(0);
        self.locking_root_id.set(Btree(u32::MAX));

        /*
         * Must drop our read locks before calling six_lock_write() -
         * six_unlock() won't do wakeups until the reader count
         * goes to 0, and it's safe because we have the node intent
         * locked:
         */
        self.node_lock_nopath(b, lock_may_not_fail, |wait, should_sleep_fn| {
            paths.lock_write_marked(idx, b.level as usize, wait, should_sleep_fn, ip)
        })
    }

    /*
     * Lock @b when the caller doesn't already have a path for it: create a
     * temporary unlocked path, take the lock, then record the lock on the path
     * so the cycle detector can find us as the holder.
     *
     * @hash_val is the node's expected identity — btree_ptr_hash_val() of the key
     * it was found under, or a snapshot of b->hash_val taken while the pointer
     * was known valid. It arms the node-reuse check in
     * bch2_six_check_for_deadlock(): if the identity is gone by the time we'd
     * sleep, the lock attempt aborts with no_btree_node_reused rather than
     * parking behind an off-path holder (btree_node_reclaim's trylocks) that may
     * hold the reused node's lock indefinitely.
     *
     * Pass 0 only when reclaim genuinely can't take the node — e.g. the write
     * completion path, which owns the write_in_flight that blocks reclaim — or
     * for key cache locks, which the check doesn't cover. Beware flags another
     * thread can clear: a journal pin doesn't pin (the node can be written and
     * the pin dropped concurrently), and neither does dirty on its own.
     *
     * Caller releases via bch2_btree_node_unlock_with_path().
     *
     * May return a transaction_restart; wrap in lockrestart_do().
     */
    pub(crate) fn node_lock_with_path(&self, paths: &mut Paths<'_>, b: &c::btree_bkey_cached_common, ty: c::six_lock_type,
                                      hash_val: u64) -> Result<PathIdx, BchError> {
        let path_idx = self.path_get_unlocked_mut(paths, Btree(b.btree_id.into()), b.level.into(), btree_node_pos(b),
                                                  b.cached, crate::ret_ip!());

        self.locking_hash_val.set(hash_val);
        self.locking_root_id.set(Btree(u32::MAX));

        let level = b.level as usize;
        ebug_assert!(level < c::BTREE_MAX_DEPTH as usize, "locking level {level}");
        self.verify_not_unlocked_or_in_restart();

        if let Some(held) = b.lock.try_lock_as(ty) {
            paths[path_idx].put_level(level, b, held);
        } else if !self.level_lock_from(paths, path_idx, level, b, ty) {
            let held = match self.node_lock_contended(b, ty, paths[path_idx].ip_allocated()) {
                Ok(held) => held,
                Err(e) => {
                    self.path_put(paths, path_idx, true);
                    return Err(e);
                }
            };
            paths[path_idx].put_level(level, b, held);
        }
        paths[path_idx].lock_taken(level);

        Ok(path_idx)
    }

    /// @f on @b, locked by node_lock_with_path(), and unlocked after, its path
    /// put: retried on a transaction restart, @f's included.
    pub(crate) fn node_locked_do<T>(&self, b: &c::btree_bkey_cached_common, ty: c::six_lock_type, hash_val: u64,
                                    mut f: impl FnMut(&mut Paths<'_>, PathIdx) -> Result<T, BchError>)
        -> Result<T, BchError>
    {
        crate::btree::trans_attempt::lockrestart_do(self, |_| {
            let mut paths = self.paths();
            let idx = self.node_lock_with_path(&mut paths, b, ty, hash_val)?;
            let ret = f(&mut paths, idx);

            self.node_unlock(&mut paths, idx, b.level.into());
            self.path_put(&mut paths, idx, true);
            ret
        })
    }

    /* relock */

    fn get_locks_fail_to_text(&self, paths: &Paths<'_>, out: &mut Printbuf, old_path: &PathSnapshot, idx: PathIdx,
                              f: &GetLocksFail) {
        let path = &paths[idx];
        prt!(out, "{} {} l={} seq={} node seq=", path.pos, path.btree_id(), f.l, path.l[f.l].lock_seq);
        match f.node {
            Ok(Some((b, seq, total))) => {
                prt!(out, "{}", seq);

                let c = self.node_lock_counts(paths, None, b, f.l);
                prt!(out, " self locked {}.{}.{}", c.n[0], c.n[1], c.n[2]);

                prt!(out, " total locked {}.{}.{}", total.n[0], total.n[1], total.n[2]);
            }
            Ok(None) => prt!(out, "{}", BchError::from_raw(0).msg()),
            Err(e)   => prt!(out, "{}", e.msg()),
        }

        out.newline();
        old_path.to_text(out, idx);
    }

    /// The locks the path wants, from its level up: what failed, if one did.
    /// With @restart, a failure restarts the transaction with it.
    fn path_get_locks(&self, paths: &mut Paths<'_>, idx: PathIdx, upgrade: bool,
                      restart: Option<bch_errcode>) -> Result<(), GetLocksFail> {
        for l in paths[idx].level() as usize.. {
            if paths[idx].node(l).is_null() {
                break;
            }

            if !(if upgrade { self.node_upgrade(paths, idx, l) } else { self.node_relock(paths, idx, l) }) {
                return Err(self.path_get_locks_err(paths, idx, l, upgrade, restart));
            }

            if l + 1 >= paths[idx].locks_want() as usize {
                break;
            }
        }

        Ok(())
    }

    fn path_get_locks_err(&self, paths: &mut Paths<'_>, idx: PathIdx, l: usize, upgrade: bool,
                          restart: Option<bch_errcode>) -> GetLocksFail {
        let path = &paths[idx];
        let f = GetLocksFail {
            l,
            node: path.l[l].b.get().map(|_| path.unlocked_node(l).map(|b| {
                (b.ptr(), b.lock().six().seq.get(), six_lock_counts(b.lock().six()))
            })),
        };

        /*
         * Do transaction restart before unlocking, so we don't pop
         * should_be_locked asserts
         */
        if let Some(e) = restart {
            let _ = self.restart(e);
        } else if paths[idx].should_be_locked() && self.restarted().0 == 0 {
            if upgrade {
                paths[idx].bits.set_locks_want(l as u8);
            }
            return f;
        }

        self.path_unlock(paths, idx);

        /*
         * When we fail to get a lock, we have to ensure that any child nodes
         * can't be relocked so bch2_btree_path_traverse has to walk back up to
         * the node that we failed to relock:
         */
        let err = if upgrade {
            bch_errcode::BCH_ERR_no_btree_node_upgrade
        } else {
            bch_errcode::BCH_ERR_no_btree_node_relock
        };
        for level in &mut paths[idx].l[..=l] {
            level.b = NodePtr::err(err.into());
        }

        f
    }

    pub(crate) fn node_relock(&self, paths: &mut Paths<'_>, idx: PathIdx, level: usize) -> bool {
        let path = &mut paths[idx];
        ebug_assert!(!(path.locked(level) && !path.write_locked(level) &&
                              path.locked_type(level) as i32 != path.lock_want_six(level) as i32),
                            "relocking level {level}: held lock isn't the one wanted");

        path.locked(level) ||
            (path.l[level].b.is_node() && self.node_relock_slowpath(paths, idx, level, true))
    }

    pub(crate) fn node_relock_notrace(&self, paths: &mut Paths<'_>, idx: PathIdx, level: usize) -> bool {
        let path = &paths[idx];
        ebug_assert!(!(path.locked(level) && path.locked_type_nowrite(level) as i32 != path.lock_want_six(level) as i32),
                     "relocking level {level}: held lock isn't the one wanted");

        path.locked(level) ||
            (path.l[level].b.is_node() && self.node_relock_slowpath(paths, idx, level, false))
    }

    pub(crate) fn node_relock_slowpath(&self, paths: &mut Paths<'_>, idx: PathIdx, level: usize, trace: bool) -> bool {
        let want = paths[idx].lock_want_six(level);

        let relocked = !race_fault() && {
            assert!(paths[idx].l[level].b.is_node(), "relocking a level with no node");
            paths[idx].relock_level(level, want) || self.level_relock_from(paths, idx, level, want)
        };

        if relocked {
            let path = &mut paths[idx];
            /* a matching seq must mean the node is still the one we had: */
            let b = path.locked_node(level).expect("a relocked level has its node");
            ebug_assert!(u32::from(b.btree_id) == path.btree_id().0 && b.level as usize == level,
                                "relocked a node that isn't the path's");
            path.lock_taken(level);
            return true;
        }

        if trace && !self.notrace_relock_fail() {
            self.fs().event_inc_trace(events::btree_path_relock_fail, |out| {
                prt!(out, "{}\n", self.fn_name());
                self.path_to_text(out, paths, idx);
            });
        }
        false
    }

    /* upgrade */

    pub(crate) fn node_upgrade(&self, paths: &mut Paths<'_>, idx: PathIdx, level: usize) -> bool {
        use btree_node_locked_type::*;

        let path = &mut paths[idx];
        if !path.is_btree_node(level) {
            return false;
        }

        match path.lock_want(level) {
            BTREE_NODE_UNLOCKED => {
                ebug_assert!(!path.locked(level), "upgrade: level {level} locked, but not wanted");
                return true;
            }
            BTREE_NODE_READ_LOCKED => {
                ebug_assert!(!path.intent_locked(level), "upgrade: level {level} intent locked, but read wanted");
                return self.node_relock(paths, idx, level);
            }
            BTREE_NODE_INTENT_LOCKED => {}
            BTREE_NODE_WRITE_LOCKED => panic!("upgrade: a write lock wanted"),
        }

        if path.intent_locked(level) {
            return true;
        }

        if race_fault() {
            return false;
        }

        if path.locked(level) {
            if path.try_upgrade_level(level) {
                return true;
            }
        } else if path.relock_level(level, c::six_lock_type::SIX_LOCK_intent) {
            return true;
        }

        if self.level_relock_from(paths, idx, level, c::six_lock_type::SIX_LOCK_intent) {
            return true;
        }

        self.fs().event_inc_trace(events::btree_path_upgrade_fail, |out| {
            prt!(out, "{}\n", self.fn_name());
            self.path_to_text(out, paths, idx);
        });
        false
    }

    /* Btree path locking: */

    pub(crate) fn path_relock_norestart(&self, paths: &mut Paths<'_>, idx: PathIdx) -> bool {
        let ret = self.path_get_locks(paths, idx, false, None).is_ok();
        self.verify_locks(paths);
        ret
    }

    pub(crate) fn path_relock(&self, paths: &mut Paths<'_>, idx: PathIdx) -> Result<(), BchError> {
        let old_path = <events::trans_restart_relock_path as TraceEvent>::trace_enabled()
            .then(|| PathSnapshot::of(&paths[idx]));

        let ret = self.path_get_locks(paths, idx, false, None).map_err(|f| {
            match old_path {
                Some(old_path) => self.fs().event_inc_trace(events::trans_restart_relock_path, |out| {
                    prt!(out, "{}\n", self.fn_name());
                    self.get_locks_fail_to_text(paths, out, &old_path, idx, &f);
                }),
                None => self.fs().event_inc(events::trans_restart_relock_path),
            }
            self.restart(bch_errcode::BCH_ERR_transaction_restart_relock_path)
        });

        self.verify_locks(paths);
        ret
    }

    pub(crate) fn path_set_should_be_locked(&self, paths: &mut Paths<'_>, idx: PathIdx) {
        let path = &mut paths[idx];
        ebug_assert!(path.locked(path.level() as usize));

        if !path.should_be_locked() {
            path.bits.set_should_be_locked(true);
            #[cfg(CONFIG_BCACHEFS_DEBUG)]
            self.fs().event_trace(events::btree_path_should_be_locked, |out| {
                prt!(out, "{}\n", self.fn_name());
                self.path_to_text_short(out, paths, idx);
            });
        }
    }

    pub(crate) fn path_upgrade_norestart(&self, paths: &mut Paths<'_>, idx: PathIdx, new_locks_want: usize) -> bool {
        paths[idx].bits.set_locks_want(new_locks_want as u8);

        /*
         * If we need it locked, we can't touch it. Otherwise, we can return
         * success - bch2_path_get() will use this path, and it'll just be
         * retraversed:
         */
        let ret = self.path_get_locks(paths, idx, true, None).is_ok() || !paths[idx].should_be_locked();

        self.verify_path_locks(paths, idx);
        ret
    }

    /// @ip is C's _RET_IP_, for the trace.
    #[inline]
    pub(crate) fn path_upgrade(&self, paths: &mut Paths<'_>, idx: PathIdx, new_locks_want: usize,
                               ip: c_ulong) -> Result<(), BchError> {
        let new_locks_want = new_locks_want.min(c::BTREE_MAX_DEPTH as usize);
        let path = &paths[idx];

        if likely(path.locks_want() as usize >= new_locks_want && path.nodes_locked != 0) {
            Ok(())
        } else {
            self.__path_upgrade(paths, idx, new_locks_want, ip)
        }
    }

    #[inline(never)]
    pub(crate) fn __path_upgrade(&self, paths: &mut Paths<'_>, idx: PathIdx, new_locks_want: usize,
                                 ip: c_ulong) -> Result<(), BchError> {
        let path           = &mut paths[idx];
        let old_locks      = path.nodes_locked;
        let old_locks_want = path.locks_want() as usize;

        path.bits.set_locks_want(old_locks_want.max(new_locks_want) as u8);

        let Err(f) = self.path_get_locks(paths, idx, true, Some(bch_errcode::BCH_ERR_transaction_restart_upgrade)) else {
            self.verify_locks(paths);
            return Ok(());
        };

        /*
         * XXX: this is ugly - we'd prefer to not be mucking with other
         * iterators in the btree_trans here.
         *
         * On failure to upgrade the iterator, setting iter->locks_want and
         * calling get_locks() is sufficient to make bch2_btree_path_traverse()
         * get the locks we want on transaction restart.
         *
         * But if this iterator was a clone, on transaction restart what we did
         * to this iterator isn't going to be preserved.
         *
         * Possibly we could add an iterator field for the parent iterator when
         * an iterator is a copy - for now, we'll just upgrade any other
         * iterators with the same btree id.
         *
         * The code below used to be needed to ensure ancestor nodes get locked
         * before interior nodes - now that's handled by
         * path_traverse_all().
         */
        let path = &mut paths[idx];
        let (cached, btree) = (path.cached(), path.btree_id());
        if !cached && !self.in_traverse_all() {
            for i in self.path_idxs() {
                let linked = &mut paths[i];
                if i != idx &&
                   linked.cached() == cached &&
                   linked.btree_id() == btree &&
                   (linked.locks_want() as usize) < new_locks_want {
                    linked.bits.set_locks_want(new_locks_want as u8);
                    let _ = self.path_get_locks(paths, i, true, None);
                }
            }
        }

        let path = &paths[idx];
        self.fs().event_inc_trace(events::trans_restart_upgrade, |out| {
            prt!(out, "{}\n", self.fn_name());
            prt!(out, "{} ", self.fn_name());
            prt_symbol(out, ip);
            out.newline();
            prt!(out, "{}", c::bbpos { btree: path.btree_id(), pos: path.pos });
            out.newline();
            prt!(out, "locks want {} -> {} level {}\n", old_locks_want, new_locks_want, f.l);
            prt!(out, "nodes_locked {:x} -> {:x}\n", old_locks, path.nodes_locked);
            let node = match f.node {
                Err(e)      => e.msg(),
                Ok(None)    => "(null)",
                Ok(Some(_)) => "(node)",
            };
            prt!(out, "node {} ", node);
            let node = f.node.ok().flatten();
            prt!(out, "path seq {} node seq {}", node.map_or(0, |(_, seq, _)| seq), path.l[f.l].lock_seq);

            if let Some((.., c)) = node {
                prt!(out, " locked {}.{}.{}", c.n[0], c.n[1], c.n[2]);
            }
            prt!(out, "\npath idx {}", idx);
            #[cfg(any(CONFIG_BCACHEFS_LOCK_TIME_STATS, CONFIG_BCACHEFS_DEBUG))]
            {
                prt!(out, " allocated: ");
                prt_symbol(out, path.ip_allocated);
            }
            out.newline();
        });

        self.verify_locks(paths);
        Err(bch_errcode::BCH_ERR_transaction_restart_upgrade.into())
    }

    pub(crate) fn path_downgrade_to(&self, paths: &mut Paths<'_>, idx: PathIdx, new_locks_want: usize) {
        let path = &mut paths[idx];
        #[cfg(CONFIG_BCACHEFS_DEBUG)]
        let old_locks_want = path.locks_want();

        if self.restarted().0 != 0 {
            return;
        }

        ebug_assert!(path.locks_want() as usize >= new_locks_want,
                            "downgrading to locks_want {new_locks_want}, from {}", path.locks_want());

        path.bits.set_locks_want(new_locks_want as u8);

        while paths[idx].nodes_locked != 0 {
            let path = &mut paths[idx];
            let l = path.highest_level_locked();
            if l < path.locks_want() as usize {
                break;
            }
            if l <= path.level() as usize {
                path.downgrade_level(l);
                break;
            }
            self.node_unlock(paths, idx, l);
        }

        self.verify_path_locks(paths, idx);
        #[cfg(CONFIG_BCACHEFS_DEBUG)]
        self.fs().event_trace(events::path_downgrade, |out| {
            prt!(out, "{}\n", self.fn_name());
            prt!(out, "old locks_want: {}\n", old_locks_want);
            self.path_to_text(out, paths, idx);
        });
    }

    pub(crate) fn path_downgrade(&self, paths: &mut Paths<'_>, idx: PathIdx) {
        if cfg!(CONFIG_BCACHEFS_DEBUG) {
            crate::warn_once!(paths[idx].l[0].b == btree_path_poison(), self.fs(),
                              "{}: btree path {:x} outlived trans->paths ({:x})",
                              self.fn_name(), ptr::from_ref(&paths[idx]).addr(), paths.as_ptr().addr());
        }

        let path = &mut paths[idx];
        let new_locks_want = path.level() as usize + (path.intent_ref != 0) as usize;

        if path.locks_want() as usize > new_locks_want {
            self.path_downgrade_to(paths, idx, new_locks_want);
        }
    }

    /* Btree transaction locking: */

    pub(crate) fn downgrade(&self, paths: &mut Paths<'_>) {
        if self.restarted().0 != 0 {
            return;
        }

        for i in self.path_idxs() {
            if paths[i].ref_ != 0 {
                self.path_downgrade(paths, i);
            }
        }
    }

    fn unlock_paths(&self, paths: &mut Paths<'_>) {
        for i in self.path_idxs() {
            self.path_unlock(paths, i);
        }

        /*
         * All locks dropped: submit any btree node writes queued in this
         * trans's context.
         */
        if !self.queued_write_bios.get().is_null() {
            crate::btree::write::trans_submit_write_bios(self);
        }
    }

    pub(crate) fn relock_traced(&self, paths: &mut Paths<'_>, trace: bool, ret_ip: c_ulong) -> Result<(), BchError> {
        self.verify_locks(paths);

        let restarted = self.restarted();
        if restarted.0 != 0 {
            return Err(restarted.into());
        }
        let tracing = trace && <events::trans_restart_relock as TraceEvent>::trace_enabled();

        for i in self.path_idxs() {
            if !paths[i].should_be_locked() {
                continue;
            }

            let old_path = tracing.then(|| PathSnapshot::of(&paths[i]));
            if let Err(f) = self.path_get_locks(paths, i, false, Some(bch_errcode::BCH_ERR_transaction_restart_relock)) {
                if let Some(old_path) = old_path {
                    self.fs().event_inc_trace(events::trans_restart_relock, |out| {
                        prt!(out, "{} ", self.fn_name());
                        prt_symbol(out, ret_ip);
                        prt!(out, "\n");
                        self.get_locks_fail_to_text(paths, out, &old_path, i, &f);
                    });
                } else if trace {
                    self.fs().event_inc(events::trans_restart_relock);
                }
                self.unlock_paths(paths);
                self.verify_locks(paths);
                return Err(bch_errcode::BCH_ERR_transaction_restart_relock.into());
            }
        }

        self.mark_locked(true);
        /* Fresh locked section — re-arm the srcu-held-too-long warning. */
        self.set_srcu_io_submitted(false);
        self.verify_locks(paths);
        Ok(())
    }

    /// Take back the locks unlock() dropped: a transaction restart if they've
    /// changed hands meanwhile. Inlined, as unlock() is, so that the trace of a
    /// relock that fails has the caller's address.
    #[inline(always)]
    pub fn relock(&self) -> Result<(), BchError> {
        if self.locked() && self.restarted().0 == 0 {
            return Ok(());
        }
        self.relock_traced(&mut self.paths(), true, crate::ret_ip!())
    }

    /// relock(), with the path table had already: for what C lends it to.
    #[inline(always)]
    pub(crate) fn relock_from(&self, paths: &mut Paths<'_>) -> Result<(), BchError> {
        if self.locked() && self.restarted().0 == 0 {
            return Ok(());
        }
        self.relock_traced(paths, true, crate::ret_ip!())
    }

    /// relock(), always attempted and without the tracepoint: as
    /// bch2_trans_relock_notrace().
    pub fn relock_notrace(&self) -> Result<(), BchError> {
        self.relock_traced(&mut self.paths(), false, 0)
    }

    /// Inlined, so that last_unlock_ip is the caller's address.
    #[inline(always)]
    pub fn unlock(&self) {
        self.unlock_from(&mut self.paths(), crate::ret_ip!());
    }

    pub(crate) fn unlock_from(&self, paths: &mut Paths<'_>, ret_ip: c_ulong) {
        self.mark_unlocked(ret_ip);

        self.unlock_paths(paths);

        /*
         * Drop the btree cache cannibalize lock too. Holding it across a
         * trans_unlock - i.e. across a sleep - is the recipe for a resource
         * deadlock: cannibalize-holder sleeps waiting on the allocator,
         * allocator needs to grow the btree cache, growing the cache needs
         * cannibalize, but we're holding it. Releasing on trans_unlock means
         * cannibalize is only held over non-sleeping critical sections;
         * callers that need it after a wake re-acquire normally.
         */
        if self.btree_cache_cannibalize_locked() {
            self.btree_cache_cannibalize_unlock();
        }
    }

    pub fn unlock_long(&self) {
        self.unlock_long_from(&mut self.paths(), crate::ret_ip!());
    }

    pub(crate) fn unlock_long_from(&self, paths: &mut Paths<'_>, ret_ip: c_ulong) {
        self.unlock_from(paths, ret_ip);
        self.enable_migrate();

        if !self.srcu_held() {
            return;
        }

        let fs = self.fs();

        for i in self.path_idxs() {
            let path = &mut paths[i];
            if path.cached() && !path.locked(0) {
                path.l[0].b = NodePtr::err(bch_errcode::BCH_ERR_no_btree_node_srcu_reset.into());
            }
        }

        let lock_time = self.srcu_lock_time.get();
        if !self.srcu_io_submitted() &&
           time_after(jiffies(), lock_time.wrapping_add(HZ * 10)) &&
           time_after(jiffies(), lock_time.wrapping_add(srcu_hold_warn_thresh(fs))) {
            let mut msg = crate::log_msg_ratelimited!(fs);

            prt!(&mut msg, "btree trans held srcu lock (delaying memory reclaim) for {} seconds\n",
                 jiffies().wrapping_sub(lock_time) / HZ);
            fs.counters.recent_to_text(&mut msg);

            let _ = prt_current_backtrace(&mut msg, 1);
        }

        drop(unsafe { SrcuReadGuard::from_raw(&fs.btree.trans.barrier, self.srcu_idx.get()) });
        self.set_srcu_held(false);
    }

    /// The SRCU read lock btree nodes are freed under, if it isn't held.
    pub(crate) fn srcu_lock(&self) {
        if !self.srcu_held() {
            self.srcu_idx.set(self.fs().btree.trans.barrier.read_lock().into_raw());
            self.srcu_lock_time.set(jiffies());
            self.set_srcu_held(true);
        }
    }

    /// An attempt's start, for the SRCU lock: dropped if it's been held for
    /// 10ms. @ret_ip is unlock_long's, C's _RET_IP_.
    pub(crate) fn srcu_begin(&self, paths: &mut Paths<'_>, ret_ip: c_ulong) {
        if self.srcu_held() && time_after(jiffies(), self.srcu_lock_time.get().wrapping_add(msecs_to_jiffies(10))) {
            self.unlock_long_from(paths, ret_ip);
        }

        /* Fresh attempt — re-arm the srcu-held-too-long warning (cleared after
         * the unlock_long above has had its chance to fire). */
        self.set_srcu_io_submitted(false);
    }

    /*
     * Returns the timeout to use for the next blocking wait done by this trans.
     *
     * The trans takes an SRCU read lock on bch2_trans_begin(); holding it across
     * a long blocking wait stalls memory reclaim. So if SRCU has been held for
     * longer than HZ when we're about to wait, drop it now (via unlock_long) and
     * let the caller wait the full timeout SRCU-free.
     *
     * Otherwise cap the returned timeout at the unconsumed portion of HZ — the
     * caller's wait will time out before SRCU has been held too long, and on
     * the next call (after the caller's natural retry) we'll be in the
     * elapsed >= HZ branch and drop SRCU here.
     */
    /// None for no cap: SRCU isn't held, or it's just been dropped.
    fn srcu_wait_budget(&self, paths: &mut Paths<'_>) -> Option<c_ulong> {
        if !self.srcu_held() {
            return None;
        }

        let elapsed = jiffies().wrapping_sub(self.srcu_lock_time.get());
        if elapsed >= HZ {
            self.unlock_long_from(paths, crate::ret_ip!());
            return None;
        }
        Some(HZ - elapsed)
    }

    /// Waits on @ev until @cond. For up to a second it keeps the SRCU lock
    /// btree nodes are freed under, so a short wait can relock after; past
    /// that it unlock_long()s, so a long wait doesn't hold up freeing. The
    /// caller must have dropped its btree locks. @cond is called once per
    /// wake, so it may be a trylock.
    pub fn wait_until<T>(&self, ev: &EventCount, mut cond: impl FnMut() -> Option<T>) -> T {
        loop {
            let seen = ev.snapshot();
            if let Some(v) = cond() {
                return v;
            }
            match self.srcu_wait_budget(&mut self.paths()) {
                // timing out is only the budget running out
                Some(left) => { let _ = ev.wait(seen, Sleep::UNINTERRUPTIBLE.timeout(left)); }
                None       => { let Ok(()) = ev.wait(seen, Sleep::UNINTERRUPTIBLE); }
            }
        }
    }

    /// Waits for @b's @flag to clear, under SRCU as wait_until() waits:
    /// trans_wait_on_node_flag().
    pub(crate) fn wait_on_node_flag(&self, paths: &mut Paths<'_>, b: &BtreeNode, flag: fn(&BtreeNodeFlags) -> bool) {
        let clear = |f: BtreeNodeFlags| !flag(&f);

        while !clear(b.flags.load()) {
            match self.srcu_wait_budget(paths) {
                Some(left) => { b.flags.wait_until_timeout(Sleep::UNINTERRUPTIBLE.io(), left, clear); }
                None       => { let Ok(()) = b.flags.wait_until(Sleep::UNINTERRUPTIBLE.io(), clear); }
            }
        }
        fence(Acquire);
    }

    /// Wait for @cl's refs - the waitlists it's on - to be put, for up to
    /// @timeout jiffies, c_long::MAX for no limit: false if they weren't.
    /// Under SRCU in short waits, then without it: trans_closure_sync_timeout().
    pub fn closure_sync_timeout(&self, cl: &Opaque<c::closure>, timeout: c_long) -> bool {
        self.closure_sync_timeout_in(&mut self.paths(), cl, timeout)
    }

    fn closure_sync_timeout_in(&self, paths: &mut Paths<'_>, cl: &Opaque<c::closure>, timeout: c_long) -> bool {
        let mut remaining = timeout;
        while remaining > 0 {
            // SAFETY: a live closure, its count atomic
            if unsafe { c::closure_nr_remaining(cl.as_ptr()) } <= 1 {
                return true;
            }

            let start = jiffies();
            let wait = self.srcu_wait_budget(paths).map_or(remaining, |left| (left as c_long).min(remaining));
            // SAFETY: as above; the waitlist's wake puts its ref
            if unsafe { c::closure_sync_timeout(cl.as_ptr(), wait as c_ulong) } == 0 {
                return true;
            }
            remaining -= jiffies().wrapping_sub(start) as c_long;
        }
        false
    }

    /// Sleep until @cond, woken by @list's closure_wake_up()s, for up to
    /// @timeout jiffies - c_long::MAX for no limit: trans_wait_event_timeout().
    /// What's left of @timeout, at least 1, once @cond is true; 0 if it timed
    /// out first. @cond is checked on the list, so a wake between the check
    /// and the sleep isn't lost.
    pub fn wait_event_timeout(&self, list: &c::closure_waitlist, cond: impl FnMut() -> bool,
                              timeout: c_long) -> c_long {
        closure_wait_event_timeout_by(list, cond, timeout, |cl, t| {
            self.closure_sync_timeout(cl, t);
        })
    }

    /// wait_event_timeout(), with no limit: trans_wait_event().
    pub fn wait_event(&self, list: &c::closure_waitlist, cond: impl FnMut() -> bool) {
        self.wait_event_timeout(list, cond, c_long::MAX);
    }

    /// wait_event_timeout(), under the caller's borrow of the path table: @cond
    /// mustn't take one.
    pub(in crate::btree) fn wait_event_timeout_in(&self, paths: &mut Paths<'_>, list: &c::closure_waitlist,
                                                  cond: impl FnMut() -> bool, timeout: c_long) -> c_long {
        closure_wait_event_timeout_by(list, cond, timeout, |cl, t| {
            self.closure_sync_timeout_in(paths, cl, t);
        })
    }

    pub(in crate::btree) fn wait_event_in(&self, paths: &mut Paths<'_>, list: &c::closure_waitlist,
                                          cond: impl FnMut() -> bool) {
        self.wait_event_timeout_in(paths, list, cond, c_long::MAX);
    }

    /// @l locked, waited for under SRCU as wait_until() waits. The caller
    /// must have dropped its btree locks.
    pub fn lock_timed<'l, R: RawTimedLock, G, P>(&self, l: &'l Locked<R, G, P>) -> ExclusiveGuard<'l, R, G, P> {
        loop {
            match self.srcu_wait_budget(&mut self.paths()) {
                Some(left) => if let Some(g) = l.try_lock_timeout(left) { return g },
                None       => return l.lock(),
            }
        }
    }

    pub(crate) fn unlock_write(&self, paths: &mut Paths<'_>) {
        for i in self.path_idxs() {
            for l in 0..c::BTREE_MAX_DEPTH as usize {
                if paths[i].write_locked(l) {
                    self.node_unlock_write(paths, i, l);
                }
            }
        }
    }

    /// @l locked, not slept on with the btree locks held: bch2_trans_mutex_lock().
    pub(crate) fn mutex_lock<'l, L: AsLocked<R = c::mutex>, A>(&self, paths: &mut Paths<'_>, l: &'l Placed<L, A>)
        -> Result<ExclusiveGuard<'l, c::mutex, L::G, L::P, Exclusive, A>, BchError>
    {
        if let Some(g) = l.try_lock() {
            return Ok(g);
        }
        let ret_ip = crate::ret_ip!();
        self.unlock_from(paths, ret_ip);
        let g = l.lock();
        self.relock_traced(paths, true, ret_ip)?;
        Ok(g)
    }

    /* Debug */

    pub(crate) fn verify_path_locks(&self, paths: &Paths<'_>, idx: PathIdx) {
        if cfg!(CONFIG_BCACHEFS_DEBUG) && debug_check_btree_locking() {
            self.check_path_locks(paths, idx);
        }
    }

    pub(crate) fn verify_locks(&self, paths: &Paths<'_>) {
        if cfg!(CONFIG_BCACHEFS_DEBUG) && debug_check_btree_locking() {
            self.check_locks(paths);
        }
    }

    fn check_path_locks(&self, paths: &Paths<'_>, idx: PathIdx) {
        let path = &paths[idx];
        if path.nodes_locked == 0 {
            /*
             * A path may be uptodate and yet have nothing locked if and only if
             * there is no node at path->level, which generally means we were
             * iterating over all nodes and got to the end of the btree
             */
            assert!(path.node(path.level() as usize).is_null() ||
                    !(path.should_be_locked() && self.locked() && self.restarted().0 == 0),
                    "path that should be locked has nothing locked, in a locked transaction");
            return;
        }

        for l in 0..c::BTREE_MAX_DEPTH as usize {
            let want = path.lock_want(l);
            let have = path.locked_type_nowrite(l);

            assert!(path.is_btree_node(l) || have == btree_node_locked_type::BTREE_NODE_UNLOCKED,
                    "path holds a lock at level {l}, with no node there");

            assert!(!path.is_btree_node(l) || want == have,
                    "path's lock at level {l} is {}, but it wants {}", have as i32, want as i32);

            assert!(!path.locked(l) || path.lock_seq_matches(l),
                    "path's lock at level {l} has a stale lock seq");
        }
    }

    fn check_locks(&self, paths: &Paths<'_>) {
        if !self.locked() {
            assert!(!self.path_idxs().any(|i| paths[i].nodes_locked != 0), "unlocked transaction holds btree locks");
            return;
        }

        for i in self.path_idxs() {
            self.check_path_locks(paths, i);
        }
    }
}

/*
 * Slow devices legitimately hold the srcu lock across submit_bio() for a long
 * time (e.g. scanning the inodes btree off a slow disk during snapshot
 * deletion), so scale the "held too long" warning past the worst observed
 * device latency rather than spamming the log when the real problem is just
 * slow storage. The lock is held over btree node reads on any online device,
 * so that's the latency we key off.
 */
fn srcu_hold_warn_thresh(fs: &Fs) -> c_ulong {
    let online = c::bch_devs_mask { d: fs.devs_online.load() };
    (fs.dev_latency_max(&online, READ as c_int) * 2).max(HZ * 10)
}

// ── For C, until its callers are Rust ────────────────────────────────────

/// # Safety
/// @b's lock is uninitialized.
pub unsafe fn bch2_btree_lock_init(b: *mut c::btree_bkey_cached_common, flags: c::six_lock_init_flags,
                                   gfp: c::gfp_t) {
    let lock = unsafe { &raw mut (*b).lock };
    unsafe {
        __six_lock_init(lock.cast(), c"b->c.lock".as_ptr().cast(), crate::new_lock!(@key), flags, gfp);
        rust_lockdep_set_notrack_class((&raw mut (*lock.cast::<c::six_lock>()).dep_map).cast());
    }
}

impl<R: RawTimedLock, G, P> Locked<R, G, P> {
    /// lock(), in attempt @t: if it isn't free, the btree locks are dropped
    /// to wait for it - bch2_trans_mutex_lock() - and past a short wait, the
    /// SRCU lock: lock_timed(). An error is drop_locks_do()'s relock, the
    /// lock not held: a restart.
    pub fn lock_trans(&self, t: &TransAttempt<'_, '_>) -> Result<ExclusiveGuard<'_, R, G, P>, BchError> {
        match self.try_lock() {
            Some(g) => Ok(g),
            None    => t.drop_locks_do(|| Ok(t.trans().lock_timed(self))),
        }
    }

    /// lock_trans(), the btree locks left dropped if they were: for a caller
    /// taking more than one, and relocking once after -
    /// bch2_trans_mutex_lock_norelock().
    pub fn lock_trans_norelock(&self, t: &TransAttempt<'_, '_>) -> ExclusiveGuard<'_, R, G, P> {
        self.try_lock().unwrap_or_else(|| {
            t.unlock();
            t.trans().lock_timed(self)
        })
    }
}
