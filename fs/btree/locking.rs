// SPDX-License-Identifier: GPL-2.0

//! locking.c's, for Rust: taking a lock inside a transaction attempt without
//! blocking on it while holding btree locks.

use crate::btree::iter::TransAttempt;
use crate::errcode::BchError;
use crate::locking::{Guard, Lock, RawTryLock};

impl<R: RawTryLock, G, P> Lock<R, G, P> {
    /// lock(), in attempt @t: if it isn't free, the btree locks are dropped
    /// to wait for it - bch2_trans_mutex_lock(). An error is drop_locks_do()'s
    /// relock, the lock not held: a restart.
    pub fn lock_trans(&self, t: &TransAttempt<'_, '_>) -> Result<Guard<'_, R, G, P>, BchError> {
        match self.try_lock() {
            Some(g) => Ok(g),
            None    => t.drop_locks_do(|| Ok(self.lock())),
        }
    }
}
