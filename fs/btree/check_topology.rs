// SPDX-License-Identifier: GPL-2.0

//! check_topology: every interior node's children checked against each other
//! and against it - gaps, overlaps, a last child that doesn't end where its
//! parent does - and repaired through journal keys, before the filesystem
//! goes read-write.
//!
//! Between steps the walk keeps only positions: each step's nodes are had by
//! node iterators at their keys, so a restart runs the step again.

use core::mem::size_of;
use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{BkeyI, BkeySC, POS_MIN, SPOS_MAX};
use crate::btree::bkey_buf::BkeyBuf;
use crate::btree::interior::{check_topology as node_check_topology, root_alloc_fake_trans};
use crate::btree::iter::{lockrestart_do, BtreeIterFlags, BtreeNodeIter, CommitFlags, TransAttempt};
use crate::btree::journal_overlay::{self, shoot_down_journal_keys, BtreeAndJournalIter};
use crate::btree::node::iter::KeyIter;
use crate::btree::node::{BtreeNode, NodeContents};
use crate::btree::node_scan::{btree_has_scanned_nodes, btree_node_is_stale, get_scanned_nodes};
use crate::btree::path::{DownKey, LockedNode, NodePtr, PathIdx, Paths};
use crate::btree::read::drop_keys_outside_node;
use crate::btree::trans::BtreeTrans;
use crate::c;
use crate::errcode::{bch_errcode, BchError};
use crate::format::{BkeyType, Bpos, Btree};
use crate::fs::{btree_id_can_reconstruct, btree_id_recovers_from_scan, Fs};
use crate::init::error::id;
use crate::init::recovery::btree_lost_data;
use crate::sb::errors::FsckFlags;
use crate::util::log::{ratelimit_reset, LogMsg};
use crate::util::vstructs::{plain_bytes, plain_bytes_mut};
use crate::util::Printbuf;
use crate::{bch_err, bch_err_fn, bch_err_msg, bch_info, btree_trans, fsck_err_flags, mustfix_fsck_err,
            mustfix_fsck_err_on, prt};

type Node<'n> = LockedNode<'n, BtreeNode>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Repair {
    None,
    /// Nodes found by scanning filled a gap: the children are walked again.
    FilledFromScan,
    DropThis,
    DropPrev,
}

/// @contents' key as a btree_ptr_v2 - a v1's converted - with its header and
/// value's head as @f leaves them.
fn btree_ptr_to_v2(contents: &NodeContents, f: impl FnOnce(&mut c::bkey, &mut c::BtreePtrV2)) -> DownKey {
    let src = BkeySC::from(contents.key_i());
    let val = src.val_bytes();
    let mut v = c::BtreePtrV2::default();

    let ptrs = match src.k.key_type() {
        BkeyType::btree_ptr => {
            v.seq     = contents.data().keys.seq;
            v.min_key = contents.data().min_key;
            val
        }
        BkeyType::btree_ptr_v2 => {
            let (head, ptrs) = val.split_at(size_of::<c::BtreePtrV2>());
            plain_bytes_mut(&mut v).copy_from_slice(head);
            ptrs
        }
        _ => unreachable!("a btree node's key is a btree pointer"),
    };

    let mut k = c::bkey::new();
    k.set_key_type(BkeyType::btree_ptr_v2);
    k.p = src.k.p;
    k.set_val_bytes(size_of::<c::BtreePtrV2>() + ptrs.len());

    f(&mut k, &mut v);

    let mut dst = DownKey::default();
    let (hdr, val) = plain_bytes_mut(dst.u64s_mut()).split_at_mut(size_of::<c::bkey>());
    let (head, rest) = val.split_at_mut(size_of::<c::BtreePtrV2>());
    hdr.copy_from_slice(plain_bytes(&k));
    head.copy_from_slice(plain_bytes(&v));
    rest[..ptrs.len()].copy_from_slice(ptrs);
    dst
}

fn set_node_min(trans: &BtreeTrans<'_>, paths: &mut Paths<'_>, idx: PathIdx, level: usize,
                new_min: Bpos) -> Result<(), BchError> {
    let fs = trans.fs();
    let b = paths.held_btree_node(idx, level).expect("a node to repair, held by its path");

    if fs.opts.verbose.get() != 0 {
        bch_info!(fs, "set_node_min(): {} -> {new_min}", BkeySC::from(b.contents().key_i()).to_text(fs));
    }

    let new = btree_ptr_to_v2(b.contents(), |_, v| {
        v.min_key = new_min;
        v.set_btree_ptr_range_updated(true.into());
    });

    trans.node_lock_write(paths, idx, level)?;
    let mut b = paths.write_locked_node(idx, level);
    b.contents_mut().data_mut().min_key = new_min;

    let ret = journal_overlay::insert(&mut fs.not_rw(), Btree(b.c.btree_id.into()), level as u32 + 1, new.key());
    if ret.is_ok() {
        drop_keys_outside_node(b.node(), b.contents_mut());
        b.contents_mut().key.set_key(new.key());
    }

    trans.node_unlock_write(paths, idx, level);
    ret
}

fn set_node_max(trans: &BtreeTrans<'_>, paths: &mut Paths<'_>, idx: PathIdx, level: usize,
                new_max: Bpos) -> Result<(), BchError> {
    let fs = trans.fs();
    let b = paths.held_btree_node(idx, level).expect("a node to repair, held by its path");
    let btree = Btree(b.c.btree_id.into());

    if fs.opts.verbose.get() != 0 {
        bch_info!(fs, "set_node_max(): {} -> {new_max}", BkeySC::from(b.contents().key_i()).to_text(fs));
    }

    let new = btree_ptr_to_v2(b.contents(), |k, v| {
        k.p = new_max;
        v.set_btree_ptr_range_updated(true.into());
    });

    trans.node_lock_write(paths, idx, level)?;
    let mut b = paths.write_locked_node(idx, level);

    let mut not_rw = fs.not_rw();
    let ret = journal_overlay::delete(&mut not_rw, btree, level as u32 + 1, b.contents().key_i().k.p).and_then(|()| {
        b.contents_mut().data_mut().max_key = new_max;
        journal_overlay::insert(&mut not_rw, btree, level as u32 + 1, new.key())
    });
    if ret.is_ok() {
        drop_keys_outside_node(b.node(), b.contents_mut());

        let bc = &fs.btree.cache;

        /*
         * max_key is b->key.k.p, which is the cache's hash key - so unlike
         * set_node_min() this has to leave the hash table and come back.
         * Roots can't be unhashed while flagged.
         */
        let permanent = b.flags.load().permanent();
        if permanent {
            let _lists = bc.lock.lock();
            b.flags.set_permanent(false);
        }

        /* unhash, rehash */
        fs.btree_node_transition_state(b.node(), c::btree_node_cache_state::FREEABLE)
            .expect("a node leaving the hash table, write locked");
        b.contents_mut().key.set_key(new.key());
        fs.btree_node_transition_state(b.node(), b.live_state()).expect("a node back into the hash table, write locked");

        /*
         * Still the root - but it was on bc->freeable with roots_b[] pointing at
         * it, which only single-threaded recovery makes safe.
         */
        if permanent {
            let _lists = bc.lock.lock();
            b.flags.set_permanent(true);
        }
    }

    trans.node_unlock_write(paths, idx, level);
    ret
}

/*
 * Commit the fsck "repaired" log entry queued by mustfix_fsck_err().
 *
 * Topology repair runs before we go rw and applies its fixes through journal
 * keys (not transactional updates), so nothing else here commits - but the log
 * is a transaction update, and the next bch2_trans_begin() would silently drop
 * it. Commit it here, at the check, *before* the mutation: a commit can return
 * a transaction restart (fault injection, journal, ...), and doing it first
 * means the restart re-runs only the check - which re-queues the log - never
 * the non-transactional mutation, which must run exactly once.
 */
fn commit_topology_repair_log(t: &TransAttempt<'_, '_>) -> Result<(), BchError> {
    if t.has_updates() {
        t.commit(None, CommitFlags::NO_ENOSPC)?;
    }
    Ok(())
}

struct Held<'n> {
    idx: PathIdx,
    b:   Node<'n>,
}

fn held<'n>(paths: &Paths<'n>, iter: &BtreeNodeIter<'_>) -> Held<'n> {
    let idx = iter.raw.path;
    let b = paths.held_btree_node(idx, paths[idx].level() as usize).expect("a node, peeked");
    Held { idx, b }
}

/// No path table held across an fsck error: its message is allocated in the
/// transaction, which may borrow the table to drop locks. Each repair borrows
/// it after.
fn check_node_boundaries(t: &TransAttempt<'_, '_>, b: Node<'_>,
                         prev: Option<Held<'_>>, cur: Held<'_>) -> Result<Repair, BchError> {
    let fs = t.fs();
    let trans = t.trans();
    let btree = Btree(b.c.btree_id.into());
    let level = usize::from(b.c.level) - 1;
    let b_data = b.contents().data();
    let cur_min = cur.b.contents().data().min_key;
    let expected_start = match &prev {
        None    => b_data.min_key,
        Some(p) => p.b.contents().key_i().k.p.successor(),
    };
    let mut buf = Printbuf::new();

    assert!(BkeySC::from(b.contents().key_i()).arm::<c::BtreePtrV2>()
            .is_none_or(|p| p.v().min_key == b_data.min_key),
            "a btree_ptr_v2's min_key not its node's");

    if expected_start == cur_min {
        return Ok(Repair::None);
    }

    prt!(&mut buf, " at btree={btree} level={}:\nparent: {}", b.c.level, BkeySC::from(b.contents().key_i()).to_text(fs));

    if let Some(p) = &prev {
        prt!(&mut buf, "\nprev: {}", BkeySC::from(p.b.contents().key_i()).to_text(fs));
    }

    prt!(&mut buf, "\nnext: {}\n", BkeySC::from(cur.b.contents().key_i()).to_text(fs));

    if expected_start < cur_min {								/* gap */
        let mut nodes_found = 0;

        if b.c.level == 1 && btree_id_recovers_from_scan(btree) {
            get_scanned_nodes(fs, btree, 0, expected_start, cur_min.predecessor(), &mut buf, &mut nodes_found)?;
            if nodes_found == 0 {
                prt!(&mut buf, "btree node scan found no nodes this range\n");
            }
        }

        if mustfix_fsck_err!(t, id::btree_node_topology_gap_between_nodes,
                             "gap between btree nodes{}", buf.as_str())? {
            commit_topology_repair_log(t)?;
            if nodes_found != 0 {
                return Ok(Repair::FilledFromScan);
            }
            set_node_min(trans, &mut trans.paths(), cur.idx, level, expected_start)?;
        }
    } else if let Some(p) = prev.filter(|p| cur.b.contents().data().seq() >
                                             p.b.contents().data().seq()) {		/* cur overwrites prev */
        if p.b.contents().data().min_key >= cur_min {						/* fully? */
            if mustfix_fsck_err!(t, id::btree_node_topology_overwritten_by_next_node,
                                 "btree node overwritten by next node{}", buf.as_str())? {
                commit_topology_repair_log(t)?;
                return Ok(Repair::DropPrev);
            }
        } else if mustfix_fsck_err!(t, id::btree_node_topology_bad_max_key,
                                    "btree node with incorrect max_key{}", buf.as_str())? {
            commit_topology_repair_log(t)?;
            set_node_max(trans, &mut trans.paths(), p.idx, level, cur_min.predecessor())?;
        }
    } else if expected_start >= cur.b.contents().data().max_key {				/* fully? */
        if mustfix_fsck_err!(t, id::btree_node_topology_overwritten_by_prev_node,
                             "btree node overwritten by prev node{}", buf.as_str())? {
            commit_topology_repair_log(t)?;
            return Ok(Repair::DropThis);
        }
    } else if mustfix_fsck_err!(t, id::btree_node_topology_bad_min_key,
                                "btree node with incorrect min_key{}", buf.as_str())? {
        commit_topology_repair_log(t)?;
        set_node_min(trans, &mut trans.paths(), cur.idx, level, expected_start)?;
    }
    Ok(Repair::None)
}

/// A node at @level, by its key @k: had by a node iterator at the key's
/// position, which lands in it whatever its range says.
fn node_at<'t>(t: &TransAttempt<'t, 't>, btree: Btree, level: usize, k: &BkeyI,
               flags: BtreeIterFlags) -> Result<BtreeNodeIter<'t>, BchError> {
    let locks_want = if flags.contains(BtreeIterFlags::INTENT) { level as u32 + 1 } else { 0 };
    let mut iter = BtreeNodeIter::new(t, btree, k.k.p, locks_want, level as u32, flags);
    iter.peek(t)?.expect("a node, at its key");
    Ok(iter)
}

/// The child key of @b at or after @pos: from its keys and the journal's.
fn child_key(t: &TransAttempt<'_, '_>, b: Node<'_>, pos: Bpos) -> Option<BkeyBuf> {
    let fs = t.fs();
    let pos = pos.max(b.contents().data().min_key);
    let mut node_iter = KeyIter::default();
    node_iter.init(fs, &b.keys(b.contents()), &pos);

    let mut iter = BtreeAndJournalIter::new(t, b, node_iter, pos);
    iter.prefetch = true;

    let k = iter.peek()?;
    let data = b.contents().data();
    assert!(k.k.p >= data.min_key && k.k.p <= data.max_key,
            "a child key at {}, outside its parent's {} - {}", k.k.p, data.min_key, data.max_key);

    let mut buf = BkeyBuf::new();
    buf.reassemble(k);
    Some(buf)
}

/// The child of @node at or after @pos, checked against the one before it:
/// None if there's none.
fn check_child(t: &TransAttempt<'_, '_>, btree: Btree, level: usize, node: &BkeyI, pos: Bpos,
               prev: Option<&BkeyI>) -> Result<Option<(BkeyBuf, Repair)>, BchError> {
    let fs = t.fs();
    let parent = node_at(t, btree, level, node, BtreeIterFlags::empty())?;

    let b = held(&t.trans().paths(), &parent).b;
    let Some(cur_k) = child_key(t, b, pos) else { return Ok(None) };

    let cur = bch_err_msg!(fs, node_at(t, btree, level - 1, &cur_k,
                                       BtreeIterFlags::INTENT | BtreeIterFlags::READ_ERRORS),
                           "getting btree node")?;
    let prev = prev.map(|k| node_at(t, btree, level - 1, k, BtreeIterFlags::INTENT)).transpose()?;

    let (cur, prev) = {
        let paths = t.trans().paths();
        (held(&paths, &cur), prev.as_ref().map(|p| held(&paths, p)))
    };

    if cur.b.flags.read_error() {
        return Ok(Some((cur_k, Repair::DropThis)));
    }

    if btree_node_is_stale(fs, cur.b.node()) {
        bch_info!(fs, "btree node older than nodes found by scanning\n  btree={btree} level={} {}",
                  level - 1, BkeySC::from(&*cur_k).to_text(fs));
        return Ok(Some((cur_k, Repair::DropThis)));
    }

    let repair = check_node_boundaries(t, b, prev, cur)?;
    Ok(Some((cur_k, repair)))
}

fn repair_node_end(t: &TransAttempt<'_, '_>, btree: Btree, level: usize, node: &BkeyI,
                   last: &BkeyI) -> Result<Repair, BchError> {
    let fs = t.fs();
    let parent = node_at(t, btree, level, node, BtreeIterFlags::empty())?;
    let child = node_at(t, btree, level - 1, last, BtreeIterFlags::INTENT)?;

    let (b, child) = {
        let paths = t.trans().paths();
        (held(&paths, &parent).b, held(&paths, &child))
    };
    let end = b.contents().key_i().k.p;
    let child_end = child.b.contents().key_i().k.p;

    if child_end == end {
        return Ok(Repair::None);
    }

    let mut buf = Printbuf::new();
    prt!(&mut buf, "\nat: btree={btree} level={level}\nparent: {}\nchild: {}",
         BkeySC::from(b.contents().key_i()).to_text(fs), BkeySC::from(child.b.contents().key_i()).to_text(fs));

    let mut nodes_found = 0;
    if level == 1 {
        get_scanned_nodes(fs, btree, 0, child_end.successor(), end, &mut buf, &mut nodes_found)?;
    }

    if mustfix_fsck_err!(t, id::btree_node_topology_bad_max_key,
                         "btree node with incorrect max_key{}", buf.as_str())? {
        commit_topology_repair_log(t)?;

        if nodes_found != 0 {
            return Ok(Repair::FilledFromScan);
        }
        set_node_max(t.trans(), &mut t.trans().paths(), child.idx, level - 1, end)?;
    }
    Ok(Repair::None)
}

/// The node at @k, a child of a node at @level, out of the cache and its key
/// out of its parent.
fn drop_node(trans: &BtreeTrans<'_>, btree: Btree, level: usize, k: &BkeyI) -> Result<(), BchError> {
    lockrestart_do(trans, |t| {
        let iter = node_at(t, btree, level - 1, k, BtreeIterFlags::INTENT | BtreeIterFlags::READ_ERRORS)?;
        t.trans().node_evict(&mut t.trans().paths(), iter.raw.path, level - 1)
    })?;

    journal_overlay::delete(&mut trans.fs().not_rw(), btree, level as u32, k.k.p)
}

/// Each of @node's children against the one before it, and the last against
/// @node's end: true if the children changed, to be walked again.
fn check_children(trans: &BtreeTrans<'_>, btree: Btree, level: usize, node: &BkeyI) -> Result<bool, BchError> {
    let fs = trans.fs();
    let mut pos = POS_MIN;
    let mut prev: Option<BkeyBuf> = None;
    let mut again = false;

    while let Some((k, repair)) =
        lockrestart_do(trans, |t| check_child(t, btree, level, node, pos, prev.as_deref()))?
    {
        let end = k.k.p;

        match repair {
            Repair::None | Repair::FilledFromScan => {
                again |= repair == Repair::FilledFromScan;
                prev = Some(k);
            }
            Repair::DropThis => drop_node(trans, btree, level, &k)?,
            Repair::DropPrev => {
                bch_info!(fs, "dropped prev node");
                drop_node(trans, btree, level, prev.as_deref().expect("a prev node, to drop"))?;
                return Ok(true);
            }
        }

        if end == node.k.p {
            break;
        }
        pos = end.successor();
    }

    if let Some(prev) = &prev {
        again |= lockrestart_do(trans, |t| repair_node_end(t, btree, level, node, prev))? == Repair::FilledFromScan;
    }
    Ok(again)
}

/// The child key of the node at @node at or after @pos, and the child read.
fn next_child(t: &TransAttempt<'_, '_>, btree: Btree, level: usize, node: &BkeyI,
              pos: Bpos) -> Result<Option<BkeyBuf>, BchError> {
    let parent = node_at(t, btree, level, node, BtreeIterFlags::empty())?;

    let b = held(&t.trans().paths(), &parent).b;
    let Some(k) = child_key(t, b, pos) else { return Ok(None) };

    bch_err_msg!(t.fs(), node_at(t, btree, level - 1, &k, BtreeIterFlags::empty()), "getting btree node")?;
    Ok(Some(k))
}

/// @node's children checked, then each repaired below, over again until a
/// pass changes nothing: DropThis if it's left with none.
fn repair_children(trans: &BtreeTrans<'_>, btree: Btree, level: usize, node: &BkeyI) -> Result<Repair, BchError> {
    let fs = trans.fs();

    loop {
        if check_children(trans, btree, level, node)? {
            continue;
        }

        let mut pos = POS_MIN;
        let mut have_child = false;
        let mut again = false;

        while let Some(k) = lockrestart_do(trans, |t| next_child(t, btree, level, node, pos))? {
            if repair_topology_recurse(trans, btree, level - 1, &k)? == Repair::DropThis {
                drop_node(trans, btree, level, &k)?;
                again = true;
            }
            have_child = true;

            if k.k.p == node.k.p {
                break;
            }
            pos = k.k.p.successor();
        }

        /*
         * XXX: we're not passing the trans object here because we're not set up
         * to handle a transaction restart - this code needs to be rewritten
         * when we start doing online topology repair
         */
        trans.unlock_long();
        if mustfix_fsck_err_on!(fs, !have_child, id::btree_node_topology_interior_node_empty,
                                "empty interior btree node at btree={btree} level={level}\n{}",
                                BkeySC::from(node).to_text(fs))? {
            return Ok(Repair::DropThis);
        }

        if !again {
            return Ok(Repair::None);
        }
    }
}

fn repair_topology_recurse(trans: &BtreeTrans<'_>, btree: Btree, level: usize,
                           node: &BkeyI) -> Result<Repair, BchError> {
    if level == 0 {
        return Ok(Repair::None);
    }

    let ret = repair_children(trans, btree, level, node);

    if ret == Ok(Repair::None) {
        lockrestart_do(trans, |t| {
            let iter = node_at(t, btree, level, node, BtreeIterFlags::empty())?;
            let b = held(&t.trans().paths(), &iter).b;
            assert!(node_check_topology(t, b).is_ok(), "a node's topology bad after its repair");
            Ok(())
        })?;
    }

    bch_err_fn!(trans.fs(), ret)
}

fn topology_check_root(t: &TransAttempt<'_, '_>, btree: Btree,
                       reconstructed_root: &mut bool) -> Result<(), BchError> {
    let fs = t.fs();
    let trans = t.trans();
    let roots = &fs.btree.cache.roots;
    let err = roots.error(btree.0);

    if err == 0 {
        return Ok(());
    }

    let mut msg = LogMsg::new(fs);
    prt!(&mut *msg, "btree root {btree} unreadable: {}\n", BchError::from_raw(err).msg());

    let mut fake_root = |level| {
        roots.lock().get_mut(btree.0).expect("a btree below btree_id_nr_alive()").alive = 0;
        fs.btree_id_root_set_error(btree.0, 0);
        let _ = root_alloc_fake_trans(trans, btree, level);
        *reconstructed_root = true;
    };

    if !btree_id_recovers_from_scan(btree) {
        fake_root(0);
        btree_lost_data(fs, &mut msg, btree)?;
    } else if !btree_has_scanned_nodes(fs, btree, &mut msg)? {
        msg.set_suppressed(true);

        let autofix = if btree_id_can_reconstruct(btree.0) { FsckFlags::AUTOFIX } else { FsckFlags::empty() };
        fsck_err_flags!(t, POS_MIN, FsckFlags::CAN_FIX | autofix,
                        id::btree_root_unreadable_and_scan_found_nothing,
                        "{}btree node scan found no nodes, continue?", msg.as_str())?;

        fake_root(0);
    } else {
        fake_root(1);
        shoot_down_journal_keys(fs, btree, 1, c::BTREE_MAX_DEPTH, POS_MIN, SPOS_MAX);

        let mut nodes_found = 0;
        get_scanned_nodes(fs, btree, 0, POS_MIN, SPOS_MAX, &mut msg, &mut nodes_found)?;
    }

    /*
     * Commit any fsck_err log entry queued above before returning: this
     * runs pre-RW (so the commit is queued for journal replay), and the
     * caller's per-btree loop starts each iteration with a bch2_trans_begin
     * that would otherwise discard it.
     */
    t.commit(None, CommitFlags::NO_ENOSPC)
}

fn check_root_boundaries(t: &TransAttempt<'_, '_>, btree: Btree) -> Result<(usize, BkeyBuf), BchError> {
    let fs = t.fs();
    let (level, root_k) = {
        let b = fs.btree_id_root(btree.0).expect("a root, after topology_check_root()");
        /* unlocked: recovery runs alone, and the root stays put */
        (usize::from(b.c.level), BkeyBuf::from_key(BkeySC::from(unsafe { b.contents() }.key_i())))
    };

    let iter = node_at(t, btree, level, &root_k, BtreeIterFlags::INTENT)?;

    let Held { idx, b } = held(&t.trans().paths(), &iter);
    let data = b.contents().data();

    assert!(BkeySC::from(b.contents().key_i()).arm::<c::BtreePtrV2>()
            .is_none_or(|p| p.v().min_key == data.min_key),
            "a btree_ptr_v2's min_key not its node's");

    let mut buf = Printbuf::new();
    prt!(&mut buf, "  at {}", b.pos_to_text(fs));

    if mustfix_fsck_err_on!(t, data.min_key != POS_MIN, id::btree_node_topology_bad_root_min_key,
                            "btree root with incorrect min_key{}", buf.as_str())? {
        set_node_min(t.trans(), &mut t.trans().paths(), idx, level, POS_MIN)?;
    }

    if mustfix_fsck_err_on!(t, b.contents().data().max_key != SPOS_MAX, id::btree_node_topology_bad_root_max_key,
                            "btree root with incorrect max_key{}", buf.as_str())? {
        set_node_max(t.trans(), &mut t.trans().paths(), idx, level, SPOS_MAX)?;
    }

    Ok((level, BkeyBuf::from_key(BkeySC::from(b.contents().key_i()))))
}

/// The root at @k, at @level, emptied by repair: out of the cache, and out of
/// roots_b[].
fn drop_root(t: &TransAttempt<'_, '_>, btree: Btree, level: usize, k: &BkeyI) -> Result<(), BchError> {
    let fs = t.fs();
    let trans = t.trans();
    let bc = &fs.btree.cache;

    let iter = node_at(t, btree, level, k, BtreeIterFlags::INTENT)?;

    let mut paths = trans.paths();
    let Held { idx, b } = held(&paths, &iter);

    /*
     * Unhashing asserts SIX_LOCK_write even here, where
     * recovery is the only thing running:
     */
    trans.node_lock_write(&mut paths, idx, level)?;

    /*
     * Undo bch2_btree_set_root_inmem() before dropping the
     * node: the cache state machine BUG_ONs on reclaiming a
     * node that still says it's a root.
     */
    bc.roots.lock().published().roots_b[btree.0 as usize].set(0);

    {
        let _lists = bc.lock.lock();
        b.flags.set_permanent(false);
    }

    let _ = fs.btree_node_transition_state(b.node(), c::btree_node_cache_state::FREEABLE);

    trans.node_unlock_write(&mut paths, idx, level);
    trans.node_unlock(&mut paths, idx, level);
    paths[idx].l[level].b = NodePtr::err(bch_errcode::BCH_ERR_no_btree_node_drop.into());
    trans.node_forget(&mut paths, b.node());
    Ok(())
}

fn check_topology(fs: &Fs) -> Result<(), BchError> {
    let trans = btree_trans!(fs);

    trans.unlock_long();

    for i in 0..fs.btree_id_nr_alive() {
        let btree = Btree(i);
        let mut reconstructed_root = false;

        loop {
            lockrestart_do(&trans, |t| topology_check_root(t, btree, &mut reconstructed_root))?;

            let (level, root_k) = lockrestart_do(&trans, |t| check_root_boundaries(t, btree))?;

            if repair_topology_recurse(&trans, btree, level, &root_k)? == Repair::DropThis {
                lockrestart_do(&trans, |t| drop_root(t, btree, level, &root_k))?;

                if !reconstructed_root {
                    fs.btree_id_root_set_error(i, -(c::EIO as i32));
                    continue;
                }

                bch_err!(fs, "empty btree root {btree}");
                let _ = root_alloc_fake_trans(&trans, btree, 0);
                fs.btree.cache.roots.lock().get_mut(i).expect("a btree below btree_id_nr_alive()").alive = 0;
            }
            break;
        }
    }

    /*
     * post topology repair there should be no errored nodes; reset
     * ratelimiters so we see new unexpected errors
     */
    ratelimit_reset(&fs.btree.read_errors_soft);
    ratelimit_reset(&fs.btree.read_errors_hard);

    Ok(())
}

crate::recovery_pass!(check_topology => check_topology);
