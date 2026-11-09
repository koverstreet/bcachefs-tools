// SPDX-License-Identifier: GPL-2.0

use super::NewKey;
use crate::btree::bkey::{bkey_val_to_text, BkeySC};
use crate::btree::iter::{BtreeIter, BtreeIterFlags};
use crate::btree::journal_overlay::peek_max;
use crate::btree::locking::btree_node_locked_type::BTREE_NODE_UNLOCKED;
use crate::btree::path::{NodePtr, PathIdx, Paths};
use crate::btree::trans::BtreeTrans;
use crate::c;
use crate::data_structures::cuckoo::Entry;
use crate::debug::trace::events;
use crate::errcode::{bch_errcode, BchError};
use crate::locking::six::SixHold;
use crate::locking::MemallocFlags;
use crate::prt;
use c::six_lock_type::SIX_LOCK_read;
use core::ffi::c_int;
use core::sync::atomic::Ordering::Relaxed;

impl BtreeTrans<'_> {
    fn btree_path_cached_set(&self, paths: &mut Paths<'_>, idx: PathIdx, ck: &c::bkey_cached, held: SixHold<'_>) {
        paths[idx].put_level(0, &ck.c, held);
        paths[idx].lock_taken(0);
    }

    /// Whether it made one: false if it raced with another fill.
    fn btree_key_cache_create(&self, path: PathIdx, ck_path: PathIdx, k: BkeySC<'_>) -> Result<bool, BchError> {
        let fs = self.fs();
        let bc = &fs.btree.key_cache;
        let (btree, pos) = { let p = &self.paths()[ck_path]; (p.btree_id(), p.pos) };

        /* Slack for varint::decode_fast(), which reads up to 7 bytes past the end: */
        let key_u64s = usize::from(k.k.u64s) + 1;

        /*
         * Allocate some extra space so that the transaction commit path is less
         * likely to have to reallocate, since that requires a transaction
         * restart:
         */
        let key_u64s = (key_u64s * 3 / 2).min(256).next_power_of_two();

        let srcu;
        let mut ck = match self.bkey_cached_alloc(btree, key_u64s)? {
            Some(got) => got,
            None => {
                srcu = fs.btree.trans.barrier.read_lock();
                match bc.bkey_cached_reuse(&srcu) {
                    Some(got) => got,
                    None => {
                        crate::bch_err_ratelimited!(fs, "error allocating memory for key cache item, btree {btree}");
                        return fs.throw(bch_errcode::BCH_ERR_ENOMEM_btree_key_cache_create);
                    }
                }
            }
        };
        /*
         * Either way, ck is returned with intent+write held: bkey_cached_alloc
         * claims them via try_claim for reclaimed entries and trylocks the fresh
         * lock for newly allocated entries; bkey_cached_reuse takes them via
         * bkey_cached_lock_for_evict.
         */

        ck.c.level    = 0;
        ck.c.btree_id = btree.0 as u8;
        ck.key        = c::bkey_cached_key { btree_id: btree.0, pos };
        ck.flags.store(1 << c::BKEY_CACHED_ACCESSED, Relaxed);

        let t = unsafe { self.attempt_in_progress() };
        let ret = (|| {
            if key_u64s > usize::from(ck.u64s) {
                self.paths()[ck_path].mark_locked_noreset(0, BTREE_NODE_UNLOCKED);

                let new_k = match t.allocate_dropping_locks(|| {
                    NewKey::alloc(key_u64s).ok_or(BchError::from(bch_errcode::BCH_ERR_ENOMEM_btree_key_cache_fill))
                }) {
                    Ok(new_k) => new_k,
                    Err(e) if e.matches(c::ENOMEM) => {
                        crate::bch_err_ratelimited!(fs, "error allocating memory for key cache key, btree {} u64s {}",
                                                    btree, key_u64s);
                        return fs.throw(bch_errcode::BCH_ERR_ENOMEM_btree_key_cache_fill);
                    }
                    Err(e) => return Err(e),
                };

                ck.set_key_buf(new_k);
            }

            self.paths()[ck_path].mark_locked_noreset(0, BTREE_NODE_UNLOCKED);
            t.allocate_dropping_locks(|| bc.bkey_cached_keys_add(&mut ck))?;
            t.allocate_dropping_locks(|| {
                bc.table.make_room().map_err(|_| BchError::from(bch_errcode::BCH_ERR_ENOMEM_btree_key_cache_insert))
            })?;

            ck.set_key(k);

            let level = self.paths()[path].level() as usize;
            self.node_lock_write(&mut self.paths(), path, level)?;

            let ret = {
                let _noreclaim = MemallocFlags::noreclaim();
                bc.table.insert_new(unsafe { Entry::new(ck.ck) })
            };

            self.node_unlock_write(&mut self.paths(), path, level);

            match ret {
                Ok(Ok(())) => Ok(true),
                /* raced with another fill? */
                Ok(Err(_)) => Ok(false),
                Err(_)     => Err(BchError::from_raw(-(c::ENOMEM as c_int))),
            }
        })();

        if let Ok(true) = ret {
            bc.nr_keys.fetch_add(1, Relaxed);
            let (ck, held) = ck.into_parts();
            let intent = held.downgrade();

            let lock_want = self.paths()[ck_path].lock_want_six(0);
            let held = if lock_want == SIX_LOCK_read { intent.downgrade() } else { intent };
            self.btree_path_cached_set(&mut self.paths(), ck_path, ck, held);
            return Ok(true);
        }

        bc.keys.remove(ck.keys_idx);
        ck.keys_idx = 0;
        bc.bkey_cached_free(self, ck);
        self.paths()[ck_path].mark_locked_noreset(0, BTREE_NODE_UNLOCKED);

        ret
    }

    /// Whether the path has its entry: false if another fill raced it in.
    #[inline(never)]
    fn btree_key_cache_fill(&self, ck_path: PathIdx, flags: BtreeIterFlags) -> Result<bool, BchError> {
        if flags.contains(BtreeIterFlags::CACHED_NOFILL) {
            self.paths()[ck_path].l[0].b = NodePtr::null();
            return Ok(true);
        }

        let fs = self.fs();
        let t = unsafe { self.attempt_in_progress() };
        let (btree, pos) = { let p = &self.paths()[ck_path]; (p.btree_id(), p.pos) };

        let mut iter = BtreeIter::new(&t, btree, pos,
                                      BtreeIterFlags::INTENT |
                                      BtreeIterFlags::NOFILTER_WHITEOUTS |
                                      BtreeIterFlags::KEY_CACHE_FILL |
                                      BtreeIterFlags::CACHED_NOFILL);
        iter.clear_flags(BtreeIterFlags::WITH_JOURNAL);
        let (iter_ref, mut k) = iter.peek_slot_shared(&t)?;

        let mut needs_immediate_flush = k.k.key_type().is_deleted();

        if self.journal_replay_not_finished() {
            let mut idx = 0;
            if let Some(jk) = peek_max(unsafe { fs.journal_keys_held() }, btree, 0, pos, pos, &mut idx) {
                k = BkeySC::from(jk);
                needs_immediate_flush |= k.k.key_type().is_deleted();
            }
        }

        /* Recheck after btree lookup, before allocating: */
        let created = self.btree_key_cache_find(btree, pos).is_none();
        if created {
            if !self.btree_key_cache_create(iter_ref.path, ck_path, k)? {
                return Ok(false);
            }

            if needs_immediate_flush {
                let paths = self.paths();
                paths[ck_path].key_cached().expect("a key cache entry, just created").set_flag(c::BKEY_CACHED_IMMEDIATE_FLUSH);
            }

            fs.event_inc_trace(events::btree_key_cache_fill, |out| {
                prt!(out, "{}\n", self.fn_name());
                bkey_val_to_text(out, Some(fs), k);
            });
        }

        /* We're not likely to need this iterator again: */
        iter.set_dontneed();
        Ok(created)
    }

    /// Whether it found one.
    #[inline]
    fn btree_path_traverse_cached_fast(&self, paths: &mut Paths<'_>, idx: PathIdx) -> Result<bool, BchError> {
        loop {
            let (btree, pos) = (paths[idx].btree_id(), paths[idx].pos);
            let Some(ck) = self.btree_key_cache_find(btree, pos) else { return Ok(false) };

            ebug_assert!(ck.c.cached);

            let lock_want = paths[idx].lock_want_six(0);

            let held = self.node_lock(paths, idx, &ck.c, 0, lock_want)?;

            if { ck.key } != (c::bkey_cached_key { btree_id: btree.0, pos }) {
                drop(held);
                continue;
            }

            if !ck.accessed() {
                ck.set_flag(c::BKEY_CACHED_ACCESSED);
            }

            self.btree_path_cached_set(paths, idx, ck, held);
            return Ok(true);
        }
    }

    pub(crate) fn btree_path_traverse_cached(&self, paths: &mut Paths<'_>, idx: PathIdx, flags: BtreeIterFlags)
        -> Result<(), BchError>
    {
        ebug_assert!(paths[idx].level() == 0);

        let ret = loop {
            match self.btree_path_traverse_cached_fast(paths, idx) {
                Ok(true)  => break Ok(()),
                Ok(false) => match paths.lend(|| self.btree_key_cache_fill(idx, flags)) {
                    Ok(true)  => break Ok(()),
                    Ok(false) => continue,
                    Err(e)    => break Err(e),
                },
                Err(e)    => break Err(e),
            }
        };

        match ret {
            Err(e) if !e.matches(bch_errcode::BCH_ERR_transaction_restart) => {
                self.node_unlock(paths, idx, 0);
                paths[idx].l[0].b = NodePtr::err(e);
            }
            Ok(()) if !flags.contains(BtreeIterFlags::CACHED_NOFILL) => {
                assert!(paths[idx].locked(0), "a cached path traversed with no lock");
            }
            _ => {}
        }

        ret
    }
}
