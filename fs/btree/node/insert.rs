// SPDX-License-Identifier: GPL-2.0

/* Inserting into a given leaf node (last stage of insert): */

use core::sync::atomic::Ordering::Relaxed;


use super::aux_tree::pack_pos_or_unpacked;
use super::iter::KeyIter;
use super::keys::{key_at, Keys};
use super::sort::maybe_compact_whiteouts;
use super::{BtreeNode, NodeContents};
use crate::btree::bkey::{BkeyI, BkeyP, BKEY_U64S};
use crate::btree::trans_attempt::lockrestart_do;
use crate::btree::interior::check_topology_msg;
use crate::btree::iter::{BtreeTrans, TransAttempt};
use crate::btree::path::{LockedNode, PathIdx, PathsUpdates, Write};
use crate::btree::node::BtreeWriteType;
use crate::btree::write::{init_next, node_write, post_write_cleanup, BtreeWriteFlags};
use crate::c;
use crate::errcode::{BchError, Errcode};
use crate::format::{BkeyType, Bpos};
use crate::fs::Fs;
use crate::journal::pins::PinHolder;
use crate::types::Le64;
use crate::os::kernel::unlikely;
use crate::util::log::LogMsg;

/// A whiteout for @pos - packed, if it packs - in @b's unwritten whiteouts:
/// for a deletion of a key a write still has to see deleted.
fn push_whiteout(b: &mut LockedNode<'_, BtreeNode, Write>, pos: Bpos) {
    assert!(b.keys_u64s_remaining(b.contents()) >= BKEY_U64S, "no room in a node for a whiteout");
    ebug_assert!(!b.flags.just_written());

    let mut k = pack_pos_or_unpacked(b.contents(), pos);
    let (buf, at) = k.as_buf_mut();
    let k = &mut buf[usize::from(at)..];
    let p = BkeyP::from_slice_mut(k).expect("a key just packed");
    p.set_needs_whiteout(true);
    let u64s = p.u64s();

    b.whiteout_u64s.set(b.whiteout_u64s.get() + u64s as u16);
    let at = b.unwritten_whiteouts().start;
    b.keys_mut().buf[at..at + u64s].copy_from_slice(&k[..u64s]);
}

/// The key an insert overwrites, marked deleted - if there was one.
#[derive(Clone, Copy)]
struct Overwrite {
    deleting:     bool,
    k:            Option<u16>,
    k_writeable:  bool,
    clobber_u64s: u16,
}

impl Overwrite {
    /// The key overwritten, if it's in a written bset: deleted in place, the
    /// iterators at it to be fixed for it.
    fn modified(&self) -> Option<u16> {
        self.k.filter(|_| !self.k_writeable)
    }
}

/// What an insert changed in a node, for the iterators at it: its last bset,
/// and the key it overwrote if that was in a written bset, deleted in place.
#[derive(Clone, Copy)]
pub(crate) struct KeyInsert {
    pub(crate) where_:       u16,
    pub(crate) clobber_u64s: u16,
    pub(crate) new_u64s:     u16,
    pub(crate) modified:     Option<u16>,
}

impl KeyInsert {
    pub(crate) fn moved(&self) -> bool {
        self.clobber_u64s != self.new_u64s
    }

    pub(crate) fn needs_fix(&self) -> bool {
        self.moved() || self.modified.is_some()
    }

    /// @iter, a path's at @keys' node, which is at @pos, fixed for the insert.
    pub(crate) fn fix(&self, iter: &mut KeyIter, keys: &Keys<'_>, pos: &Bpos) {
        if self.moved() {
            iter.fix(keys, keys.node().c.level.get(), keys.bkey_to_bset(self.where_), self.where_,
                     self.clobber_u64s, self.new_u64s, self.modified, pos);
        } else if let Some(k) = self.modified {
            iter.fix_key_modified(keys, k, pos);
        }
    }
}

/// The key @insert overwrites, at @node_iter's position, marked deleted:
/// None if @insert deletes a key that isn't there.
#[inline(always)]
fn bset_overwrite(b: &mut LockedNode<'_, BtreeNode, Write>, node_iter: &KeyIter, insert: &mut BkeyI)
    -> Option<Overwrite>
{
    let deleting = insert.k.key_type().is_deleted();

    let k = {
        let contents = b.contents();
        let keys = b.keys(contents);

        ebug_assert!(!b.flags.just_written());
        ebug_assert!(!b.last_bset_written(contents));
        ebug_assert!(!(deleting && insert.k.val_u64s() != 0));
        ebug_assert!(insert.k.p >= contents.data().min_key);
        ebug_assert!(insert.k.p <= contents.data().max_key);
        ebug_assert!(usize::from(insert.k.u64s) <= b.keys_u64s_remaining(contents));

        let k = node_iter.peek_all()
            .filter(|&k| contents.cmp_left_packed(keys.key_at(k), &insert.k.p).is_eq());

        /* @k is the key being overwritten/deleted, if any: */
        ebug_assert!(!k.is_some_and(|k| keys.key_at(k).key_type().is_deleted()));
        k
    };

    /* Deleting, but not found? nothing to do: */
    if deleting && k.is_none() {
        return None;
    }

    let k_writeable = k.is_some_and(|k| k >= b.contents().last_set().first_offset());
    let mut clobber_u64s = 0;

    if let Some(k) = k {
        let needs_whiteout = {
            let mut keys = b.keys_mut();
            let set = keys.as_keys().bkey_to_bset(k);

            keys.contents.nr.account_key_drop(set, key_at(keys.buf, k));

            let key = keys.key_at_mut(k);
            key.set_key_type(BkeyType::deleted);
            if k_writeable {
                clobber_u64s = key.u64s() as u16;
            }

            let needs_whiteout = key.needs_whiteout();
            key.set_needs_whiteout(false);
            needs_whiteout
        };

        if needs_whiteout {
            if deleting {
                push_whiteout(b, insert.k.p);
            } else {
                insert.k.format_bits.set_needs_whiteout(1);
            }
        }
    }

    Some(Overwrite { deleting, k, k_writeable, clobber_u64s })
}

/// @insert put in @b's last bset, at @node_iter's position, over @ow.
#[inline(always)]
fn bset_insert(b: &mut LockedNode<'_, BtreeNode, Write>, node_iter: &KeyIter, insert: &mut BkeyI,
               ow: Overwrite) -> KeyInsert {
    let modified = ow.modified();
    let Overwrite { deleting, k, k_writeable, clobber_u64s } = ow;
    let mut keys = b.keys_mut();

    let (where_, new_u64s) = if deleting {
        let k = k.expect("a deletion with no key to delete returned above");
        if k_writeable {
            keys.bset_delete(k, clobber_u64s);
        }
        (k, 0)
    } else {
        let where_ = match k {
            Some(k) if k_writeable => k,
            _ => node_iter.bset_pos(keys.contents.sets().last().expect("a node has a bset")),
        };

        keys.bset_insert(where_, insert, clobber_u64s);
        (where_, key_at(keys.buf, where_).u64s() as u16)
    };

    KeyInsert { where_, clobber_u64s, new_u64s, modified }
}

impl BtreeNode {
    /// @seq, pinned by the write in writes[@slot].
    #[inline]
    pub fn add_journal_pin(&self, fs: &Fs, slot: usize, seq: u64) {
        fs.journal.pin_add(seq, &self.writes[slot], journal_pin_type_btree(self.c.level.get()));
    }
}

fn journal_pin_type_btree(level: u8) -> c::JournalPinType {
    c::JournalPinType::from_bits(c::JournalPinType::JOURNAL_PIN_TYPE_btree0 as u32 - u32::from(level))
}

impl<const I: usize> PinHolder<crate::field!(writes[I])> for BtreeNode {
    fn journal_flush(&self, fs: &Fs, seq: u64) -> Result<(), BchError> {
        /*
         * The journal pin doesn't pin the node: another thread can write the
         * node and drop the pin while we run, and reclaim can then reuse the
         * node's memory. Snapshot the identity to arm the reuse check: gone
         * already (unhashed nodes can't be dirty), or going while we'd sleep
         * on the lock (aborting the attempt), means the pinned write is done
         * and there's nothing to flush.
         */
        let hash_val = self.hash_val();
        if hash_val == 0 {
            return Ok(());
        }

        let trans = crate::btree_trans!(fs);
        let ret = lockrestart_do(&trans, |t| {
            let trans = t.trans();
            let mut paths = trans.paths();
            let path_idx = trans.node_lock_with_path(&mut paths, &self.c, c::SixLockType::SIX_LOCK_read, hash_val)?;

            let _ = self.flags.fetch_update(Relaxed, Relaxed, |f| {
                (f.dirty() && f.write_idx() as usize == I && self.writes[I].seq() == seq).then(|| {
                    f.with_write_type(BtreeWriteType::journal_reclaim)
                     .with_need_write(true)
                     .with_pin_flushed(true)
                })
            });

            let held = paths[path_idx].locked_btree_node(self.c.level.get().into())
                .expect("the node, locked by node_lock_with_path()");
            node_write(fs, held, BtreeWriteType::initial, BtreeWriteFlags::only_if_need);

            trans.node_unlock(&mut paths, path_idx, self.c.level.get().into());
            trans.path_put(&mut paths, path_idx, true);
            Ok(())
        });

        match ret {
            Err(e) if e.matches(Errcode::no_btree_node_reused) => Ok(()),
            r => r,
        }
    }
}

impl BtreeTrans<'_> {
    /// Path @idx's node at @level, which it holds for intent, write locked and
    /// prepped for an insert: node_lock_write() and node_prep_for_write().
    #[inline]
    pub(in crate::btree) fn node_lock_write_prep(&self, t: &TransAttempt<'_, '_>, pu: &mut PathsUpdates<'_, '_>,
                                                 idx: PathIdx, level: usize) -> Result<(), BchError> {
        self.node_lock_write_at(pu.paths, idx, level, false, crate::ret_ip!())?;
        let reinit_iter =
            self.prep_for_write(&mut pu.paths[idx].locked_btree_node_mut(level).expect("a node just write locked"));

        if reinit_iter {
            self.node_reinit_iter(t, pu, idx, level);
        }
        Ok(())
    }

    /// @b, just write locked, prepped for an insert: whether the paths'
    /// iterators at it need reinitializing.
    #[inline(always)]
    fn prep_for_write(&self, b: &mut LockedNode<'_, BtreeNode, Write>) -> bool {
        let fs = self.fs();
        let mut reinit_iter = unlikely(b.flags.just_written()) && post_write_cleanup(fs, b);

        /*
         * If the last bset has been written, or if it's gotten too big - start
         * a new bset to insert into. init_next() works on the node alone, and
         * a reinit on the paths and updates at it alone: one reinit, after
         * both, does for two.
         */
        if b.want_new_bset(b.contents(), fs).is_some() {
            reinit_iter |= init_next(self, b);
        }
        reinit_iter
    }

    /// Handle overwrites and do insert, for non extents - at @node_iter's
    /// position: an iterator of the caller's, fixed for it as at @pos. What
    /// changed, for the iterators of any paths at @b - None, if nothing did.
    pub(crate) fn bset_insert_key(&self, b: &mut LockedNode<'_, BtreeNode, Write>, node_iter: &mut KeyIter,
                                  pos: &Bpos, insert: &mut BkeyI) -> Option<KeyInsert> {
        let level = b.c.level.get();
        ebug_assert!(level != 0 || insert.k.p == *pos);

        let ow = bset_overwrite(b, node_iter, insert)?;
        let ins = bset_insert(b, node_iter, insert, ow);

        if ins.moved() {
            let keys = b.keys(b.contents());
            /* as in C, the caller's iterator isn't fixed for the key modified */
            node_iter.fix(&keys, level, keys.bkey_to_bset(ins.where_), ins.where_, ins.clobber_u64s, ins.new_u64s,
                          None, pos);

            if self.verify_iterators() {
                node_iter.verify(&keys);
            }
        }
        Some(ins)
    }

    #[inline(never)]
    fn insert_topology_check(&self, t: &TransAttempt<'_, '_>, b: LockedNode<'_, BtreeNode>) -> bool {
        let fs = self.fs();
        let mut msg = LogMsg::new(fs);
        msg.set_suppressed(true);

        let bad = check_topology_msg(t, b, &mut msg).is_err();
        if bad {
            crate::prt!(&mut msg, "Btree update created topology error:\n");
            self.updates_to_text(&mut msg);
            fs.emergency_read_only(&mut msg);
        }
        bad
    }

    /// Insert one key into a leaf node: path @idx is at @insert's pos, path
    /// @locked holds the node write locked - @idx, or another path at it - and
    /// @journal_seq is the sequence number of the journal reservation. @under
    /// sees the node after the insert, under its Write view.
    #[inline]
    pub(in crate::btree) fn insert_key_leaf(&self, t: &TransAttempt<'_, '_>, pu: &mut PathsUpdates<'_, '_>,
                                            locked: PathIdx, idx: PathIdx, insert: &mut BkeyI, journal_seq: u64,
                                            under: impl FnOnce(&LockedNode<'_, BtreeNode, Write>)) {
        let paths = &mut pu.paths;
        let level = paths[idx].level() as usize;
        ebug_assert!(level != 0 || insert.k.p == paths[idx].pos);
        ebug_assert!(paths[locked].node(level) == paths[idx].node(level));

        let pos = paths[idx].pos;
        let node_iter = paths[idx].l[level].iter;

        /*
         * With the path's own iterator, if it's the path that write locked the
         * node: that's fixed for the insert under the Write view, the other
         * paths' after.
         */
        let (mut b, own_iter) = if locked == idx {
            let (b, iter) = paths[idx].locked_level_write(level).expect("a node to insert into, write locked");
            (b, Some(iter))
        } else {
            (paths[locked].locked_btree_node_mut(level).expect("a node to insert into, write locked"), None)
        };

        let ins = self.insert_key_leaf_in(t, &mut b, &node_iter, own_iter, &pos, insert, journal_seq);
        under(&b);
        self.insert_key_leaf_fix(t, pu, locked, idx, level, ins);
    }

    /// insert_key_leaf()'s insert into @b, at @node_iter's position, under its
    /// Write view: @own_iter, the iterator of the path that write locked @b if
    /// it's the one at @pos, fixed for it. What changed for the other paths'
    /// iterators, and whether @b was compacted - None if nothing did.
    fn insert_key_leaf_in(&self, t: &TransAttempt<'_, '_>, b: &mut LockedNode<'_, BtreeNode, Write>,
                          node_iter: &KeyIter, own_iter: Option<&mut KeyIter>, pos: &Bpos,
                          insert: &mut BkeyI, journal_seq: u64) -> Option<(KeyInsert, bool)> {
        let fs = self.fs();
        let level = b.c.level.get();

        let set_u64s = |c: &NodeContents| c.last_set().u64s() as isize;
        let live_u64s = |c: &NodeContents| c.nr.live_u64s as isize;
        let (old_u64s, old_live_u64s) = (set_u64s(b.contents()), live_u64s(b.contents()));

        let ow = bset_overwrite(b, node_iter, insert)?;
        let ins = bset_insert(b, node_iter, insert, ow);

        if ins.needs_fix() {
            if let Some(iter) = own_iter {
                ins.fix(iter, &b.keys(b.contents()), pos);
            }
        }

        /*
         * The other paths' iterators are fixed for the insert after, unless the
         * node was compacted, which reinits them all: nothing between moves its
         * keys.
         */
        let compacted = if unlikely(level != 0 && self.insert_topology_check(t, b.held())) {
            false
        } else {
            {
                let set = b.contents().last_set().data_offset;
                let mut keys = b.keys_mut();
                let i = keys.bset_head_mut(set);
                i.journal_seq = Le64::new(journal_seq.max(i.journal_seq.get()));
            }

            b.add_journal_pin(fs, usize::from(b.flags.write_idx()), journal_seq);

            if unlikely(!b.flags.dirty()) {
                fs.btree_node_set_dirty(b.node());
            }

            let live_u64s_added = live_u64s(b.contents()) - old_live_u64s;
            let u64s_added = set_u64s(b.contents()) - old_u64s;

            if live_u64s_added < 0 {
                let contents = b.contents();
                let live = contents.nr.live_u64s as isize;
                for sib in &contents.sib_u64s {
                    if sib.get() != u16::MAX {
                        sib.set(live.max(sib.get() as isize + live_u64s_added) as u16);
                    }
                }
            }

            u64s_added > live_u64s_added && unlikely(maybe_compact_whiteouts(b))
        };
        Some((ins, compacted))
    }

    /// The iterators of the paths at path @idx's node at @level fixed for
    /// insert_key_leaf_in()'s @ins - but @locked's, if it's @idx, which that
    /// fixed.
    #[inline]
    fn insert_key_leaf_fix(&self, t: &TransAttempt<'_, '_>, pu: &mut PathsUpdates<'_, '_>,
                           locked: PathIdx, idx: PathIdx, level: usize, ins: Option<(KeyInsert, bool)>) {
        match ins {
            Some((_, true)) => self.node_reinit_iter(t, pu, idx, level),
            Some((ins, false)) if ins.needs_fix() =>
                self.node_iter_fix(&mut pu.paths, idx, level, ins, (locked == idx).then_some(idx)),
            _ => {}
        }
    }
}

