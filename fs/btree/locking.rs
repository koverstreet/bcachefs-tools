// SPDX-License-Identifier: GPL-2.0

//! locking.c's, for Rust: taking a lock inside a transaction attempt without
//! blocking on it while holding btree locks.

#![allow(non_camel_case_types)]

use crate::fs::Fs;
use crate::btree::iter::{BtreeTrans, TransAttempt};
use crate::btree::path::{BtreePath, PathIdx};
use crate::errcode::BchError;
use crate::locking::{memalloc_flags_restore, memalloc_flags_save, ExclusiveGuard, Locked, RawTimedLock, PF_MEMALLOC_NOIO};
use crate::c;
use crate::util::percpu::{bch2_migrate_disable, bch2_migrate_enable, bch2_raw_smp_processor_id};
#[cfg(CONFIG_BCACHEFS_LOCK_TIME_STATS)]
use crate::util::{kernel::local_clock, time_stats::TimeStats};
use crate::util::Printbuf;
use crate::types::{DArray, c_default};
use core::ffi::{c_int, c_uint, c_ulong};
use cstruct_macros::{bitfield, c_extern, CStruct, c_enum};
use nestify::nest;

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
        self.fs().btree.trans.stats.get(self.fn_idx as usize)
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

c_extern! {
    pub fn rust_bch2_inode_shard_idx(c: *mut Fs) -> u64;
    pub fn bch2_check_for_deadlock(trans: *mut BtreeTrans<'_>, cycle: *mut Printbuf) -> c_int;
}

/* State used for the cycle detector */
/*
 * @trans wants to lock @b with type @type
 */
nest! {
    #[repr(C)]
    #[derive(CStruct)]
    pub struct trans_waiting_for_lock {
        pub trans: *mut BtreeTrans<'static>,
        pub node_want: *mut c::btree_bkey_cached_common,
        #[c_bitfield]
        #>[bitfield(u8)]
        pub bits: pub struct trans_waiting_for_lock_bits {
            #[bits(8)] pub lock_want: c::six_lock_type,
        },

        /* for iterating over held locks :*/
        pub level: u8,
        pub path_idx: PathIdx,
        pub waitlist_idx: u16,
        pub node_have: *mut c::btree_bkey_cached_common,

        /*
         * Conflicting waiters we found on the lock at (@path_idx, @level),
         * cached at snapshot time so iteration is stable against concurrent
         * wakeup activity. @waitlist_idx is the next entry to descend into.
         */
        pub waitlist: DArray<*mut BtreeTrans<'static>, 16>,
    }
}
c_default!(trans_waiting_for_lock);

#[repr(C)]
#[derive(Default, CStruct)]
pub struct lock_graph {
    pub g: [c::trans_waiting_for_lock; 8],
    pub nr: c_uint,
    pub printed_chain: bool,
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
