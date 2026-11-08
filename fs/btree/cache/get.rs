// SPDX-License-Identifier: GPL-2.0

use super::{bch2_btree_id_level_to_text, btree_ptr_hash_val, cached_node};
use crate::btree::bkey::{bkey_val_to_text, BkeyI, BkeySC, BkeyV};
use crate::btree::iter::{BtreeIterFlags, BtreeTrans};
use crate::btree::locking::btree_node_locked_type;
use crate::btree::node::BtreeNode;
use crate::btree::path::{bch2_btree_path_to_text, PathIdx, Paths};
use crate::btree::read::{bch2_btree_node_wait_on_read, bch2_btree_node_wait_on_write};
use crate::btree::types::BtreeWriteType;
use crate::btree::validate::node_btree;
use crate::btree::write::{node_write, BtreeWriteFlags};
use crate::c;
use crate::debug::trace::events;
use crate::errcode::{bch_errcode, BchError};
use crate::format::{BkeyType, Btree};
use crate::fs::Fs;
use crate::init::passes::RecoveryPass;
use crate::locking::six::SixHold;
use crate::prt;
use crate::types::L1_CACHE_BYTES;
use crate::util::kernel::{likely, prefetch, race_fault, unlikely};
use crate::util::log::LogMsg;
use c::btree_node_cache_state as State;
use c::six_lock_type::{self, SIX_LOCK_intent, SIX_LOCK_read};

fn btree_node_mem_ptr<'f>(fs: &'f Fs, k: &BkeyI) -> Option<&'f BtreeNode> {
    let p = BkeySC::from(k).arm::<c::BtreePtrV2>()?;
    let mem_ptr = unsafe { (&raw const p.v().mem_ptr).read_volatile() } as *const BtreeNode;
    (!mem_ptr.is_null()).then(|| unsafe { cached_node(fs, mem_ptr) })
}

#[cold]
#[inline(never)]
fn btree_bad_header(fs: &Fs, b: &BtreeNode) {
    if fs.recovery.runner.pass_done.get() < RecoveryPass::check_allocations {
        return;
    }

    let contents = unsafe { b.contents() };
    let data = contents.data();
    let mut msg = LogMsg::new(fs);
    prt!(msg, "cached btree node header doesn't match expected (memory corruption?)\nexpected: ");
    bch2_btree_id_level_to_text(&mut msg, Btree(b.c.btree_id.into()), b.c.level.into());
    prt!(msg, "\nptr: ");
    bkey_val_to_text(&mut msg, Some(fs), BkeySC::from(contents.key_i()));

    prt!(msg, "\ngot: ");
    bch2_btree_id_level_to_text(&mut msg, Btree(node_btree(data)), data.level() as u32);
    prt!(msg, "\nmin {}", data.min_key);
    prt!(msg, "\nmax {}", data.max_key);

    fs.topology_error(&mut msg);
}

#[inline(always)]
fn btree_check_header(fs: &Fs, b: &BtreeNode) {
    let contents = unsafe { b.contents() };
    let data = contents.data();
    let k = BkeySC::from(contents.key_i());

    if u32::from(b.c.btree_id) != node_btree(data) ||
        u64::from(b.c.level) != data.level() ||
        data.max_key != k.k.p ||
        k.arm::<c::BtreePtrV2>().is_some_and(|p| data.min_key != p.v().min_key) {
        btree_bad_header(fs, b);
    }
}

#[inline(always)]
fn prefetch_aux(b: &BtreeNode) {
    prefetch(b.aux_data);

    let contents = unsafe { b.contents() };
    for t in &contents.set[..contents.nsets as usize] {
        let p = unsafe { b.aux_data.cast::<u64>().add(t.aux_data_offset as usize).cast::<u8>() };

        prefetch(p.wrapping_add(L1_CACHE_BYTES * 0));
        prefetch(p.wrapping_add(L1_CACHE_BYTES * 1));
        prefetch(p.wrapping_add(L1_CACHE_BYTES * 2));
    }
}

impl BtreeTrans<'_> {
    #[cold]
    #[inline(never)]
    fn btree_node_reused(&self, paths: &Paths<'_>, idx: PathIdx) -> BchError {
        self.fs().event_inc_trace(events::trans_restart_btree_node_reused, |out| {
            prt!(out, "{}\n", self.fn_name());
            bch2_btree_path_to_text(out, self, idx, &paths[idx]);
        });
        self.restart(bch_errcode::BCH_ERR_transaction_restart_lock_node_reused)
    }

    #[inline(never)]
    fn btree_node_mem_ptr_set(&self, paths: &mut Paths<'_>, idx: PathIdx, plevel: usize, b: &BtreeNode) {
        let locked = paths[idx].locked(plevel);

        if !self.node_relock(paths, idx, plevel) {
            return;
        }

        {
            let path = &paths[idx];
            let parent = path.locked_btree_node(plevel).expect("a level just relocked");
            let keys = parent.keys(parent.contents());
            let k = keys.key_at(path.l[plevel].iter.peek_all().expect("a path's key, in its parent"));
            ebug_assert!(k.key_type() == BkeyType::btree_ptr_v2);

            // mem_ptr: the value's first field
            let mem_ptr = k.val(&keys.contents().format).as_ptr().cast_mut();
            unsafe { mem_ptr.write_volatile(core::ptr::from_ref(b) as u64) };
        }

        if !locked {
            self.node_unlock(paths, idx, plevel);
        }
    }

    #[inline(never)]
    fn btree_node_get_slowpath(&self, paths: &mut Paths<'_>, idx: PathIdx, k: &BkeyI, level: usize,
                               lock_type: six_lock_type, flags: BtreeIterFlags)
        -> Result<(&BtreeNode, SixHold<'_>), BchError>
    {
        let fs = self.fs();
        let hash_val = btree_ptr_hash_val(BkeySC::from(k));

        if !self.node_relock(paths, idx, level + 1) {
            return Err(self.btree_node_reused(paths, idx));
        }

        ebug_assert!(level < c::BTREE_MAX_DEPTH as usize);

        let (b, held) = 'retry: loop {
            let (b, mut held) = match fs.btree_cache_find(k) {
                None => {
                    if unlikely(flags.contains(BtreeIterFlags::NOFILL)) {
                        return Err(fs.err(bch_errcode::BCH_ERR_no_btree_node_nofill));
                    }

                    /*
                     * We must have the parent locked to call bch2_btree_node_fill(),
                     * else we could read in a btree node from disk that's been
                     * freed:
                     */
                    let btree = paths[idx].btree_id();
                    match self.btree_node_fill(paths, Some(idx), k, btree, level, lock_type, true)? {
                        /* We raced and found the btree node in the cache */
                        None => continue 'retry,
                        Some(got) => got,
                    }
                }
                Some(b) => {
                    if paths[idx].locked_type(level + 1) == btree_node_locked_type::BTREE_NODE_READ_LOCKED {
                        self.node_unlock(paths, idx, level + 1);
                    }

                    self.locking_hash_val.set(hash_val);
                    self.locking_root_id.set(Btree(u32::MAX));
                    let held = match self.node_lock(paths, idx, &b.c, level, lock_type) {
                        Err(e) if e.matches(bch_errcode::BCH_ERR_transaction_restart) => return Err(e),
                        r => r,
                    };

                    if unlikely(held.as_ref().is_err_and(|e| *e == bch_errcode::BCH_ERR_no_btree_node_reused.into()) ||
                                b.hash_val() != hash_val ||
                                b.c.level as usize != level ||
                                race_fault()) {
                        drop(held);
                        if self.node_relock(paths, idx, level + 1) {
                            continue 'retry;
                        }

                        return Err(self.btree_node_reused(paths, idx));
                    }

                    let held = held.expect("btree_node_lock(): a lock, a restart, or the node reused");

                    /* avoid atomic set bit if it's not needed: */
                    if !b.flags.accessed() {
                        b.flags.set_accessed(true);
                    }
                    (b, held)
                }
            };

            if unlikely(b.flags.read_in_flight()) {
                let seq = b.c.lock.six().seq.get();

                drop(held);
                self.unlock_from(paths, crate::ret_ip!());

                bch2_btree_node_wait_on_read(self, paths, b);

                self.relock_from(paths)?;
                self.path_relock(paths, idx)?;

                /*
                 * should_be_locked is not set on this path yet, so we need to
                 * relock it specifically:
                 */
                let Some(relocked) = b.c.lock.relock_as(lock_type, seq) else { continue 'retry };
                held = relocked;
            }

            break (b, held);
        };

        if paths[idx].locked_type(level + 1) == btree_node_locked_type::BTREE_NODE_READ_LOCKED {
            self.node_unlock(paths, idx, level + 1);
        }

        if unlikely(b.flags.read_error()) {
            drop(held);
            return Err(bch_errcode::BCH_ERR_btree_node_read_err_cached.into());
        }

        prefetch_aux(b);

        ebug_assert!(Btree(b.c.btree_id.into()) == paths[idx].btree_id());
        ebug_assert!(unsafe { b.contents() }.data().level() == level as u64);
        btree_check_header(fs, b);

        let down = paths.down_key();
        if unlikely(!btree_node_mem_ptr(fs, down).is_some_and(|p| core::ptr::eq(p, b))) &&
            likely(!self.journal_replay_not_finished() && down.k.key_type() == BkeyType::btree_ptr_v2) {
            self.btree_node_mem_ptr_set(paths, idx, level + 1, b);
        }

        Ok((b, held))
    }

    /**
     * bch2_btree_node_get - find a btree node in the cache and lock it, reading it
     * in from disk if necessary.
     *
     * @trans:	btree transaction object
     * @path:	btree_path being traversed
     * @k:		pointer to btree node (generally KEY_TYPE_btree_ptr_v2)
     * @level:	level of btree node being looked up (0 == leaf node)
     * @lock_type:	SIX_LOCK_read or SIX_LOCK_intent
     * @trace_ip:	ip of caller of btree iterator code (i.e. caller of bch2_btree_iter_peek())
     *
     * The btree node will have either a read or a write lock held, depending on
     * the @write parameter.
     *
     * Returns: btree node or ERR_PTR()
     */
    #[inline(always)]
    pub(crate) fn btree_node_get(&self, paths: &mut Paths<'_>, idx: PathIdx, k: &BkeyI, level: usize,
                                 lock_type: six_lock_type, flags: BtreeIterFlags)
        -> Result<(&BtreeNode, SixHold<'_>), BchError>
    {
        ebug_assert!(level < c::BTREE_MAX_DEPTH as usize);
        ebug_assert!(level + 1 == paths[idx].level() as usize);

        let b = btree_node_mem_ptr(self.fs(), k);
        if let Some(b) = b {
            prefetch(&b.c.lock);
        }

        /*
         * Check b->hash_val _before_ calling btree_node_lock() - this might not
         * be the node we want anymore, and trying to lock the wrong node could
         * cause an unneccessary transaction restart:
         */
        let hash_val = btree_ptr_hash_val(BkeySC::from(k));
        let Some(b) = b.filter(|b| likely(b.hash_val() == hash_val)) else {
            return self.btree_node_get_slowpath(paths, idx, k, level, lock_type, flags);
        };

        if paths[idx].locked_type(level + 1) == btree_node_locked_type::BTREE_NODE_READ_LOCKED {
            self.node_unlock(paths, idx, level + 1);
        }

        self.locking_hash_val.set(hash_val);
        self.locking_root_id.set(Btree(u32::MAX));
        let held = match self.node_lock(paths, idx, &b.c, level, lock_type) {
            Ok(held) => held,
            Err(e) if e == bch_errcode::BCH_ERR_no_btree_node_reused.into() =>
                return self.btree_node_get_slowpath(paths, idx, k, level, lock_type, flags),
            Err(e) => return Err(e),
        };

        let f = b.flags.load();
        if unlikely(b.hash_val() != hash_val ||
                    b.c.level as usize != level ||
                    race_fault() ||
                    f.read_in_flight() ||
                    f.read_error()) {
            drop(held);
            return self.btree_node_get_slowpath(paths, idx, k, level, lock_type, flags);
        }

        /* avoid atomic set bit if it's not needed: */
        if !f.accessed() {
            b.flags.set_accessed(true);
        }

        prefetch_aux(b);

        ebug_assert!(Btree(b.c.btree_id.into()) == paths[idx].btree_id());
        ebug_assert!(unsafe { b.contents() }.data().level() == level as u64);
        btree_check_header(self.fs(), b);

        Ok((b, held))
    }

    pub(crate) fn btree_node_get_noiter(&self, paths: &mut Paths<'_>, k: &BkeyI, btree: Btree, level: usize,
                                        nofill: bool) -> Result<Option<(&BtreeNode, SixHold<'_>)>, BchError> {
        let fs = self.fs();
        let hash_val = btree_ptr_hash_val(BkeySC::from(k));

        ebug_assert!(level < c::BTREE_MAX_DEPTH as usize);

        let ret = 'out: {
            let mut mem_ptr = btree_node_mem_ptr(fs, k);
            let (b, held) = loop {
                let Some(b) = mem_ptr.take().or_else(|| fs.btree_cache_find(k)) else {
                    if nofill {
                        break 'out Ok(None);
                    }

                    match self.btree_node_fill(paths, None, k, btree, level, SIX_LOCK_read, true) {
                        /* We raced and found the btree node in the cache */
                        Ok(None) => continue,
                        Err(_) if self.btree_cache_cannibalize_lock(None).is_ok() => continue,
                        Err(e) => break 'out Err(e),
                        Ok(Some(got)) => break got,
                    }
                };

                self.locking_hash_val.set(hash_val);
                self.locking_root_id.set(Btree(u32::MAX));
                let held = match self.node_lock_nopath_as(&b.c, SIX_LOCK_read, false, crate::ret_ip!(), false) {
                    Err(e) if e.matches(bch_errcode::BCH_ERR_transaction_restart) => break 'out Err(e),
                    Err(e) if e.matches(bch_errcode::BCH_ERR_no_btree_node_reused) => continue,
                    Err(e) => panic!("btree_node_lock_nopath(): {e}"),
                    Ok(held) => held,
                };

                if unlikely(b.hash_val() != hash_val ||
                            u32::from(b.c.btree_id) != btree.0 ||
                            b.c.level as usize != level) {
                    drop(held);
                    continue;
                }

                /* avoid atomic set bit if it's not needed: */
                if !b.flags.accessed() {
                    b.flags.set_accessed(true);
                }
                break (b, held);
            };

            /* XXX: waiting on IO with btree locks held: */
            bch2_btree_node_wait_on_read(self, paths, b);

            prefetch_aux(b);

            if unlikely(b.flags.read_error()) {
                drop(held);
                break 'out Err(bch_errcode::BCH_ERR_btree_node_read_err_cached.into());
            }

            ebug_assert!(u32::from(b.c.btree_id) == btree.0);
            ebug_assert!(unsafe { b.contents() }.data().level() == level as u64);
            btree_check_header(fs, b);
            Ok(Some((b, held)))
        };

        self.btree_cache_cannibalize_unlock();
        ret
    }

    pub(crate) fn btree_node_prefetch(&self, paths: &mut Paths<'_>, path: Option<PathIdx>, k: &BkeyI, btree: Btree,
                                      level: usize) -> Result<(), BchError> {
        let fs = self.fs();

        assert!(path.is_none_or(|p| paths[p].locked(level + 1)), "prefetching from an unlocked node");
        assert!(level < c::BTREE_MAX_DEPTH as usize, "prefetching at level {level}");

        if fs.btree_cache_find(k).is_some() {
            return Ok(());
        }

        // dropped: unlocked
        let _ = self.btree_node_fill(paths, path, k, btree, level, SIX_LOCK_read, false)?;
        Ok(())
    }

    pub(crate) fn btree_node_evict(&self, paths: &mut Paths<'_>, k: &BkeyI) {
        let fs = self.fs();

        let Some(b) = fs.btree_cache_find(k) else { return };

        assert!(!b.flags.permanent(), "evicting a permanent node");
        loop {
            /* not allowed to wait on io with btree locks held: */

            /* XXX we're called from btree_gc which will be holding other btree
             * nodes locked
             */
            bch2_btree_node_wait_on_read(self, paths, b);
            bch2_btree_node_wait_on_write(self, paths, b);

            self.locking_hash_val.set(0);
            self.locking_root_id.set(Btree(u32::MAX));
            let intent = self.node_lock_nopath_as(&b.c, SIX_LOCK_intent, true, crate::ret_ip!(), false)
                .expect("a lock that may not fail, failed");
            let held = self.node_lock_nopath_write(&b.c, intent, crate::ret_ip!(), false);

            if unlikely(b.hash_val() != btree_ptr_hash_val(BkeySC::from(k))) {
                drop(held);
                break;
            }

            if b.flags.dirty() {
                node_write(self, b, BtreeWriteType::cache_reclaim, BtreeWriteFlags::empty());
                drop(held);
                continue;
            }

            assert!(!b.flags.dirty(), "evicting a dirty node");

            fs.btree.evicted_size.record(b.hash_val(), unsafe { b.contents() }.nr.live_u64s);
            let _ = fs.btree_node_transition_state(b, State::FREED);
            drop(held);
            break;
        }
    }
}
