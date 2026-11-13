// SPDX-License-Identifier: GPL-2.0

//! A btree node's key - its pointer, in its parent or as the root - changed
//! in place, the node not rewritten.

use core::ptr;
use crate::alloc::capacity::DiskReservation;
use crate::btree::bkey::{BkeySC, BkeyV, BkeyVMut, BkeyVal};
use crate::btree::iter::{BtreeIter, BtreeIterFlags, BtreeNodeIter, BtreeNodeRef, CommitFlags, CommitOpts,
                         TransAttempt, TransBkey, UpdateTriggerFlags};
use crate::btree::node::BtreeNode;
use crate::btree::path::LockedNode;
use crate::btree::trigger::{key_trigger, TriggerOp};
use crate::c;
use crate::data::reconcile::needs_reconcile::strip_reconcile;
use crate::errcode::{Errcode, BchError};
use crate::format::Btree;
use crate::util::vstructs::Validated;

impl<'a, 't> TransAttempt<'a, 't> {
    /// The pointer to the node at @level of @btree, at @key's position, made
    /// @key, committed.
    pub fn btree_node_update_key_at_pos<V: ?Sized + BkeyVal<State = Validated>>(
        &self, btree: Btree, level: u32, key: TransBkey<'_, 't, V>,
    ) -> Result<(), BchError> {
        self.node_update_key_at_pos(btree, level, key.untyped())
    }

    fn node_update_key_at_pos(&self, btree: Btree, level: u32, key: TransBkey<'_, 't>) -> Result<(), BchError> {
        assert!(key.k().is_btree_ptr(), "a node's key made one that isn't a btree pointer");

        let mut iter = BtreeNodeIter::new(self, btree, key.k().p, 0, level - 1, BtreeIterFlags::empty());
        let b = iter.peek_ref(self)?.expect("the node a btree node pointer points to");
        self.node_update_key(iter.iter(), b, key, CommitFlags::NO_ENOSPC.into(), false)
    }

    /// @node's key made @key - in its parent, or as the root, and in the node:
    /// @node is what @iter is at.
    pub(crate) fn btree_node_update_key<V: ?Sized + BkeyVal<State = Validated>>(
        &self,
        iter:          &mut BtreeIter<'a>,
        node:          BtreeNodeRef,
        key:           TransBkey<'_, 't, V>,
        flags:         impl Into<CommitOpts>,
        skip_triggers: bool,
    ) -> Result<(), BchError> {
        self.node_update_key(iter, node, key.untyped(), flags.into(), skip_triggers)
    }

    fn node_update_key(&self, iter: &mut BtreeIter<'a>, node: BtreeNodeRef, key: TransBkey<'_, 't>,
                       flags: CommitOpts, skip_triggers: bool) -> Result<(), BchError> {
        let trans = self.trans();
        let level = node.level() as usize;

        /*
         * Awkward - we can't rely on caller specifying BTREE_ITER_intent, and
         * the commit will downgrade locks
         */
        trans.path_upgrade(&mut trans.paths(), iter.path, level + 1, crate::ret_ip!())?;

        trans.paths()[iter.path].intent_ref += 1;
        let ret = self.node_update_key_in(iter, node, key, flags, skip_triggers);
        trans.paths()[iter.path].intent_ref -= 1;
        ret
    }

    fn node_update_key_in(&self, iter: &mut BtreeIter<'a>, node: BtreeNodeRef, mut key: TransBkey<'_, 't>,
                          flags: CommitOpts, skip_triggers: bool) -> Result<(), BchError> {
        let trans = self.trans();
        let fs = self.fs();
        let level = node.level() as usize;
        let b = trans.paths().held_btree_node(iter.path, level)
            .filter(|b| ptr::eq(b.node(), node.as_ptr()))
            .expect("updating the key of a node its iterator doesn't hold");
        assert!(!b.is_fake(), "updating a fake node's key");
        assert!(trans.paths()[iter.path].intent_locked(level), "updating the key of a node not intent locked");

        if b.will_make_reachable() {
            return self.new_node_update_key(iter, b, key);
        }

        let btree = Btree(b.c.btree_id.into());
        if !fs.btree_id_root(btree.into()).is_some_and(|r| ptr::eq(r, b.node())) {
            let mut parent = BtreeNodeIter::new(self, btree, b.contents().key.k.p, 0, level as u32 + 1,
                                                BtreeIterFlags::INTENT);
            parent.iter().traverse(self)?;
            let flags = if skip_triggers { UpdateTriggerFlags::NORUN } else { UpdateTriggerFlags::empty() };
            self.update(parent.iter(), &key, flags)?;
        } else {
            /*
             * has_interior_updates has exactly one setter,
             * btree_trans_update_by_path():
             *
             *	trans->has_interior_updates |= path->level != 0;
             *
             * so it is automatic for anything expressed as a
             * bch2_trans_update() - which is how the non-root case
             * above gets it, from the parent update at level + 1.
             * Updating the root is just as much an interior update,
             * but it goes out as journal entries and never touches
             * that path, so it has to say so itself.
             *
             * It matters because we hold b across the commit below
             * and write new_key into b->key afterwards. Without the
             * flag, a leaf that fills up during the commit is split
             * and the commit retried in place rather than restarted
             * (bch2_trans_commit_error()). The trigger below
             * inserts into the same btree b lives in, so it is our
             * own commit that can then grow the root - freeing b
             * with this key change still in flight, and stranding
             * the old key in the interior update's old_nodes[],
             * where trigger_old will later delete a backpointer
             * this commit already deleted.
             */
            trans.set_has_interior_updates(true);

            if !skip_triggers {
                let buf_u64s = key.buf_u64s;
                let mut op = TriggerOp::new(btree, level as u32 + 1, BkeySC::from(b.contents().key_i()), key.as_s(),
                                            UpdateTriggerFlags::INSERT |
                                            UpdateTriggerFlags::OVERWRITE |
                                            UpdateTriggerFlags::TRANSACTIONAL,
                                            buf_u64s);
                key_trigger(self, &mut op)?;
            }

            self.journal_entry_add(c::JsetEntryType::overwrite, b.c.btree_id as u8, b.c.level + 1,
                                   b.contents().key_i().as_bytes())?;
            self.journal_entry_add(c::JsetEntryType::btree_root, b.c.btree_id as u8, b.c.level,
                                   key.as_ref().as_bytes())?;

            /*
             * propagated back to c->btree.roots[].key by
             * bch2_journal_entry_to_btree_root() incorrect for
             */
        }

        let res = DiskReservation::new(fs);
        self.commit(Some(&res), flags)?;

        let new_b = trans.paths().held_btree_node(iter.path, level).expect("a node held across the commit");
        if !ptr::eq(new_b.node(), b.node()) {
            /*
             * We were asked to update the key for a node that was
             * also modified during the commit (due to triggers),
             * and that node was freed:
             */
            assert!(new_b.will_make_reachable(), "a node replaced in its key's commit, by one not yet reachable");
            return Ok(());
        }

        self.node_key_set(iter, b, &key);
        Ok(())
    }

    /// A new node's key: not in its parent yet - the update that will make it
    /// reachable has its key, and commits it.
    fn new_node_update_key(&self, iter: &BtreeIter<'a>, b: LockedNode<'_, BtreeNode>, mut key: TransBkey<'_, 't>)
        -> Result<(), BchError>
    {
        let trans = self.trans();
        let fs = self.fs();

        let mut commit_lock = trans.mutex_lock(&mut trans.paths(), &fs.btree.interior_updates.commit_lock)?;

        let Some(update) = b.will_make_reachable.update() else {
            return Err(trans.restart(Errcode::BCH_ERR_transaction_restart_nested));
        };

        /*
         * Reconcile state on a new node's key is computed, with
         * triggers, by btree_update_nodes_written_trans() - which
         * strips it again if the node is freed before it becomes
         * reachable. The update that frees it runs trigger_old on
         * b->key, so b->key mustn't carry reconcile state that
         * trigger_new never saw:
         */
        strip_reconcile(fs, key.as_s());

        self.node_key_set(iter, b, &key);

        unsafe { update.as_ref() }.new_node_key_set(&mut commit_lock, b.node(), key.as_sc());
        Ok(())
    }

    /// @b's own copy of its key made @key, under its write lock.
    fn node_key_set(&self, iter: &BtreeIter<'a>, b: LockedNode<'_, BtreeNode>, key: &TransBkey<'_, 't>) {
        let trans = self.trans();
        let level = usize::from(b.c.level);
        let mut paths = trans.paths();

        trans.node_lock_write_nofail(&mut paths, iter.path, level);
        paths.write_locked_node(iter.path, level).contents_mut().set_key(key.as_sc());
        trans.node_unlock_write(&mut paths, iter.path, level);
    }
}
