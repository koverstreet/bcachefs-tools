// SPDX-License-Identifier: GPL-2.0

//! locking.c's, for Rust: taking a lock inside a transaction attempt without
//! blocking on it while holding btree locks.

#![allow(non_camel_case_types)]

use crate::fs::Fs;
use crate::btree::iter::TransAttempt;
use crate::errcode::BchError;
use crate::locking::{ExclusiveGuard, Locked, RawTimedLock};
use crate::c;
use crate::util::Printbuf;
use crate::types::{DArray, c_default};
use core::ffi::{c_int, c_uint};
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

c_extern! {
    pub fn rust_bch2_inode_shard_idx(c: *mut Fs) -> u64;
    pub fn bch2_check_for_deadlock(trans: *mut c::btree_trans, cycle: *mut Printbuf) -> c_int;
}

/* State used for the cycle detector */
/*
 * @trans wants to lock @b with type @type
 */
nest! {
    #[repr(C)]
    #[derive(CStruct)]
    pub struct trans_waiting_for_lock {
        pub trans: *mut c::btree_trans,
        pub node_want: *mut c::btree_bkey_cached_common,
        #[c_bitfield]
        #>[bitfield(u8)]
        pub bits: pub struct trans_waiting_for_lock_bits {
            #[bits(8)] pub lock_want: c::six_lock_type,
        },

        /* for iterating over held locks :*/
        pub level: u8,
        pub path_idx: c::btree_path_idx_t,
        pub waitlist_idx: u16,
        pub node_have: *mut c::btree_bkey_cached_common,

        /*
         * Conflicting waiters we found on the lock at (@path_idx, @level),
         * cached at snapshot time so iteration is stable against concurrent
         * wakeup activity. @waitlist_idx is the next entry to descend into.
         */
        pub waitlist: DArray<*mut c::btree_trans, 16>,
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
