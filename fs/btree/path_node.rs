// SPDX-License-Identifier: GPL-2.0

//! A transaction's paths and the nodes they're at: when one of those nodes
//! changes, is replaced or is freed, the paths at it are fixed up to match.

use core::ptr;

use crate::btree::iter::{BtreeTrans, TransAttempt};
use crate::btree::journal_overlay::peek_slot;
use crate::btree::locking::BtreeNodeLockedType::*;
use crate::btree::node::insert::KeyInsert;
use crate::btree::node::BtreeNode;
use crate::btree::path::{KeyVal, LockedNode, NodePtr, PathIdx, Paths, PathsUpdates};
use crate::btree::traverse::level_init_iter;
use crate::c;
use crate::errcode::Errcode;
use crate::format::Btree;
use crate::locking::six::{six_lock_increment_ip, SixHold};
use crate::prt;
use crate::util::Printbuf;

/// The lock seq of path @idx's node at @level, which it holds: the paths at the
/// node are those that have it as of that seq, locked or not.
fn node_lock_seq(paths: &Paths<'_>, idx: PathIdx, level: usize) -> u32 {
    paths[idx].locked_btree_node(level).expect("a node changed, held by its path").c.lock.six().seq.get()
}

impl BtreeTrans<'_> {
    /// The iterators of the paths at path @idx's node at @level, but @fixed's,
    /// fixed for @insert: the node's seq had only if another path has the node.
    pub(crate) fn node_iter_fix(&self, paths: &mut Paths<'_>, idx: PathIdx, level: usize, insert: KeyInsert,
                                fixed: Option<PathIdx>) {
        let node = paths[idx].node(level);
        let mut seq = None;

        for linked in self.path_idxs() {
            if Some(linked) == fixed || paths[linked].node(level) != node {
                continue;
            }

            let seq = *seq.get_or_insert_with(|| node_lock_seq(paths, idx, level));
            if paths[linked].l[level].lock_seq != seq {
                continue;
            }

            let pos = paths[linked].pos;
            let (b, iter) = paths.node_and_iter_mut(idx, level, linked).expect("a node changed, held by its path");
            insert.fix(iter, &b.keys(b.contents()), &pos);
            self.path_verify_level(paths, linked, level);
        }

        if let Some(fixed) = fixed {
            self.path_verify_level(paths, fixed, level);
        }
    }

    /// The iterators of the paths at path @idx's node at @level peeked: past
    /// the deleted keys they're at.
    pub(crate) fn node_iter_peek(&self, paths: &mut Paths<'_>, idx: PathIdx, level: usize) {
        let (node, seq) = (paths[idx].node(level), node_lock_seq(paths, idx, level));

        for linked in self.path_idxs() {
            if paths[linked].node(level) == node && paths[linked].l[level].lock_seq == seq {
                let (b, iter) = paths.node_and_iter_mut(idx, level, linked).expect("a node changed, held by its path");
                iter.peek(&b.keys(b.contents()));
            }
        }
    }

    /// revalidate_updates_in_node(), of the node path @idx holds at @level.
    pub(in crate::btree) fn revalidate_updates_at(&self, t: &TransAttempt<'_, '_>, pu: &mut PathsUpdates<'_, '_>,
                                                  idx: PathIdx, level: usize) {
        let paths = &*pu.paths;
        let b = paths[idx].locked_btree_node(level).expect("a node changed, held by its path");
        self.revalidate_updates_in_node(t, paths, pu.updates, b);
    }

    pub(in crate::btree) fn revalidate_updates_in_node(&self, t: &TransAttempt<'_, '_>, paths: &Paths<'_>,
                                                       updates: &mut [c::btree_insert_entry],
                                                       b: LockedNode<'_, BtreeNode>) {
        let data = b.contents().data();
        let btree = Btree(b.c.btree_id.get().into());

        for e in updates.iter_mut() {
            let pos = e.key().k.p;

            if e.cached() || e.key_cache_flushing() || e.level() != b.c.level.get() || e.btree_id() != btree ||
               pos < data.min_key || pos > data.max_key {
                continue;
            }

            let mut old_k = e.old_k;
            let mut old_v = paths[e.path].peek_slot(&mut old_k).map_or(ptr::null(), |KeyVal(v)| v);

            if let Some(jk) = t.journal_keys() {
                if let Some(j_k) = peek_slot(jk, btree, b.c.level.get().into(), pos) {
                    old_k = j_k.k;
                    old_v = (&raw const j_k.v).cast();
                }
            }

            e.old_k = old_k;
            e.old_v = old_v;
        }
    }

    /*
     * A btree node is being replaced - update the iterator to point to the new
     * node:
     */
    pub(in crate::btree) fn node_add(&self, t: &TransAttempt<'_, '_>, pu: &mut PathsUpdates<'_, '_>,
                                     b: LockedNode<'_, BtreeNode>) {
        let level = b.c.level.get() as usize;
        let paths = &mut pu.paths;

        for idx in self.path_idxs() {
            let path = &paths[idx];
            if path.cached() || level < path.level() as usize || !path.pos_in_node(b) {
                continue;
            }

            let ty = if path.nodes_locked != 0 { path.lock_want(level) } else { BTREE_NODE_UNLOCKED };

            self.node_unlock(paths, idx, level);

            if ty != BTREE_NODE_UNLOCKED {
                let hold = unsafe {
                    six_lock_increment_ip(b.c.lock.six(), ty.six(), 0);
                    SixHold::from_raw(&b.c.lock, ty.six())
                };
                let path = &mut paths[idx];
                path.put_level(level, &b.c, hold);
                path.lock_taken(level);
            }

            assert!(paths[idx].locked_type(level) == ty);
            self.path_level_init(paths, idx, level, b);
        }

        self.revalidate_updates_in_node(t, pu.paths, pu.updates, b);
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
    pub(crate) fn node_forget(&self, paths: &mut Paths<'_>, b: *const BtreeNode) {
        for idx in self.path_idxs() {
            for l in 0..c::BTREE_MAX_DEPTH as usize {
                if paths[idx].l[l].b.is(b.cast()) && !paths[idx].locked(l) {
                    paths[idx].l[l].b = NodePtr::err(Errcode::no_btree_node_drop.into());
                }
            }
        }
    }

    pub(crate) fn node_verify_not_in_iters(&self, paths: &Paths<'_>, level: usize, b: *const BtreeNode) {
        /*
         * Unlocked references were dropped by bch2_trans_node_forget(), locked
         * ones migrated by bch2_trans_node_add(): a path still holding a lock
         * on @b is one bch2_trans_node_add() couldn't migrate.
         */
        for idx in self.path_idxs() {
            let path = &paths[idx];
            if path.l[level].b.is(b.cast()) && path.locked(level) {
                let b = path.locked_btree_node(level).expect("a node a path holds locked");
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
    pub(in crate::btree) fn node_reinit_iter(&self, t: &TransAttempt<'_, '_>, pu: &mut PathsUpdates<'_, '_>,
                                             idx: PathIdx, level: usize) {
        let fs = self.fs();
        let paths = &mut *pu.paths;
        let (node, seq) = (paths[idx].node(level), node_lock_seq(paths, idx, level));

        for linked in self.path_idxs() {
            if paths[linked].node(level) == node && paths[linked].l[level].lock_seq == seq {
                let pos = paths[linked].pos;
                let (b, iter) = paths.node_and_iter_mut(idx, level, linked).expect("a node changed, held by its path");
                level_init_iter(iter, fs, &b.keys(b.contents()), &pos, level);
            }
        }

        self.revalidate_updates_at(t, pu, idx, level);
    }
}
