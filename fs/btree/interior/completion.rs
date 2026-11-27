// SPDX-License-Identifier: GPL-2.0

//! An interior update made visible and then completed: its new nodes written,
//! the nodes it frees and the node it updated tracked meanwhile - and once
//! the writes are in, its pointers committed, in the order the updates were
//! made visible, and the node it updated unblocked.

use core::ptr::{self, NonNull};
use core::sync::atomic::Ordering::Relaxed;
use crate::btree::bkey::{BkeyI, BkeySC, BkeyV, BkeyVMut};
use crate::data_structures::{fifo::FifoRead, array_vec::ArrayVec};
use crate::btree::cache::btree_ptr_hash_val;
use crate::btree::trans_attempt::lockrestart_do;
use crate::btree::iter::{BtreeIterFlags, BtreeNodeIter, BtreeNodeRef, CommitFlags, CommitOpts, TransAttempt,
                         TransBkey, UpdateTriggerFlags, Watermark};
use crate::btree::node::{BtreeNode, NR_WRITES, BtreeNodeFlags, BtreeWriteType};
use crate::btree::path::{LockedNode, Paths};
use crate::btree::trigger::{deleted_room, key_trigger, TriggerOp};
use crate::btree::write::{node_write_if_need, node_write_locked, BtreeWriteFlags};
use crate::data::extents_format::{BKEY_BTREE_PTR_U64s_MAX, BtreePtrV2};
use crate::data::reconcile::trigger::SetNeedsReconcileCtx;
use crate::format::{btree_node, Deleted, JsetEntryType};
use crate::journal::pins::JournalPinType;
use crate::sb::members_format::MEMBER_INVALID;
use crate::locking::six::SixLockType::{SIX_LOCK_intent, SIX_LOCK_read};
use crate::data::ptrs::{BkeyVPtrs, ExtentEntry, Ptrs};
use crate::data::reconcile::needs_reconcile::strip_reconcile;
use crate::errcode::{Errcode, BchError};
use crate::fs::Fs;
use crate::fs_fatal_error;
use crate::types::Le64;
use crate::util::vstructs::Head;
use super::{write_blocked, BtreeUpdate, InteriorUpdate, Mode, NewNode, NewPointer, NodeKey, OldNode, Pointee,
            TakenNode, UpdateBox, WriteBlocked, UPDATE_NODES_MAX};

impl InteriorUpdate<'_> {
    /*
     * We're updating @b with pointers to nodes that haven't finished writing yet:
     * block @b from being written until @as completes
     */
    pub(super) fn updated_node(&self, b: LockedNode<'_, BtreeNode>) {
        assert!(self.level_end() >= usize::from(b.c.level.get()), "an interior update's node past its reserve");
        assert!(b.flags.dirty(), "an interior update's node not dirty");
        assert!(b.c.level.get() != 0, "an interior update's node a leaf");

        let slot = b.flags.write_idx();
        let mut g = self.make_visible(Mode::Node(BtreeNodeRef::from(b), slot));
        self.level_end.store(b.c.level.get(), Relaxed);

        b.flags.set_write_blocked(true);
        unsafe { write_blocked(b.node(), slot, &mut g).push_front(self) };
    }

    pub(super) fn updated_root(&self) {
        let _g = self.make_visible(Mode::Root);
    }

    /*
     * Stamp sectors_written on @b's btree_ptr_v2 key. Must be called before the
     * key is consumed (via bch2_keylist_add into parent_keys, or bch2_btree_set_root)
     * so that the key the parent commits and the entry in @as->new_nodes carry the
     * correct sectors_written.
     */
    pub(super) fn emit_new_node_key(&self, n: &mut NewNode<'_>) {
        let fs = self.fs();
        let mut n = n.locked();
        let bytes = size_of::<Head<btree_node>>() + n.contents().data().keys.u64s() * size_of::<u64>();
        let sectors = (bytes.next_multiple_of(fs.block_bytes() as usize) >> 9) as u16;

        n.update_key(|k| if let Some(mut p) = (&mut **k).as_s().arm::<BtreePtrV2>() {
            p.v_mut().sectors_written = crate::types::Le16::new(sectors);
        });
    }

    /*
     * bch2_btree_update_write_new_node:
     *
     * Adds @b to @as's new_nodes list (so we capture root-status after
     * bch2_btree_set_root has run, and so btree_update_nodes_written can
     * iterate it for sb-mark, journal record, and will_make_reachable
     * cleanup), then hands @b off for writing: sets dirty + need_write
     * deferred from bch2_btree_node_alloc (so journal reclaim can't pick @b
     * up for flushing during construction), sets will_make_reachable to
     * gate any subsequent writes besides the first, then submits the first
     * write.
     *
     * Must be called after the parent commit has propagated @b's key (or
     * bch2_btree_set_root has installed @b as the root).  Anything that
     * needs @b->key in its committed-to-disk form (parent_keys / set_root)
     * must instead pair with bch2_btree_update_emit_new_node_key earlier.
     */
    pub(super) fn write_new_node(&self, paths: &mut Paths<'_>, at: TakenNode) {
        let fs = self.fs();
        let mut n = paths[at.path].locked_btree_node_mut(at.level).expect("a new node, held by its path");

        unsafe { self.new_nodes() }.push(NewPointer::node(fs, n.held()));

        self.pending.fetch_add(1, Relaxed);

        {
            let _g = fs.btree.interior_updates.lock.lock();
            n.will_make_reachable.set(self.raw);
            n.flags.set_will_make_reachable(true);
        }

        fs.btree_node_set_dirty(n.node());
        n.flags.set_need_write(true);

        {
            let owned = &mut *self.owned.borrow_mut();
            let taken = owned.taken.iter_mut().find(|t| t.at == at).expect("a new node the update gave a path");
            owned.open_buckets.extend(taken.ob.drain(fs));
        }

        node_write_locked(self.trans(), &mut n, BtreeWriteType::initial, BtreeWriteFlags::empty());
    }

    /*
     * @b is being split/rewritten: it may have pointers to not-yet-written btree
     * nodes and thus outstanding btree_updates - redirect @b's
     * btree_updates to point to this btree_update:
     */
    pub(super) fn will_free_node(&self, b: LockedNode<'_, BtreeNode>) {
        let fs = self.fs();

        b.flags.set_dying(true);

        if b.flags.fake() {
            return;
        }

        /*
         * Does this node have any btree_update operations preventing
         * it from being written?
         *
         * If so, redirect them to point to this btree_update: we can
         * write out our new nodes, but we won't make them visible until those
         * operations complete
         */
        for slot in 0..NR_WRITES {
            WriteBlocked::drain(&mut fs.btree.interior_updates.lock.lock(), |g| write_blocked(b.node(), slot, g), |g, p| {
                p.set_mode(g, Mode::Update);
                fs.journal.pin_copy(&self.journal, &p.journal, JournalPinType::JOURNAL_PIN_TYPE_other);

                /*
                 * for flush_held_btree_writes() waiting on updates to flush or
                 * nodes to be writeable:
                 */
                fs.btree.interior_updates.wait.wake();
            });
        }

        b.flags.set_dirty(false);
        b.flags.set_need_write(false);
        b.flags.set_write_blocked(false);
        b.flags.set_prev_write_blocked(false);

        /*
         * Does this node have unwritten data that has a pin on the journal?
         *
         * If so, transfer that pin to the btree_update operation -
         * note that if we're freeing multiple nodes, we only need to keep the
         * oldest pin of any of the nodes we're freeing. We'll release the pin
         * when the new nodes are persistent and reachable on disk:
         */
        for w in &b.writes {
            fs.journal.pin_copy(&self.journal, w, JournalPinType::JOURNAL_PIN_TYPE_other);
            fs.journal.pin_drop(w);
        }

        self.owned.borrow_mut().old_nodes.push(OldNode::of(b));
    }
}

fn has_reconcile(fs: &Fs, k: BkeySC<'_>) -> bool {
    k.arm::<Ptrs>().is_some_and(|k| k.extent_entries(fs).any(|e| match e {
        ExtentEntry::reconcile(_) => true,
        ExtentEntry::ptr(p)       => u32::from(p.dev()) == MEMBER_INVALID,
        _                         => false,
    }))
}

/// A new node the commit changes the key of, held for intent through the
/// commit by its iterator, and its pointer's index in new_nodes.
type KeyChanged<'t> = ArrayVec<(BtreeNodeIter<'t>, usize), UPDATE_NODES_MAX>;

/*
 * The transactional part of an interior btree node update, where we journal the
 * update we did to the interior node and update alloc info:
 */
fn nodes_written_trans<'t>(t: &TransAttempt<'t, 't>, u: &BtreeUpdate, commit_lock: &mut super::CommitGuard<'_>)
    -> Result<KeyChanged<'t>, BchError>
{
    let fs = t.fs();
    let btree = u.btree;
    let opts = crate::posix::inode_opts::get_fs(fs, true);

    /*
     * Our caller uses BCH_TRANS_COMMIT_no_check_rw, so after emergency
     * read-only nothing else gates this path. Bail explicitly: running the
     * alloc info triggers below for an interior update whose journal half
     * won't land underflows bucket sector counts.
     */
    fs.journal.error()?;

    t.trans().journal_pin.set(ptr::from_ref(&u.journal).cast_mut());

    for i in u.owned.borrow().old_nodes.iter() {
        let level = u32::from(i.level) + 1;
        let old = BkeySC::from(&*i.key);
        let mut deleted = deleted_room(old.k.p);
        let mut op = TriggerOp::new(btree, level, old, TransBkey::from_room(&mut deleted),
                                    UpdateTriggerFlags::OVERWRITE | UpdateTriggerFlags::TRANSACTIONAL);
        key_trigger(t, &mut op)?;

        t.journal_entry_add(JsetEntryType::overwrite, btree.0 as u8, level as u8, &*i.key)?;
    }

    let mut key_changed = KeyChanged::new();
    for (idx, i) in u.new_nodes_locked(commit_lock).iter_mut().enumerate() {
        let level = u32::from(i.level);
        if let Pointee::Node { update_node_key, .. } = &mut i.to {
            *update_node_key = false;
        }
        strip_reconcile(fs, (&mut *i.key).as_s());

        let mut k = t.bkey_reassemble_room(BkeySC::from(&*i.key),
                                           BKEY_BTREE_PTR_U64s_MAX * size_of::<u64>())?;
        k.set_needs_reconcile(t, None, &opts, SetNeedsReconcileCtx::SET_NEEDS_RECONCILE_foreground, 0)?;

        /*
         * This is not strictly the best way of doing this, what we
         * really want is a flag for 'did
         * bch2_bkey_set_needs_reconcile() change anything, and do we
         * need to update the node key'; there's no reason we couldn't
         * be calling bch2_bkey_set_needs_reconcile() at node allocation
         * time to better handle the case where we have to pad with
         * invalid pointers because we don't currently have devices
         * available to meet the desired replication level.
         */

        if has_reconcile(fs, k.as_sc()) {
            let Pointee::Node { b, update_node_key, .. } = &mut i.to else {
                panic!("a deleted pointer with reconcile state")
            };
            match BtreeNodeIter::at_node(t, b.node(fs)) {
                Ok(iter) => {
                    *update_node_key = true;
                    key_changed.push((iter, idx));
                }
                Err(e) if e == BchError::from(Errcode::btree_node_dying) => strip_reconcile(fs, k.as_s()),
                Err(e) => return Err(e),
            }
        }

        let deleted = BkeyI::<Deleted>::at(k.k().p);
        let mut op = TriggerOp::new(btree, level + 1, BkeySC::from(deleted.as_ref()), k.reborrow(),
                                    UpdateTriggerFlags::INSERT | UpdateTriggerFlags::TRANSACTIONAL);
        key_trigger(t, &mut op)?;
        i.key.set_key(k.as_sc());

        let (root, update_node_key) = match i.to {
            Pointee::Node { root, update_node_key, .. } => (root, update_node_key),
            Pointee::Deleted                           => (false, false),
        };
        if !update_node_key || root {
            let (ty, level) = if root {
                (JsetEntryType::btree_root, level)
            } else {
                (JsetEntryType::btree_keys, level + 1)
            };
            t.journal_entry_add(ty, btree.0 as u8, level as u8, k.as_ref())?;
        } else {
            let mut parent = BtreeNodeIter::new(t, btree, k.k().p, 0, level + 1, BtreeIterFlags::INTENT);
            parent.iter().traverse(t)?;
            /*
             * XXX: we shouldn't be logging overwrites here, need a
             * flag for that
             */
            t.update(parent.iter(), k, UpdateTriggerFlags::NORUN)?;
        }
    }

    Ok(key_changed)
}

/// The keys the commit changed given to their nodes, each write locked through
/// the path that's held it since: nofail, as nothing after the commit can
/// restart. Under commit_lock, as btree_node_update_key() changes a new node's
/// key.
fn new_node_keys_set(t: &TransAttempt<'_, '_>, u: &BtreeUpdate, commit_lock: &mut super::CommitGuard<'_>,
                     key_changed: &KeyChanged<'_>) {
    let trans = t.trans();
    let mut paths = trans.paths();
    let new_nodes = u.new_nodes_locked(commit_lock);

    for (iter, i) in key_changed {
        let (idx, level) = (iter.raw.path, usize::from(paths[iter.raw.path].level()));
        trans.node_lock_write_nofail(&mut paths, idx, level);
        paths[idx].locked_btree_node_mut(level).expect("a new node, write locked").set_key(&*new_nodes[*i].key);
        trans.node_unlock_write(&mut paths, idx, level);
    }
}

/* If the node has been reused, we might be reading uninitialized memory - that's fine: */
#[inline(never)]
fn btree_node_seq_matches(b: &BtreeNode, seq: Le64) -> bool {
    /* b->data is not stable while read in flight */
    if b.flags.read_in_flight() {
        return false;
    }

    let data = unsafe { ptr::read_volatile(&raw const (*b.contents.data_ptr()).bufs.data) }.cast::<Head<btree_node>>();
    let b_seq = if data.is_null() { Le64::default() } else { unsafe { ptr::read_volatile(&raw const (*data).keys.seq) } };
    b_seq == seq
}

fn nodes_written(fs: &Fs, owner: UpdateBox) {
    let u = &*owner;
    let mut journal_seq = 0;

    /* before the transaction: commit_lock is taken outside btree locks */
    {
        /* btree_node_update_key() changes a new node's key under commit_lock */
        let keys: ArrayVec<NodeKey, UPDATE_NODES_MAX> = {
            let mut commit_lock = fs.btree.interior_updates.commit_lock.lock();
            u.new_nodes_locked(&mut commit_lock).iter().map(|i| i.key.clone()).collect()
        };

        if !keys.iter().all(|k| fs.btree_bitmap_marked(BkeySC::from(&**k), true)) {
            let mut sb = fs.sb_lock();
            let dirty = keys.iter().fold(false, |d, k| d | fs.btree_bitmap_mark_locked(&mut sb, BkeySC::from(&**k)));
            if dirty {
                fs.write_super(&mut sb);
            }
        }
    }

    let trans = crate::btree_trans!(fs);

    /*
     * Wait for any in flight writes to finish before we free the old nodes
     * on disk. But we haven't pinned those old nodes in the btree cache,
     * they might have already been evicted.
     *
     * The update we're completing deleted references to those nodes from the
     * btree, so we know if they've been evicted they can't be pulled back in.
     * We just have to check if the nodes we have pointers to are still those
     * old nodes, and haven't been reused.
     *
     * This can't be done locklessly because the data buffer might have been
     * vmalloc allocated, and they're not RCU freed. We also need the
     * __no_kmsan_checks annotation because even with the btree node read
     * lock, nothing tells us that the data buffer has been initialized (if
     * the btree node has been reused for a different node, and the data
     * buffer swapped for a new data buffer).
     */
    let old: ArrayVec<(BtreeNodeRef, Le64, u64), UPDATE_NODES_MAX> = u.owned.borrow().old_nodes.iter()
        .map(|i| (i.b, i.seq, btree_ptr_hash_val(BkeySC::from(&*i.key))))
        .collect();
    for (b, seq, hash) in old {
        let b = b.node(fs);

        /*
         * i->b is a stale pointer: this update deleted the node, so
         * btree_node_reclaim may have reused its memory — e.g. as
         * another update's prealloc node, whose owner holds it
         * intent+write while possibly blocked on the allocator. The
         * open buckets the allocator is waiting on are released by
         * this workqueue (bch2_open_bucket_put below), so sleeping on
         * the node here closes the loop and deadlocks. Arm the
         * identity check with the old key's hash: if we'd sleep on a
         * node that's no longer ours, the lock attempt aborts.
         *
         * An abort means no wait is owed. Sustained write locks on an
         * unhashed node come only from reclaim (which requires
         * write_in_flight clear before grabbing) or the write
         * completion path (which runs after the bio finished) — either
         * way the IO this wait guards against is already done.
         */
        let seq_matches = trans.node_locked_do(&b.c, SIX_LOCK_read, hash, |_, _| Ok(btree_node_seq_matches(b, seq)))
            .unwrap_or(false);
        trans.unlock_long();

        if seq_matches {
            b.wait_on_flag(BtreeNodeFlags::write_in_flight_inner);
        }
    }

    /*
     * We did an update to a parent node where the pointers we added pointed
     * to child nodes that weren't written yet: now, the child nodes have
     * been written so we can write out the update to the interior node.
     */

    /*
     * We can't call into journal reclaim here: we'd block on the journal
     * reclaim lock, but we may need to release the open buckets we have
     * pinned in order for other btree updates to make forward progress, and
     * journal reclaim does btree updates when flushing bkey_cached entries,
     * which may require allocations as well.
     */

    trans.unlock();
    let mut new_nodes: ArrayVec<BtreeNodeRef, UPDATE_NODES_MAX> = ArrayVec::new();
    let ret = {
        let mut commit_lock = fs.btree.interior_updates.commit_lock.lock();

        let ret = lockrestart_do(&trans, |t| {
            let key_changed = nodes_written_trans(t, u, &mut commit_lock)?;
            t.commit(Some(&u.disk_res), CommitOpts::new()
                     .watermark(Watermark::interior_updates)
                     .flags(CommitFlags::NO_ENOSPC | CommitFlags::NO_CHECK_RW | CommitFlags::JOURNAL_RECLAIM))?;
            journal_seq = t.journal_res_seq();
            new_node_keys_set(t, u, &mut commit_lock, &key_changed);
            Ok(())
        });
        if let Err(e) = ret {
            if fs.journal.error().is_ok() {
                fs_fatal_error!(fs, "{}", e.msg());
            }
        }

        /*
         * Clear will_make_reachable under commit_lock, to avoid racing with
         * btree_node_update_key():
         */
        for i in u.new_nodes_locked(&mut commit_lock).iter() {
            let Pointee::Node { b: r, .. } = i.to else { continue };
            let b = r.node(fs);

            b.will_make_reachable.clear(NonNull::from(u));
            b.flags.set_will_make_reachable(false);
            new_nodes.push(r);
        }
        ret
    };

    /*
     * Ensure transaction is unlocked before using btree_node_lock_nopath()
     * (the use of which is always suspect, we need to work on removing this
     * in the future)
     *
     * It should be, but bch2_path_get_unlocked_mut() -> bch2_path_get()
     * calls bch2_path_upgrade(), before we call path_make_mut(), so we may
     * rarely end up with a locked path besides the one we have here:
     */
    trans.unlock();

    /*
     * We have to be careful because another thread might be getting ready
     * to free as->b and calling btree_update_reparent() on us - we'll
     * recheck under btree_update_lock below:
     */
    let mode = u.mode(&fs.btree.interior_updates.lock.lock());
    if let Some(Mode::Node(r, slot)) = mode {
        let b = r.node(fs);

        /*
         * @b is the node we did the final insert into:
         *
         * On failure to get a journal reservation, we still have to
         * unblock the write and allow most of the write path to happen
         * so that shutdown works, but the i->journal_seq mechanism
         * won't work to prevent the btree write from being visible (we
         * didn't get a journal sequence number) - instead
         * node_write() doesn't do the actual write if
         * we're in journal error state:
         */
        /*
         * Unarmed take: this block owns clearing b's
         * write_blocked (the list_del below), which is what
         * makes b unreclaimable - so the reuse race can't
         * reach it, and skipping on a spurious abort would
         * leak write_blocked and leave b unwritable forever.
         * The reparent race is handled by the as->b recheck
         * under interior_updates.lock.
         *
         * Except that a reparent clears write_blocked for us, and
         * then b can be freed and reused before we have its lock:
         * its level is read once it's locked.
         */
        let _ = trans.node_locked_do(&b.c, SIX_LOCK_intent, 0, |paths, idx| {
            let level = usize::from(b.c.level.get());
            trans.node_lock_write(paths, idx, level)?;

            let pending = slot != b.flags.write_idx();
            let do_pin = {
                let mut g = fs.btree.interior_updates.lock.lock();
                let blocked = write_blocked(b, slot, &mut g);

                unsafe { blocked.remove(u) };
                if blocked.is_empty() {
                    if pending {
                        b.flags.set_prev_write_blocked(false);
                    } else {
                        b.flags.set_write_blocked(false);
                    }
                }

                /*
                 * Node might have been freed, recheck under
                 * btree_interior_updates.lock; b's write lock
                 * keeps it valid past the mutex drop, so the
                 * journal pin work can happen outside the
                 * mutex.
                 */
                u.mode(&g) == Some(Mode::Node(r, slot))
            };

            if do_pin {
                assert!(b.c.level.get() != 0, "an interior update's node a leaf");
                assert!(b.flags.dirty(), "an interior update's node not dirty");
                assert!(!pending || b.flags.prev_write_pending(), "an interior update blocking a write gone");

                if ret.is_ok() {
                    let mut w = paths[idx].locked_btree_node_mut(level).expect("an interior update's node, write locked");
                    /* the last of its write's bsets */
                    let t = w.contents().set[usize::from(w.contents().nsets) - 1 - usize::from(pending)];
                    let mut keys = w.keys_mut();
                    let last = keys.bset_head_mut(t.data_offset);

                    last.journal_seq = Le64::new(journal_seq.max(last.journal_seq.get()));

                    b.add_journal_pin(fs, usize::from(slot), journal_seq);
                } else {
                    /*
                     * If we didn't get a journal sequence
                     * number we can't write this btree node,
                     * because recovery won't know to ignore
                     * this write:
                     */
                    b.flags.set_never_write(true);
                }
            }

            trans.node_unlock_write(paths, idx, level);

            node_write_if_need(&trans, paths, idx, level);
            Ok(())
        });
    }

    fs.journal.pin_drop(&u.journal);

    for b in new_nodes {
        let b = b.node(fs);
        /*
         * Unarmed take: new nodes are dirty until
         * written, hence unreclaimable - the reuse
         * race can't reach them, and a spurious skip
         * here would leave a node that never gets its
         * write kicked.
         */
        let _ = trans.node_locked_do(&b.c, SIX_LOCK_read, 0, |paths, idx| {
            node_write_if_need(&trans, paths, idx, b.c.level.get().into());
            Ok(())
        });
    }

    u.owned.borrow_mut().open_buckets.clear();

    super::update_free(fs, owner);
}

/// The interior update worker: the updates on unwritten completed in order,
/// each once its new nodes are written - the first not yet stops it. Taken
/// off unwritten to complete: the worker never runs twice at once, so the
/// next isn't taken until this one's done.
pub(super) fn worker(fs: &Fs) {
    loop {
        let owner = {
            let mut g = fs.btree.interior_updates.lock.lock();
            if !g.unwritten.peek_front().is_some_and(|u| u.nodes_written()) {
                break;
            }
            g.unwritten.pop_front().expect("the front, just peeked")
        };

        nodes_written(fs, owner);
    }
}
