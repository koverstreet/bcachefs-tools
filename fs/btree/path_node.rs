// SPDX-License-Identifier: GPL-2.0

//! A transaction's paths and the nodes they're at: when one of those nodes
//! changes, is replaced or is freed, the paths at it are fixed up to match.

use core::ptr;

use crate::btree::iter::BtreeTrans;
use crate::btree::journal_overlay::peek_slot;
use crate::btree::locking::btree_node_locked_type::*;
use crate::btree::node::iter::KeyIter;
use crate::btree::node::BtreeNode;
use crate::btree::path::{KeyVal, LockedNode, NodePtr, PathIdx, Paths};
use crate::c;
use crate::errcode::bch_errcode;
use crate::format::Btree;
use crate::locking::six::{six_lock_increment_ip, SixHold};
use crate::prt;
use crate::util::Printbuf;

impl BtreeTrans<'_> {
    pub(crate) fn path_at_node(&self, paths: &Paths<'_>, idx: PathIdx, b: &BtreeNode) -> bool {
        paths[idx].has_node(b.c.level as usize, &b.c, b.c.lock.six().seq.get())
    }

    pub(crate) fn path_fix_key_modified(&self, paths: &mut Paths<'_>, b: LockedNode<'_, BtreeNode>, where_: u16) {
        let level = b.c.level as usize;
        let keys = b.keys(b.contents());

        for idx in self.path_idxs() {
            if !self.path_at_node(paths, idx, &b) {
                continue;
            }

            let pos = paths[idx].pos;
            paths[idx].l[level].iter.fix_key_modified(&keys, where_, &pos);
            self.path_verify_level(paths, idx, level);
        }
    }

    /// # Safety
    /// @node_iter is path @idx's iterator at @b's level, or one of the
    /// caller's.
    pub(crate) unsafe fn node_iter_fix(&self, paths: &mut Paths<'_>, idx: PathIdx, b: LockedNode<'_, BtreeNode>,
                                       node_iter: *mut KeyIter, where_: u16, clobber_u64s: u16, new_u64s: u16) {
        let level = b.c.level as usize;
        let keys = b.keys(b.contents());
        let t = keys.bkey_to_bset(where_);

        if !ptr::eq(node_iter, &paths[idx].l[level].iter) {
            let pos = paths[idx].pos;
            let iter = unsafe { &mut *node_iter };
            iter.fix(&keys, b.c.level, t, where_, clobber_u64s, new_u64s, &pos);

            if self.verify_iterators() {
                iter.verify(&keys);
            }
        }

        for linked in self.path_idxs() {
            if !self.path_at_node(paths, linked, &b) {
                continue;
            }

            let pos = paths[linked].pos;
            paths[linked].l[level].iter.fix(&keys, b.c.level, t, where_, clobber_u64s, new_u64s, &pos);
            self.path_verify_level(paths, linked, level);
        }
    }

    pub(crate) fn revalidate_updates_in_node(&self, paths: &mut Paths<'_>, b: LockedNode<'_, BtreeNode>) {
        let fs = self.fs();
        let data = b.contents().data();
        let btree = Btree(b.c.btree_id.into());

        for i in 0..self.nr_updates.get() as usize {
            let e = paths.update_mut(i);
            let pos = e.key().k.p;

            if e.cached() || e.key_cache_flushing() || e.level() != b.c.level || e.btree_id() != btree ||
               pos < data.min_key || pos > data.max_key {
                continue;
            }

            let path = e.path;
            let mut old_k = e.old_k;
            let mut old_v = paths[path].peek_slot(&mut old_k).map_or(ptr::null(), |KeyVal(v)| v);

            if self.journal_replay_not_finished() {
                if let Some(j_k) = unsafe { peek_slot(fs, btree, b.c.level.into(), pos) } {
                    old_k = j_k.k;
                    old_v = (&raw const j_k.v).cast();
                }
            }

            let e = paths.update_mut(i);
            e.old_k = old_k;
            e.old_v = old_v;
        }
    }

    /*
     * A btree node is being replaced - update the iterator to point to the new
     * node:
     */
    pub(crate) fn node_add(&self, paths: &mut Paths<'_>, b: LockedNode<'_, BtreeNode>) {
        let level = b.c.level as usize;

        for idx in self.path_idxs() {
            let path = &paths[idx];
            if path.cached() || level < path.level() as usize || !path.pos_in_node(b) {
                continue;
            }

            let t = if path.nodes_locked != 0 { path.lock_want(level) } else { BTREE_NODE_UNLOCKED };

            self.node_unlock(paths, idx, level);

            if t != BTREE_NODE_UNLOCKED {
                let hold = unsafe {
                    six_lock_increment_ip(b.c.lock.six(), t.six(), 0);
                    SixHold::from_raw(&b.c.lock, t.six())
                };
                let path = &mut paths[idx];
                path.put_level(level, &b.c, hold);
                path.lock_taken(level);
            }

            assert!(paths[idx].locked_type(level) == t);
            self.path_level_init(paths, idx, level, b);
        }

        self.revalidate_updates_in_node(paths, b);
    }

    /*
     * @b is being freed, or reused for a different node: paths that have it cached
     * without holding a lock on it must forget it.
     *
     * The saved lock_seq doesn't protect a stale pointer from this transaction:
     * __bch2_btree_node_unlock_write() advances the saved seq of every path here
     * that has @b and a matching seq, locked or not - so a stale path would stay in
     * sync through our own write unlocks, and relock onto whatever @b becomes.
     *
     * Paths holding @b locked are left alone: bch2_trans_node_add() migrates them.
     */
    pub(crate) fn node_forget(&self, paths: &mut Paths<'_>, b: &BtreeNode) {
        for idx in self.path_idxs() {
            for l in 0..c::BTREE_MAX_DEPTH as usize {
                if paths[idx].l[l].b.is(&b.c) && !paths[idx].locked(l) {
                    paths[idx].l[l].b = NodePtr::err(bch_errcode::BCH_ERR_no_btree_node_drop.into());
                }
            }
        }
    }

    pub(crate) fn node_verify_not_in_iters(&self, paths: &Paths<'_>, b: &BtreeNode) {
        let level = b.c.level as usize;

        /*
         * Unlocked references were dropped by bch2_trans_node_forget(), locked
         * ones migrated by bch2_trans_node_add(): a path still holding a lock
         * on @b is one bch2_trans_node_add() couldn't migrate.
         */
        for idx in self.path_idxs() {
            let path = &paths[idx];
            if path.l[level].b.is(&b.c) && path.locked(level) {
                let mut buf = Printbuf::new();
                prt!(&mut buf, "path still references btree node being freed, level {}:\n{}\n",
                     level, b.pos_to_text(self.fs()));
                path.to_text(&mut buf, idx);
                crate::warn_once!(true, self.fs(), "{}", buf.as_str());
            }
        }
    }

    /*
     * A btree node has been modified in such a way as to invalidate iterators - fix
     * them:
     */
    pub(crate) fn node_reinit_iter(&self, paths: &mut Paths<'_>, b: LockedNode<'_, BtreeNode>) {
        let level = b.c.level as usize;

        for idx in self.path_idxs() {
            if self.path_at_node(paths, idx, &b) {
                paths[idx].level_init_iter(self.fs(), level, b);
            }
        }

        self.revalidate_updates_in_node(paths, b);
    }
}
