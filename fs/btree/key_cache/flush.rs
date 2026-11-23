// SPDX-License-Identifier: GPL-2.0

use super::LockedEntry;
use crate::btree::bkey::BkeySC;
use crate::btree::trans_attempt::lockrestart_do;
use crate::btree::iter::{BtreeIter, BtreeIterFlags, CommitFlags, CommitOpts, UpdateTriggerFlags, Watermark};
use crate::btree::path::{LockedNode, NodePtr, PathIdx, Paths};
use crate::btree::trans::BtreeTrans;
use crate::btree::trans_attempt::TransAttempt;
use crate::c;
use crate::data_structures::sort::heapsort_by;
use crate::errcode::{Errcode, BchError};
use crate::format::Btree;
use crate::fs::Fs;
use crate::journal::pins::PinHolder;
use crate::util::alloc::{flags, KVVec};
use crate::util::log::LogMsg;
use crate::util::rcu::read_lock;
use crate::{bch_err_ratelimited, fs_fatal_error, prt};
use c::JournalPinType::JOURNAL_PIN_TYPE_key_cache;
use c::SixLockType::SIX_LOCK_intent;
use core::ffi::c_int;
use core::sync::atomic::Ordering::Relaxed;

/*
 * A dirty cached key holds a journal pin on the oldest entry its updates are
 * in, on a seq not yet retired, and ck->seq - the newest - is no older than
 * that; a clean one holds no pin. btree_key_cache_flush_pos() pins the btree
 * leaf at ck->journal.seq on trust, so a pin that's gone stale shows up there
 * as "journal pin set for seq N outside live range" - far from whatever made
 * it stale.
 */
#[cold]
#[inline(never)]
fn bkey_cached_pin_warn(fs: &Fs, ck: LockedNode<'_, c::bkey_cached>, what: &str) {
    let mut buf = LogMsg::new(fs);
    let key = ck.key.get();
    prt!(buf, "{} {}: dirty {} seq {} pin seq {}, journal last_seq {}",
         Btree(key.btree_id), { key.pos }, ck.dirty() as u32,
         ck.contents().seq, ck.journal.seq(), fs.journal.lock.last_seq.get());
    crate::warn_once!(true, fs, "cached key {what}: {}", buf.as_str());
}

fn bkey_cached_check_pin(fs: &Fs, ck: LockedNode<'_, c::bkey_cached>) {
    let pin_seq = ck.journal.seq();

    if ck.dirty() {
        if pin_seq == 0 {
            bkey_cached_pin_warn(fs, ck, "dirty with no journal pin");
        } else if pin_seq < fs.journal.lock.last_seq.get() {
            bkey_cached_pin_warn(fs, ck, "dirty, pinning a retired journal seq");
        } else if ck.contents().seq < pin_seq {
            bkey_cached_pin_warn(fs, ck, "newest update older than its journal pin");
        }
    } else if pin_seq != 0 {
        bkey_cached_pin_warn(fs, ck, "clean but still holds a journal pin");
    }
}

fn journal_low_on_space(fs: &Fs) -> bool {
    fs.journal.flags.get(c::JournalFlag::low_on_space) ||
        fs.journal.flags.get(c::JournalFlag::low_on_pin)
}

fn btree_key_cache_flush_pos(t: &TransAttempt<'_, '_>, key: c::bkey_cached_key, journal_seq: u64,
                             commit_flags: CommitFlags, evict: bool) -> Result<(), BchError> {
    let trans = t.trans();
    let fs = t.fs();
    let bc = &fs.btree.key_cache;
    let j = &fs.journal;
    let (btree, pos) = (Btree(key.btree_id), { key.pos });

    let mut b_iter = BtreeIter::new(t, btree, pos,
                                    BtreeIterFlags::SLOTS |
                                    BtreeIterFlags::INTENT |
                                    BtreeIterFlags::ALL_SNAPSHOTS);
    let mut c_iter = BtreeIter::new(t, btree, pos,
                                    BtreeIterFlags::CACHED |
                                    BtreeIterFlags::INTENT);
    b_iter.clear_flags(BtreeIterFlags::KEY_CACHED);

    c_iter.traverse(t)?;

    let Some(ck) = trans.paths().held_key_cached(c_iter.path) else {
        return Ok(());
    };

    if ck.dirty() {
        if journal_seq != 0 && ck.journal.seq() != journal_seq {
            return Ok(());
        }

        bkey_cached_check_pin(fs, ck);
        t.set_journal_seq_to_pin(ck.journal.seq());

        /*
         * If we're at the end of the journal, we really want to free up space
         * in the journal right away - we don't want to pin that old journal
         * sequence number with a new btree node write, we want to re-journal
         * the update
         */
        let last_seq = j.lock.last_seq.get();
        let watermark = if ck.journal.seq() == last_seq { Watermark::reclaim } else { Watermark::btree };

        let mut commit_flags = commit_flags;
        if ck.journal.seq() != last_seq || !journal_low_on_space(fs) {
            commit_flags |= CommitFlags::NO_JOURNAL_RES;
        }

        let (b_iter, btree_k) = b_iter.peek_slot_shared(t)?;

        /* Check that we're not violating cache coherency rules: */
        assert!(!btree_k.k.key_type().is_deleted(), "flushing a cached key over a deleted btree key");

        t.update(b_iter, &t.bkey_copy(ck.contents().key())?,
                 UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE |
                 UpdateTriggerFlags::KEY_CACHE_RECLAIM |
                 UpdateTriggerFlags::NORUN)?;
        t.commit(None, CommitOpts::new()
                 .watermark(watermark)
                 .flags(CommitFlags::NO_CHECK_RW |
                        CommitFlags::NO_ENOSPC |
                        CommitFlags::NO_SKIP_NOOPS |
                        commit_flags))?;

        j.pin_drop(&ck.journal);

        assert!(trans.paths()[c_iter.path].locked(0), "a flushed cached path not locked");
    }

    if !evict {
        if ck.dirty() {
            ck.clear_flag(c::BKEY_CACHED_DIRTY);
            bc.nr_dirty.fetch_sub(1, Relaxed);
        }
        bkey_cached_check_pin(fs, ck);
    } else {
        let mut paths = trans.paths();
        let path = c_iter.path;

        for i in trans.path_idxs() {
            if i != path {
                trans.path_unlock(&mut paths, i);
            }
        }

        trans.node_lock_write_nofail(&mut paths, path, 0);

        if ck.dirty() {
            ck.clear_flag(c::BKEY_CACHED_DIRTY);
            bc.nr_dirty.fetch_sub(1, Relaxed);
        }
        bkey_cached_check_pin(fs, ck);

        let mut ck = LockedEntry::new(ck.node(), paths[path].take_level(0).expect("a flushed cached path not locked"));
        if bc.bkey_cached_evict(&mut ck) {
            bc.bkey_cached_free(trans, ck);
        }
    }

    Ok(())
}

impl PinHolder<crate::field!(journal)> for c::bkey_cached {
    fn journal_flush(&self, fs: &Fs, seq: u64) -> Result<(), BchError> {
        let _srcu = fs.btree.trans.barrier.read_lock();

        /*
         * Lockless bailout: if the pin has already been updated past @seq, or
         * the key isn't dirty, there's nothing to do. False negatives (we miss
         * work) are fine — reclaim retries. Avoids taking ck's intent lock on
         * the hot reclaim path when there's nothing to do.
         *
         * The intent lock taken below serializes against
         * btree_key_cache_flush_pos(), which reads ck->journal.seq while the
         * cached path's intent lock is held: a read lock here would let
         * pin_update advance the pin out from under that reader, leaving it
         * with a captured ck->journal.seq < j->last_seq and BUG'ing in
         * bch2_journal_pin_set() once the trans commit fed it back in.
         */
        if self.journal.seq() != seq || !self.dirty() {
            return Ok(());
        }

        let trans = crate::btree_trans!(fs);

        let attempt = |commit_flags: CommitFlags| lockrestart_do(&trans, |t| {
            let mut paths = trans.paths();
            let path_idx = trans.node_lock_with_path(&mut paths, &self.c, SIX_LOCK_intent, 0)?;
            let ck = paths.held_key_cached(path_idx).expect("an entry just locked");
            let key = ck.key.get();
            let ck_seq = ck.contents().seq;

            let do_flush = if ck.journal.seq() != seq || !ck.dirty() {
                /* raced; nothing to do */
                false
            } else if ck_seq != seq {
                fs.journal.pin_update(ck_seq, &ck.journal, JOURNAL_PIN_TYPE_key_cache);
                false
            } else {
                true
            };
            trans.node_unlock(&mut paths, path_idx, 0);
            trans.path_put(&mut paths, path_idx, true);
            drop(paths);

            if do_flush {
                btree_key_cache_flush_pos(t, key, seq, commit_flags, false)
            } else {
                Ok(())
            }
        });

        let mut ret = attempt(CommitFlags::JOURNAL_RECLAIM);
        if ret.as_ref().is_err_and(|e| e.matches(Errcode::journal_reclaim_would_deadlock)) {
            // A fresh journal reservation would block reclaim: retry with
            // none - the key goes into the leaf under the pin it has, which
            // moves to the dirty node
            ret = attempt(CommitFlags::JOURNAL_RECLAIM | CommitFlags::NO_JOURNAL_RES);

            /*
             * no_journal_res only skips taking a journal reservation in the
             * commit; it doesn't clear the watermark guard in
             * btree_update_start() that throws this same error for a btree
             * node split or alloc, so the retry can still legitimately hit it
             * again. Not fatal - the pin stays dirty and reclaim retries it
             * next pass - but logged, so it isn't indistinguishable from the
             * expected first-attempt case.
             */
            if ret.as_ref().is_err_and(|e| e.matches(Errcode::journal_reclaim_would_deadlock)) {
                bch_err_ratelimited!(fs, "key cache flush: the no_journal_res retry also hit journal_reclaim_would_deadlock (btree node reserve below the reclaim watermark)");
            }
        }

        if let Err(e) = &ret {
            if !e.matches(Errcode::journal_reclaim_would_deadlock) && fs.journal.error().is_ok() {
                fs_fatal_error!(fs, "flushing key cache: {}", e.msg());
            }
        }
        ret
    }
}

impl Fs {
    /*
     * Going RO: flush every dirty key cache entry to the btree, dropping its
     * journal pin. The journal-pin-driven flush path
     * (bkey_cached's journal_flush()) has a re-journal branch when we're at
     * j->last_seq and the journal is low on space - it acquires a fresh journal
     * reservation and pins a new sequence. That's what we want during normal
     * operation to free up space, but it defeats the clean_passes loop in
     * __bch2_fs_read_only: every flush spawns a fresh pin and the loop never
     * converges. Walk the cache directly and force no_journal_res so each flush
     * only drops pins, never adds them.
     */
    pub fn btree_key_cache_flush_going_ro(&self) -> Result<bool, BchError> {
        let bc = &self.btree.key_cache;

        if bc.nr_dirty.load(Relaxed) == 0 || self.journal.error().is_err() {
            return Ok(false);
        }

        let srcu = self.btree.trans.barrier.read_lock();
        let trans = crate::btree_trans!(self);
        let mut keys: KVVec<c::bkey_cached_key> = KVVec::new();
        let mut any_done = false;

        /*
         * Preallocate enough up front for the common case (no concurrent
         * dirties); on push failure under rcu we just flush what we have and
         * walk again to find the rest.
         */
        keys.reserve(bc.nr_dirty.load(Relaxed) as usize + 64)
            .map_err(|_| BchError::from_raw(-(c::ENOMEM as c_int)))?;

        loop {
            let mut full = false;
            keys.clear();

            {
                let rcu = read_lock();

                for (_, ck) in bc.keys_from(0, &rcu, &srcu) {
                    if !ck.dirty() {
                        continue;
                    }

                    if keys.push_gfp(ck.key.get(), flags::NOWAIT | flags::NOWARN).is_err() {
                        full = true;
                        break;
                    }
                }
            }

            if keys.is_empty() {
                break;
            }

            // not sort_unstable(): its recursion overflows the kernel stack
            heapsort_by(&mut keys, |l, r| l.btree_id.cmp(&r.btree_id).then({ l.pos }.cmp(&{ r.pos })));

            for k in keys.iter() {
                let ret = lockrestart_do(&trans, |t| {
                    btree_key_cache_flush_pos(t, *k, 0, CommitFlags::NO_JOURNAL_RES, false)
                });
                if let Err(e) = ret {
                    if !e.matches(Errcode::journal_reclaim_would_deadlock) && self.journal.error().is_ok() {
                        return crate::bch_err_fn!(self, Err(e));
                    }
                }
            }
            any_done = true;

            if !full {
                break;
            }
        }

        Ok(any_done)
    }
}

/// @i is the transaction's, its path a cached one traversed to its entry, and
/// write locked.
pub fn bch2_btree_insert_key_cached(t: &TransAttempt<'_, '_>, paths: &mut Paths<'_>, i: &c::btree_insert_entry) {
    let fs = t.fs();
    let bc = &fs.btree.key_cache;
    let mut ck = paths.write_locked_key_cached(i.path);
    let insert = i.key();
    let mut kick_reclaim = false;

    assert!(!insert.k.key_type().is_deleted(), "a deleted key inserted into the key cache");

    ck.contents_mut().set_key(BkeySC::from(insert));

    if !ck.dirty() {
        ebug_assert!(!fs.flags.get(c::FsFlag::clean_shutdown));
        ck.set_flag(c::BKEY_CACHED_DIRTY);
        bc.nr_dirty.fetch_add(1, Relaxed);

        if bc.nr_keys_need_flush() != 0 {
            kick_reclaim = true;
        }
    }

    let journal_res_seq = t.journal_res_seq();

    /*
     * To minimize lock contention, we only add the journal pin here and
     * defer pin updates to the flush callback via ->seq. Be careful not to
     * update ->seq on nojournal commits because we don't want to update the
     * pin to a seq that doesn't include journal updates on disk. Otherwise
     * we risk losing the update after a crash.
     *
     * The only exception is if the pin is not active in the first place. We
     * have to add the pin because journal reclaim drives key cache
     * flushing. The flush callback will not proceed unless ->seq matches
     * the latest pin, so make sure it starts with a consistent value.
     */
    let nojournal = i.flags & UpdateTriggerFlags::NOJOURNAL.bits() != 0;
    if !nojournal || ck.journal.seq() == 0 {
        ck.contents_mut().seq = journal_res_seq;
    }
    /*
     * The seq we pin with is the commit's journal reservation, or for a
     * no_journal_res commit, trans->journal_seq_to_pin - which nothing
     * resets between commits:
     */
    let seq = journal_res_seq;
    let (last_seq, pin_back) = (fs.journal.lock.last_seq.get(), fs.journal.pin_range().pin_back.get());
    crate::warn_once!(seq == 0 || seq < last_seq || seq >= pin_back, fs,
                      "key cache insert pinning journal seq {seq}, live range [{last_seq}, {pin_back}){}",
                      if nojournal { " (nojournal)" } else { "" });

    fs.journal.pin_add(seq, &ck.journal, JOURNAL_PIN_TYPE_key_cache);
    bkey_cached_check_pin(fs, ck.held());

    if kick_reclaim {
        fs.journal.reclaim_kick();
    }
}

/// # Safety
/// @idx is a cached path holding its entry write locked.
pub unsafe fn bch2_btree_key_cache_drop(trans: &BtreeTrans<'_>, paths: &mut Paths<'_>, idx: PathIdx) {
    let fs = trans.fs();
    let bc = &fs.btree.key_cache;
    let ck = paths.held_key_cached(idx).expect("a key cache drop with no entry locked");

    /*
     * We just did an update to the btree, bypassing the key cache: the key
     * cache key is now stale and must be dropped, even if dirty:
     */
    if ck.dirty() {
        ck.clear_flag(c::BKEY_CACHED_DIRTY);
        bc.nr_dirty.fetch_sub(1, Relaxed);
        fs.journal.pin_drop(&ck.journal);
    }
    bkey_cached_check_pin(fs, ck);

    let mut locked = LockedEntry::new(ck.node(), paths[idx].take_level(0).expect("a key cache drop of an entry not locked"));
    bc.bkey_cached_evict(&mut locked);
    bc.bkey_cached_free(trans, locked);

    for i in trans.path_idxs() {
        if paths[i].l[0].b.is(&ck.c) {
            /*
             * It's safe to clear should_be_locked here because
             * we're evicting from the key cache, and we still have
             * the underlying btree locked: filling into the key
             * cache would require taking a write lock on the btree
             * node
             */
            paths[i].bits.set_should_be_locked(false);
            trans.path_unlock(paths, i);
            paths[i].l[0].b = NodePtr::err(Errcode::no_btree_node_drop.into());
        }
    }

    trans.verify_locks(paths);
}
