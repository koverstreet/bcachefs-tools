// SPDX-License-Identifier: GPL-2.0

/* Inserting into a given leaf node (last stage of insert): */

use core::ffi::c_int;
use core::mem::{offset_of, size_of};
use core::sync::atomic::Ordering::Relaxed;

use cstruct_macros::c_export;

use super::aux_tree::pack_pos_or_unpacked;
use super::iter::KeyIter;
use super::keys::key_at;
use super::sort::bch2_maybe_compact_whiteouts;
use super::BtreeNode;
use crate::btree::bkey::{BkeyI, BkeyP, BKEY_U64S};
use crate::btree::trans_attempt::lockrestart_do;
use crate::btree::interior::check_topology_msg;
use crate::btree::iter::BtreeTrans;
use crate::btree::path::{BtreePath, LockedNode, PathIdx, Paths};
use crate::btree::types::BtreeWriteType;
use crate::btree::write::{bch2_btree_init_next, bch2_btree_post_write_cleanup, __bch2_btree_node_write,
                          BtreeWriteFlags};
use crate::c;
use crate::cstructs::c as cs;
use crate::errcode::{bch_errcode, ret_to_c};
use crate::format::{BkeyType, Bpos};
use crate::fs::Fs;
use crate::journal::pins::Pin;
use crate::types::Le64;
use crate::util::kernel::unlikely;
use crate::util::log::LogMsg;

/// A whiteout for @pos - packed, if it packs - in @b's unwritten whiteouts:
/// for a deletion of a key a write still has to see deleted.
///
/// # Safety
/// @b is write locked.
unsafe fn push_whiteout(b: &BtreeNode, pos: Bpos) {
    assert!(b.keys_u64s_remaining(unsafe { b.contents() }) >= BKEY_U64S, "no room in a node for a whiteout");
    ebug_assert!(!b.flags.just_written());

    let mut k = pack_pos_or_unpacked(b, pos);
    let (buf, at) = k.as_buf_mut();
    let k = &mut buf[usize::from(at)..];
    let p = BkeyP::from_slice_mut(k).expect("a key just packed");
    p.set_needs_whiteout(true);
    let u64s = p.u64s();

    b.whiteout_u64s.set(b.whiteout_u64s.get() + u64s as u16);
    let at = b.unwritten_whiteouts().start;
    b.keys_mut(unsafe { b.contents_mut() }).buf[at..at + u64s].copy_from_slice(&k[..u64s]);
}

impl BtreeNode {
    pub fn add_journal_pin(&self, fs: &Fs, seq: u64) {
        let flush: unsafe extern "C" fn(_, _, _) -> _ = if self.flags.write_idx() { node_flush1 } else { node_flush0 };

        fs.journal.pin_add(seq, &self.current_write().journal, journal_pin_type_btree(self.c.level), Some(flush));
    }
}

fn journal_pin_type_btree(level: u8) -> c::journal_pin_type {
    c::journal_pin_type::from_bits(c::journal_pin_type::JOURNAL_PIN_TYPE_btree0 as u32 - u32::from(level))
}

/// # Safety
/// @pin is @writes[@idx].journal of a node - which may have been written, and
/// its memory reused, since: see below.
unsafe fn node_flush(j: *mut c::journal, pin: *mut Pin, idx: usize, seq: u64) -> c_int {
    let fs = unsafe { &*j.byte_sub(offset_of!(Fs, journal)).cast::<Fs>() };
    let b = unsafe {
        &*pin.byte_sub(offset_of!(BtreeNode, writes) + idx * size_of::<cs::btree_write>() +
                       offset_of!(cs::btree_write, journal)).cast::<BtreeNode>()
    };
    let w = &b.writes[idx];

    /*
     * The journal pin doesn't pin the node: another thread can write the
     * node and drop the pin while we run, and reclaim can then reuse the
     * node's memory. Snapshot the identity to arm the reuse check: gone
     * already (unhashed nodes can't be dirty), or going while we'd sleep
     * on the lock (aborting the attempt), means the pinned write is done
     * and there's nothing to flush.
     */
    let hash_val = b.hash_val();
    if hash_val == 0 {
        return 0;
    }

    let trans = crate::btree_trans!(fs);
    let ret = lockrestart_do(&trans, |t| {
        let trans = t.trans();
        let path_idx = trans.node_lock_with_path(&mut trans.paths(), &b.c, c::six_lock_type::SIX_LOCK_read,
                                                 hash_val)?;

        let _ = b.flags.fetch_update(Relaxed, Relaxed, |f| {
            (f.dirty() && f.write_idx() as usize == idx && w.journal.seq() == seq).then(|| {
                f.with_write_type(BtreeWriteType::journal_reclaim)
                 .with_need_write(true)
            })
        });

        __bch2_btree_node_write(trans, b, BtreeWriteFlags::only_if_need.bits());

        let mut paths = trans.paths();
        trans.node_unlock(&mut paths, path_idx, b.c.level.into());
        trans.path_put(&mut paths, path_idx, true);
        Ok(())
    });

    match ret {
        Err(e) if e.matches(bch_errcode::BCH_ERR_no_btree_node_reused) => 0,
        r => ret_to_c(r),
    }
}

unsafe extern "C" fn node_flush0(j: *mut c::journal, pin: *mut Pin, seq: u64) -> c_int {
    unsafe { node_flush(j, pin, 0, seq) }
}

unsafe extern "C" fn node_flush1(j: *mut c::journal, pin: *mut Pin, seq: u64) -> c_int {
    unsafe { node_flush(j, pin, 1, seq) }
}

impl BtreeTrans<'_> {
    pub(crate) fn node_prep_for_write(&self, paths: &mut Paths<'_>, b: &BtreeNode) {
        let fs = self.fs();

        if unlikely(b.flags.just_written()) && bch2_btree_post_write_cleanup(fs, b) {
            self.node_reinit_iter(paths, unsafe { LockedNode::new(b) });
        }

        /*
         * If the last bset has been written, or if it's gotten too big - start
         * a new bset to insert into:
         */
        if b.want_new_bset(unsafe { b.contents() }, fs).is_some() {
            bch2_btree_init_next(self, paths, b);
        }
    }

    /// Handle overwrites and do insert, for non extents.
    ///
    /// # Safety
    /// @b is path @idx's node at its level, write locked; @node_iter is the
    /// path's iterator there, or one of the caller's.
    pub(crate) unsafe fn bset_insert_key(&self, paths: &mut Paths<'_>, idx: PathIdx, b: &BtreeNode,
                                         node_iter: *mut KeyIter, insert: &mut BkeyI) -> bool {
        let lb = unsafe { LockedNode::new(b) };
        let deleting = insert.k.key_type().is_deleted();

        let k = {
            let contents = lb.contents();
            let keys = b.keys(contents);

            ebug_assert!(!b.flags.just_written());
            ebug_assert!(!b.bset_written(contents.last_set()));
            ebug_assert!(!(deleting && insert.k.val_u64s() != 0));
            ebug_assert!(insert.k.p >= contents.data().min_key);
            ebug_assert!(insert.k.p <= contents.data().max_key);
            ebug_assert!(usize::from(insert.k.u64s) <= b.keys_u64s_remaining(contents));
            ebug_assert!(b.c.level != 0 || insert.k.p == paths[idx].pos);

            let k = unsafe { &*node_iter }.peek_all()
                .filter(|&k| contents.cmp_left_packed(keys.key_at(k), &insert.k.p).is_eq());

            /* @k is the key being overwritten/deleted, if any: */
            ebug_assert!(!k.is_some_and(|k| keys.key_at(k).key_type().is_deleted()));
            k
        };

        /* Deleting, but not found? nothing to do: */
        if deleting && k.is_none() {
            return false;
        }

        let k_writeable = k.is_some_and(|k| k >= lb.contents().last_set().first_offset());
        let mut clobber_u64s = 0;

        if let Some(k) = k {
            let needs_whiteout = {
                let mut keys = b.keys_mut(unsafe { b.contents_mut() });
                let set = keys.as_keys().bkey_to_bset(k);

                keys.nr.account_key_drop(set, key_at(keys.buf, k));

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
                    unsafe { push_whiteout(b, insert.k.p) };
                } else {
                    insert.k.format_bits.set_needs_whiteout(1);
                }
            }

            if !k_writeable {
                self.path_fix_key_modified(paths, lb, k);
            }
        }

        let (where_, new_u64s) = {
            let mut keys = b.keys_mut(unsafe { b.contents_mut() });

            if deleting {
                let k = k.expect("a deletion with no key to delete returned above");
                if k_writeable {
                    keys.bset_delete(k, clobber_u64s);
                }
                (k, 0)
            } else {
                let where_ = match k {
                    Some(k) if k_writeable => k,
                    _ => unsafe { &*node_iter }.bset_pos(keys.sets.last().expect("a node has a bset")),
                };

                keys.bset_insert(where_, insert, clobber_u64s);
                (where_, key_at(keys.buf, where_).u64s() as u16)
            }
        };

        if clobber_u64s != new_u64s {
            unsafe { self.node_iter_fix(paths, idx, lb, node_iter, where_, clobber_u64s, new_u64s) };
        }

        true
    }

    #[inline(never)]
    fn insert_topology_check(&self, b: &BtreeNode) -> bool {
        let fs = self.fs();
        let mut msg = LogMsg::new(fs);
        msg.set_suppressed(true);

        let bad = check_topology_msg(self, unsafe { LockedNode::new(b) }, &mut msg).is_err();
        if bad {
            crate::prt!(&mut msg, "Btree update created topology error:\n");
            self.updates_to_text(&mut msg);
            fs.emergency_read_only(&mut msg);
        }
        bad
    }

    /// Insert one key into a leaf node: path @idx is at @insert's pos, and
    /// @journal_seq is the sequence number of the journal reservation.
    ///
    /// # Safety
    /// Path @idx's node at its level is write locked.
    pub(crate) unsafe fn insert_key_leaf(&self, paths: &mut Paths<'_>, idx: PathIdx, insert: &mut BkeyI,
                                         journal_seq: u64) {
        let fs = self.fs();
        let level = paths[idx].level() as usize;
        let b = unsafe {
            &*core::ptr::from_ref(paths[idx].locked_btree_node(level).expect("a write locked path has its node").node())
        };
        let node_iter = &raw mut paths[idx].l[level].iter;

        let set_u64s = || unsafe { b.contents() }.last_set().u64s() as isize;
        let live_u64s = || unsafe { b.contents() }.nr.live_u64s as isize;
        let (old_u64s, old_live_u64s) = (set_u64s(), live_u64s());

        if unlikely(!unsafe { self.bset_insert_key(paths, idx, b, node_iter, insert) }) {
            return;
        }

        if unlikely(b.c.level != 0 && self.insert_topology_check(b)) {
            return;
        }

        {
            let contents = unsafe { b.contents_mut() };
            let set = contents.last_set().data_offset;
            let mut keys = b.keys_mut(contents);
            let i = keys.bset_head_mut(set);
            i.journal_seq = Le64::new(journal_seq.max(i.journal_seq.get()));
        }

        b.add_journal_pin(fs, journal_seq);

        if unlikely(!b.flags.dirty()) {
            fs.btree_node_set_dirty(b);
        }

        let live_u64s_added = live_u64s() - old_live_u64s;
        let u64s_added = set_u64s() - old_u64s;

        if live_u64s_added < 0 {
            let contents = unsafe { b.contents() };
            let live = contents.nr.live_u64s as isize;
            for sib in &contents.sib_u64s {
                if sib.get() != u16::MAX {
                    sib.set(live.max(sib.get() as isize + live_u64s_added) as u16);
                }
            }
        }

        if u64s_added > live_u64s_added && unlikely(bch2_maybe_compact_whiteouts(fs, b)) {
            self.node_reinit_iter(paths, unsafe { LockedNode::new(b) });
        }
    }
}

// ── For C: nodes C holds write locked ───────────────────────────────────────

#[c_export]
pub fn bch2_btree_node_prep_for_write(trans: &BtreeTrans<'_>, #[lent(trans)] paths: &mut Paths<'_>, b: &BtreeNode) {
    trans.node_prep_for_write(paths, b)
}

/// # Safety
/// As insert_key_leaf(); @insert is a key.
#[c_export]
pub unsafe fn bch2_btree_insert_key_leaf(trans: &BtreeTrans<'_>, #[lent(trans)] paths: &mut Paths<'_>,
                                         path: *mut BtreePath, insert: *mut c::bkey_i, journal_seq: u64) {
    let insert = unsafe { &mut *<BkeyI>::from_raw_mut(insert) };
    unsafe { trans.insert_key_leaf(paths, trans.path_idx_of(path), insert, journal_seq) }
}
