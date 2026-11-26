// SPDX-License-Identifier: GPL-2.0

use super::{btree_id_level_to_text, btree_ptr_hash_val, cached_node, OwnedNode};
use crate::btree::bkey::{bkey_val_to_text, BkeyI, BkeySC, BkeyV};
use crate::btree::iter::{BtreeIterFlags, BtreeTrans};
use crate::btree::locking::BtreeNodeLockedType;
use crate::btree::node::{BtreeNode, BtreeWriteType};
use crate::btree::path::{btree_path_to_text, LockedNode, NodePtr, PathIdx, Paths};
use crate::btree::read::{btree_node_wait_on_read, btree_node_wait_on_write};
use crate::btree::write::{node_write, BtreeWriteFlags};
use crate::data::extents_format::BtreePtrV2;
use crate::debug::trace::events;
use crate::errcode::{Errcode, BchError};
use crate::format::{BkeyType, Btree, BTREE_MAX_DEPTH};
use crate::fs::Fs;
use crate::init::passes::RecoveryPass;
use crate::locking::{six::SixHold, six::SixLockType::{self, SIX_LOCK_read}};
use crate::prt;
use crate::types::L1_CACHE_BYTES;
use crate::os::kernel::{likely, prefetch, race_fault, unlikely};
use crate::util::log::LogMsg;

fn btree_node_mem_ptr<'f>(fs: &'f Fs, k: &BkeyI) -> Option<&'f BtreeNode> {
    let p = BkeySC::from(k).arm::<BtreePtrV2>()?;
    let mem_ptr = p.v().mem_ptr.get() as *const BtreeNode;
    (!mem_ptr.is_null()).then(|| unsafe { cached_node(fs, mem_ptr) })
}

#[cold]
#[inline(never)]
fn btree_bad_header(fs: &Fs, b: LockedNode<'_, BtreeNode>) {
    if fs.recovery.runner.pass_done.get() < RecoveryPass::check_allocations {
        return;
    }

    let contents = b.contents();
    let data = contents.data();
    let mut msg = LogMsg::new(fs);
    prt!(msg, "cached btree node header doesn't match expected (memory corruption?)\nexpected: ");
    btree_id_level_to_text(&mut msg, Btree(b.c.btree_id.get().into()), b.c.level.get().into());
    prt!(msg, "\nptr: ");
    bkey_val_to_text(&mut msg, Some(fs), BkeySC::from(b.key()));

    prt!(msg, "\ngot: ");
    btree_id_level_to_text(&mut msg, data.btree_id(), data.level() as u32);
    prt!(msg, "\nmin {}", data.min_key);
    prt!(msg, "\nmax {}", data.max_key);

    fs.topology_error(&mut msg);
}

#[inline(always)]
fn btree_check_header(fs: &Fs, b: LockedNode<'_, BtreeNode>) {
    let contents = b.contents();
    let data = contents.data();
    let k = BkeySC::from(b.key());

    if Btree(b.c.btree_id.get().into()) != data.btree_id() ||
        b.c.level.get() != data.level() ||
        data.max_key != k.k.p ||
        k.arm::<BtreePtrV2>().is_some_and(|p| data.min_key != p.v().min_key) {
        btree_bad_header(fs, b);
    }
}

#[inline(always)]
fn prefetch_aux(b: LockedNode<'_, BtreeNode>) {
    let contents = b.contents();
    prefetch(contents.bufs.aux_data);

    for t in &contents.set[..contents.nsets as usize] {
        let p = contents.bufs.aux_data.cast::<u64>().wrapping_add(t.aux_data_offset as usize).cast::<u8>();

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
            btree_path_to_text(out, self, idx, &paths[idx]);
        });
        self.restart(Errcode::transaction_restart_lock_node_reused)
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
                               lock_type: SixLockType, flags: BtreeIterFlags)
        -> Result<(&BtreeNode, SixHold<'_>), BchError>
    {
        let fs = self.fs();
        let hash_val = btree_ptr_hash_val(BkeySC::from(k));

        if !self.node_relock(paths, idx, level + 1) {
            return Err(self.btree_node_reused(paths, idx));
        }

        ebug_assert!(level < BTREE_MAX_DEPTH as usize);

        let (b, held) = 'retry: loop {
            let (b, mut held) = match fs.btree_cache_find(k) {
                None => {
                    if unlikely(flags.contains(BtreeIterFlags::NOFILL)) {
                        return Err(fs.err(Errcode::no_btree_node_nofill));
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
                    if paths[idx].locked_type(level + 1) == BtreeNodeLockedType::BTREE_NODE_READ_LOCKED {
                        self.node_unlock(paths, idx, level + 1);
                    }

                    self.locking_hash_val.set(hash_val);
                    self.locking_root_id.set(Btree(u32::MAX));
                    let held = match self.node_lock(paths, idx, &b.c, level, lock_type) {
                        Err(e) if e.matches(Errcode::transaction_restart) => return Err(e),
                        r => r,
                    };

                    if unlikely(held.as_ref().is_err_and(|e| *e == Errcode::no_btree_node_reused.into()) ||
                                b.hash_val() != hash_val ||
                                b.c.level.get() as usize != level ||
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

                btree_node_wait_on_read(self, paths, b);

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

        if paths[idx].locked_type(level + 1) == BtreeNodeLockedType::BTREE_NODE_READ_LOCKED {
            self.node_unlock(paths, idx, level + 1);
        }

        if unlikely(b.flags.read_error()) {
            if flags.contains(BtreeIterFlags::READ_ERRORS) {
                return Ok((b, held));
            }

            drop(held);
            return Err(Errcode::btree_node_read_err_cached.into());
        }

        let locked = LockedNode::of_hold(b, &held);
        prefetch_aux(locked);

        ebug_assert!(Btree(b.c.btree_id.get().into()) == paths[idx].btree_id());
        ebug_assert!(usize::from(locked.contents().data().level()) == level);
        btree_check_header(fs, locked);

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
                                 lock_type: SixLockType, flags: BtreeIterFlags)
        -> Result<(&BtreeNode, SixHold<'_>), BchError>
    {
        ebug_assert!(level < BTREE_MAX_DEPTH as usize);
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

        if paths[idx].locked_type(level + 1) == BtreeNodeLockedType::BTREE_NODE_READ_LOCKED {
            self.node_unlock(paths, idx, level + 1);
        }

        self.locking_hash_val.set(hash_val);
        self.locking_root_id.set(Btree(u32::MAX));
        let held = match self.node_lock(paths, idx, &b.c, level, lock_type) {
            Ok(held) => held,
            Err(e) if e == Errcode::no_btree_node_reused.into() =>
                return self.btree_node_get_slowpath(paths, idx, k, level, lock_type, flags),
            Err(e) => return Err(e),
        };

        let f = b.flags.load();
        if unlikely(b.hash_val() != hash_val ||
                    b.c.level.get() as usize != level ||
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

        let locked = LockedNode::of_hold(b, &held);
        prefetch_aux(locked);

        ebug_assert!(Btree(b.c.btree_id.get().into()) == paths[idx].btree_id());
        ebug_assert!(usize::from(locked.contents().data().level()) == level);
        btree_check_header(self.fs(), locked);

        Ok((b, held))
    }

    pub(crate) fn btree_node_prefetch(&self, paths: &mut Paths<'_>, path: Option<PathIdx>, k: &BkeyI, btree: Btree,
                                      level: usize) -> Result<(), BchError> {
        let fs = self.fs();

        assert!(path.is_none_or(|p| paths[p].locked(level + 1)), "prefetching from an unlocked node");
        assert!(level < BTREE_MAX_DEPTH as usize, "prefetching at level {level}");

        if fs.btree_cache_find(k).is_some() {
            return Ok(());
        }

        // dropped: unlocked
        let _ = self.btree_node_fill(paths, path, k, btree, level, SIX_LOCK_read, false)?;
        Ok(())
    }

    /// The node path @idx has at @level, out of the cache - written first, if
    /// it's dirty - and out of the path: topology repair's, for a node it's
    /// taking out of the btree. Waiting on its IO drops the transaction's
    /// locks: a restart, if they can't be had back.
    pub(crate) fn node_evict(&self, paths: &mut Paths<'_>, idx: PathIdx, level: usize) -> Result<(), BchError> {
        let fs = self.fs();

        self.path_upgrade(paths, idx, level + 1, crate::ret_ip!())?;

        let b = loop {
            let b = paths[idx].cached_btree_node(fs, level).expect("an upgraded path holds its level");
            let f = b.flags.load();

            assert!(!f.permanent(), "evicting a btree root");

            if f.dirty() {
                self.node_lock_write(paths, idx, level)?;
                let held = paths[idx].locked_btree_node(level).expect("a write locked path's node");
                node_write(self.fs(), held, BtreeWriteType::cache_reclaim, BtreeWriteFlags::empty());
                self.node_unlock_write(paths, idx, level);
            } else if !f.read_in_flight() && !f.write_in_flight() {
                break b;
            }

            /* not allowed to wait on io with btree locks held: */
            self.unlock_from(paths, crate::ret_ip!());
            btree_node_wait_on_read(self, paths, b);
            btree_node_wait_on_write(self, paths, b);
            self.relock_from(paths)?;
        };

        self.node_lock_write(paths, idx, level)?;

        let live_u64s = paths[idx].locked_btree_node(level).expect("a level just write locked").contents().nr.live_u64s;
        fs.btree.evicted_size.record(b.hash_val(), live_u64s);
        let (_, held) = paths[idx].take_level(level).expect("a level just write locked");
        {
            let mut lists = fs.btree.cache.lock.lock();
            OwnedNode::take(fs, &mut lists, b, held).free(fs, &mut lists);
        }
        paths[idx].l[level].b = NodePtr::err(Errcode::no_btree_node_drop.into());
        self.node_forget(paths, b);
        Ok(())
    }
}
