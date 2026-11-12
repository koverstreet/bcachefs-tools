// SPDX-License-Identifier: GPL-2.0

//! Splits, merges and rewrites, on InteriorUpdate.

use core::ffi::{c_int, c_uint};
use core::ptr;
use core::sync::atomic::Ordering::{Relaxed, SeqCst};
use cstruct_macros::c_export;
use crate::btree::bkey::{self, bkey_validate, BkeyI, BkeyP, BkeySC, BkeyV, ValidateFlags,
                         BKEY_FORMAT_CURRENT, POS_MIN, SPOS_MAX};
use crate::btree::cache::btree_ptr_hash_val;
use crate::btree::bkey_buf::BkeyBuf;
use crate::btree::iter::{lockrestart_do, BtreeIter, BtreeIterFlags, BtreeNodeIter, BtreeTrans, CommitFlags, CommitOpts,
                         TransAttempt, Watermark};
use crate::btree::journal_overlay::{journal_key_overwrite, key_deleted_in_journal, BtreeAndJournalIter};
use crate::btree::node::BtreeNode;
use crate::btree::node::iter::KeyIter;
use crate::btree::node::sort::{bch2_btree_build_aux_trees, bch2_btree_sort_into, bch2_maybe_compact_whiteouts};
use crate::btree::path::{LockedNode, PathIdx, Paths};
use crate::btree::shard::key_is_shard_boundary;
use crate::btree::types::BtreeWriteType;
use crate::btree::validate::{bkey_in_btree_node, validate_from};
use crate::c;
use crate::data_structures::array_vec::ArrayVec;
use crate::data::keylist::{keys, keys_mut};
use crate::debug::trace::events;
use crate::errcode::{bch_errcode, ret_to_c, BchError};
use crate::format::{Bpos, Btree};
use crate::fs::Fs;
use crate::init::error::{count_fsck_err, BkeyValidate};
use crate::init::progress::Progress;
use crate::locking::MemallocFlags;
use crate::{bch_info, btree_trans, prt};
use crate::sb::errors::SbErrorId;
use crate::util::Printbuf;
use crate::util::backtrace::prt_task_backtrace;
use crate::util::kernel::{dump_stack, local_clock};
use crate::util::log::LogMsg;
use crate::util::sched;
use crate::util::time_stats::TimeStats;
use crate::util::vstructs::Head;

use super::{InteriorUpdate, NewNode, ParentKeys, StartOpts};

/// A btree node held locked: by a path, or new from the update's reserve.
type Node<'n> = LockedNode<'n, BtreeNode>;

fn insert_fixup_btree_ptr(trans: &BtreeTrans<'_>, paths: &mut Paths<'_>, path: PathIdx, b: Node<'_>,
                          node_iter: &mut KeyIter, insert: &mut BkeyI) {
    let fs = trans.fs();
    let k = BkeySC::from(&*insert);

    if let Some(p) = k.arm::<c::BtreePtrV2>() {
        assert!(p.v().sectors_written != 0, "a btree_ptr_v2 with no sectors written: {}", k.to_text(fs));
    }

    if trans.journal_replay_not_finished() {
        unsafe { journal_key_overwrite(fs, Btree(b.c.btree_id.into()), b.c.level.into(), insert.k().p) };
    }

    let from = validate_from(&b, ValidateFlags::commit);
    let v = BkeyValidate { fs, k, from: &from };
    if bkey_validate(&v).and_then(|()| bkey_in_btree_node(&v, &b)).is_err() {
        crate::fs_inconsistent!(fs, "insert_fixup_btree_ptr(): inserting invalid bkey");
        dump_stack();
    }

    {
        let contents = b.contents();
        let node_keys = b.keys(contents);
        while node_iter.peek_all().is_some_and(|i| node_keys.bkey_iter_pos_cmp(i, &insert.k().p).is_lt()) {
            node_iter.advance(&node_keys);
        }
    }

    unsafe { trans.bset_insert_key(paths, path, &b, node_iter, insert) };

    let _ = b.flags.fetch_update(SeqCst, Relaxed, |f| {
        Some(f.with_write_type(BtreeWriteType::interior).with_need_write(true))
    });
}

/// The keys at the front of @list that go in interior node @b - up to its end
/// - inserted from @node_iter, @path's at @b's level, and taken off the list.
pub(crate) fn insert_keys_interior(trans: &BtreeTrans<'_>, paths: &mut Paths<'_>, path: PathIdx, b: Node<'_>,
                                   mut node_iter: KeyIter, list: &mut ParentKeys) -> Result<(), BchError> {
    let fs = trans.fs();
    assert!(b.c.level > 0, "inserting pointers into a leaf");

    let front = list.front().expect("inserting no keys").k().p;
    {
        let contents = b.contents();
        let node_keys = b.keys(contents);
        while let Some(i) = node_iter.prev_all(&node_keys) {
            if node_keys.unpack_pos(i) < front {
                break;
            }
        }
    }

    let end = b.contents().key_i().k().p;
    let mut inserted = 0;
    for k in keys_mut(list.as_u64s_mut()).take_while(|k| k.k().p <= end) {
        inserted += usize::from(k.k().u64s);
        insert_fixup_btree_ptr(trans, paths, path, b, &mut node_iter, k);
    }

    let mut msg = LogMsg::new(fs);
    msg.set_suppressed(true);

    if let Err(e) = check_topology_msg(trans, b, &mut msg) {
        prt!(msg, "inserted keys\n");
        msg.indent_add(2);
        for k in keys(&list.as_u64s()[..inserted]) {
            prt!(msg, "{}\n", k.to_text(fs));
        }
        msg.indent_sub(2);

        let _ = prt_task_backtrace(&mut msg, sched::current(), 0);
        fs.emergency_read_only(&mut msg);
        return Err(e);
    }

    list.drop_front(inserted);
    Ok(())
}

/*
 * For updates to interior nodes, we've got to do the insert before we split
 * because the stuff we're inserting has to be inserted atomically. Post split,
 * the keys might have to go in different nodes and the split would no longer be
 * atomic.
 *
 * Worse, if the insert is from btree node coalescing, if we do the insert after
 * we do the split (and pick the pivot) - the pivot we pick might be between
 * nodes that were coalesced, and thus in the middle of a child node post
 * coalescing:
 */
pub(crate) fn split_insert_keys(trans: &BtreeTrans<'_>, paths: &mut Paths<'_>, path: PathIdx, b: Node<'_>,
                                list: &mut ParentKeys) -> Result<(), BchError> {
    let Some(front) = list.front().map(|k| k.k().p) else { return Ok(()) };

    let contents = b.contents();
    if front > contents.data().max_key {
        return Ok(());
    }

    let mut node_iter = KeyIter::default();
    node_iter.init(trans.fs(), &b.keys(contents), &front);
    insert_keys_interior(trans, paths, path, b, node_iter, list)
}

fn topology_err(fs: &Fs, b: Node<'_>, out: &mut Printbuf) -> BchError {
    prt!(out, "in parent node:\n{}\n", b.pos_to_text(fs));
    let e = fs.topology_error(out);
    let _ = prt_task_backtrace(out, sched::current(), 1);
    e
}

/// The previous child's end: None before the first, while @prev holds
/// BkeyBuf::new()'s deleted key.
fn prev_end(prev: &BkeyBuf) -> Option<Bpos> {
    let k: &BkeyI = prev.as_ref();
    (!k.k().key_type().is_deleted()).then_some(k.k().p)
}

/*
 * Verify that child nodes correctly span parent node's range:
 */
pub(crate) fn check_topology_msg(trans: &BtreeTrans<'_>, b: Node<'_>, out: &mut Printbuf) -> Result<(), BchError> {
    let fs = trans.fs();
    let contents = b.contents();
    let data = contents.data();

    if let Some(p) = BkeySC::from(contents.key_i()).arm::<c::BtreePtrV2>() {
        assert!(p.v().min_key == data.min_key, "a node whose btree_ptr_v2 min_key {} isn't its own, {}",
                p.v().min_key, data.min_key);
    }

    /*
     * Don't use btree_node_is_root(): we're called by btree split, after
     * creating a new root but before setting it
     */
    if fs.btree_id_root(b.c.btree_id.into()).is_some_and(|r| ptr::eq(r, b.node())) {
        if data.min_key != POS_MIN {
            prt!(out, "btree root with incorrect min_key: {}\n", data.min_key);
            count_fsck_err(fs, SbErrorId::btree_root_bad_min_key, out);
            return Err(topology_err(fs, b, out));
        }

        if data.max_key != SPOS_MAX {
            prt!(out, "btree root with incorrect max_key: {}\n", data.max_key);
            count_fsck_err(fs, SbErrorId::btree_root_bad_max_key, out);
            return Err(topology_err(fs, b, out));
        }
    }

    if b.c.level == 0 {
        return Ok(());
    }

    let mut node_iter = KeyIter::default();
    node_iter.init_from_start(&b.keys(contents));
    let mut iter = BtreeAndJournalIter::new(trans, b, node_iter, data.min_key);
    let mut prev = BkeyBuf::new();

    while let Some(k) = iter.peek() {
        let Some(bp) = k.arm::<c::BtreePtrV2>() else { return Ok(()) };

        let expected_min = match prev_end(&prev) {
            None      => data.min_key,
            Some(end) => end.successor(),
        };
        if expected_min != bp.v().min_key {
            prt!(out, "end of prev node doesn't match start of next node\nprev {}\nnext {}\n",
                 BkeySC::from(prev.as_ref()).to_text(fs), k.to_text(fs));
            count_fsck_err(fs, SbErrorId::btree_node_topology_bad_min_key, out);
            return Err(topology_err(fs, b, out));
        }

        prev.reassemble(k);
        iter.advance();
    }

    let Some(end) = prev_end(&prev) else {
        prt!(out, "empty interior node\n");
        count_fsck_err(fs, SbErrorId::btree_node_topology_empty_interior_node, out);
        return Err(topology_err(fs, b, out));
    };

    if end != contents.key.k.p {
        prt!(out, "last child node doesn't end at end of parent node\nchild: {}\n",
             BkeySC::from(prev.as_ref()).to_text(fs));
        count_fsck_err(fs, SbErrorId::btree_node_topology_bad_max_key, out);
        return Err(topology_err(fs, b, out));
    }

    Ok(())
}

pub(crate) fn check_topology(trans: &BtreeTrans<'_>, b: Node<'_>) -> Result<(), BchError> {
    let mut msg = LogMsg::new(trans.fs());
    msg.set_suppressed(true);
    check_topology_msg(trans, b, &mut msg)
}

pub(super) fn split_threshold(fs: &Fs) -> usize {
    fs.btree_max_u64s() * 3 / 4
}

/*
 * Per-candidate scratch for picking a split pivot: the best
 * candidate's pivot and formats become its dsts().
 */
struct SplitLayout {
    pivot:  Bpos,
    format: [c::bkey_format; 2],
    size:   [usize; 2],
}

/*
 * Walk every src key once and produce a split_layout: per-side format /
 * size / pivot. Two pivot policies:
 *
 *   @n1_target_u64s == 0: static @pivot, partition by key.p <= pivot.
 *   @n1_target_u64s != 0: running u64s, place keys on side[0] until cumulative
 *     reaches @n1_target_u64s. On interior nodes shift past a deleted-in-journal
 *     or deleted-in-@inserting cross-key (parent reference can't point at a
 *     to-be-deleted key). The eventual pivot is the last unshifted side[0] key.
 */
fn predict_split(trans: &BtreeTrans<'_>, srcs: &[Node<'_>], inserting: Option<&InteriorUpdate<'_>>,
                 n1_target_u64s: usize, mut pivot: Bpos) -> SplitLayout {
    let mut state = [c::bkey_format_state::new(), c::bkey_format_state::new()];
    let mut nr_keys = [0; 2];
    let mut val_u64s = [0; 2];
    let (mut u64s, mut n1_u64s) = (0, n1_target_u64s);

    if n1_target_u64s != 0 {
        pivot = POS_MIN;
    }

    for src in srcs {
        let contents = src.contents();
        let keys = src.keys(contents);
        let mut iter = KeyIter::default();
        iter.init_from_start(&keys);

        while let Some(i) = iter.peek(&keys) {
            let k = keys.key_at(i);
            let uk = keys.unpack(i);

            let side = if n1_target_u64s != 0 {
                let shifted = src.c.level != 0 &&
                    u64s < n1_u64s &&
                    u64s + k.u64s() >= n1_u64s &&
                    (key_deleted_in_journal(trans, Btree(src.c.btree_id.into()), src.c.level.into(), uk.p) ||
                     inserting.is_some_and(|u| u.parent_key_deleted(uk.p)));
                if shifted {
                    n1_u64s += k.u64s();
                }

                let side = usize::from(u64s >= n1_u64s);
                u64s += k.u64s();
                if side == 0 && !shifted {
                    pivot = uk.p;
                }
                side
            } else {
                usize::from(uk.p > pivot)
            };

            state[side].add_key(&uk);
            nr_keys[side] += 1;
            val_u64s[side] += k.val_u64s(&contents.format);

            iter.advance(&keys);
        }
    }

    let first = srcs[0].contents().data();
    let last = srcs[srcs.len() - 1].contents().data();
    state[0].add_pos(first.min_key);
    state[0].add_pos(pivot);
    state[1].add_pos(pivot.successor());
    state[1].add_pos(last.max_key);

    let format = state.map(|mut s| s.done());
    let size = [0, 1].map(|i| nr_keys[i] * usize::from(format[i].key_u64s) + val_u64s[i]);
    SplitLayout { pivot, format, size }
}

/*
 * Pick a split pivot for @b: the per-side formats, and the pivot that ends
 * the first side - a single layout via predict_split's running mode
 * (journal-delete-aware shift uses @inserting). Accepted unconditionally;
 * caller is committed to splitting. A per-side predicted format that
 * overflows the node buf is replaced by @b's own.
 */
fn split_layout(trans: &BtreeTrans<'_>, b: Node<'_>, inserting: Option<&InteriorUpdate<'_>>) -> SplitLayout {
    let fs = trans.fs();
    let contents = b.contents();
    let shard_pivot = b.shard_pivot(contents, fs);

    let mut layout = if shard_pivot != POS_MIN {
        /*
         * Shard pivot takes priority over size balance: force the
         * split at the boundary to keep the leaf level shard-aligned,
         * which is what gives concurrent inode/dirent/xattr allocators
         * their disjoint-btree-node separation. An uneven layout is
         * fine — must_split is recursive, so an overflowing side gets
         * another shot.
         */
        predict_split(trans, &[b], inserting, 0, shard_pivot)
    } else {
        predict_split(trans, &[b], inserting, usize::from(contents.nr.live_u64s) * 3 / 5, POS_MIN)
    };

    for i in 0..2 {
        if size_of::<Head<c::btree_node>>() + layout.size[i] * size_of::<u64>() > b.buf_bytes() {
            layout.format[i] = contents.format;
        }
    }
    layout
}

/*
 * Pick a split pivot for merging @srcs into two: three candidates (natural
 * breaks at srcs[0]/srcs[nr-2] max_key + mid-stream at total_live/2), pick
 * the one minimizing max(size[0], size[1]) with non-empty sides. Bail if
 * best exceeds split_thresh.
 */
fn merge_layout(trans: &BtreeTrans<'_>, srcs: &[Node<'_>]) -> Option<SplitLayout> {
    let fs = trans.fs();
    let shard_pivot = srcs[0].shard_pivot(srcs[0].contents(), fs);

    let best = if shard_pivot != POS_MIN {
        /*
         * Mergers never reach this branch in practice: sib_u64s is
         * poisoned at shard boundaries (merge()), so srcs[0]'s
         * shard_pivot is POS_MIN whenever a merger gets here.
         */
        Some(predict_split(trans, srcs, None, 0, shard_pivot))
    } else {
        let max_key = |b: Node<'_>| b.contents().data().max_key;

        let mut candidates: ArrayVec<Bpos, 3> = ArrayVec::new();
        candidates.push(max_key(srcs[0]));
        if srcs.len() >= 3 {
            candidates.push(max_key(srcs[srcs.len() - 2]));
        }

        let total_live: usize = srcs.iter().map(|s| usize::from(s.contents().nr.live_u64s)).sum();
        let target = total_live / 2;
        let mut curr = 0;
        'srcs: for s in srcs {
            let keys = s.keys(s.contents());
            let mut iter = KeyIter::default();
            iter.init_from_start(&keys);

            while let Some(i) = iter.peek(&keys) {
                curr += keys.key_at(i).u64s();
                if curr >= target {
                    candidates.push(keys.unpack_pos(i));
                    break 'srcs;
                }
                iter.advance(&keys);
            }
        }

        candidates.iter()
            .map(|&pivot| predict_split(trans, srcs, None, 0, pivot))
            /*
             * Empty-side candidate scores 0 on its empty side and
             * would beat a balanced candidate whose far side
             * inherits a wide format from the trailing src's
             * max_key. Reject before the min-worst comparison.
             */
            .filter(|cand| cand.size[0] != 0 && cand.size[1] != 0)
            .min_by_key(|cand| cand.size[0].max(cand.size[1]))
    };

    best.filter(|best| best.size[0].max(best.size[1]) <= split_threshold(fs))
}

#[derive(Clone, Copy)]
struct DstLayout {
    format:  c::bkey_format,
    min_key: Bpos,
    max_key: Bpos,
}

impl SplitLayout {
    fn dsts(&self, min_key: Bpos, max_key: Bpos) -> [DstLayout; 2] {
        [DstLayout { format: self.format[0], min_key, max_key: self.pivot },
         DstLayout { format: self.format[1], min_key: self.pivot.successor(), max_key }]
    }
}

fn alloc_dst<'u>(update: &'u InteriorUpdate<'_>, paths: &mut Paths<'_>, srcs: &[Node<'_>],
                 DstLayout { format, min_key, max_key }: DstLayout) -> NewNode<'u> {
    let seq = srcs.iter().map(|s| s.contents().data().seq()).max().unwrap_or(0) + 1;
    let mut n = update.alloc_node(paths, usize::from(srcs[0].c.level));
    let contents = n.contents_mut();
    assert!(contents.nsets == 1, "packing into a node with {} bsets", contents.nsets);

    super::reserve::set_range(contents, min_key, max_key);

    let data = contents.data_mut();
    data.set_seq(seq);
    data.format = format;
    contents.set_format(format);
    n
}

/*
 * Pack core: take dsts laid out by the caller (split_layout for split,
 * merge_layouts for merge) and pack every src key into the
 * appropriate dst, partitioning by key.p vs dsts[0]'s max_key.
 */
fn pack_into_dsts(trans: &BtreeTrans<'_>, srcs: &[Node<'_>], dsts: &mut [NewNode<'_>; 2]) {
    let pivot = dsts[0].contents().data().max_key;
    let mut out = dsts.each_mut().map(|n| n.keys_mut());
    let mut at = out.each_ref().map(|keys| keys.sets[0].first_offset() as usize);

    for src in srcs {
        let src_contents = src.contents();
        let src_keys = src.keys(src_contents);
        let mut iter = KeyIter::default();
        iter.init_from_start(&src_keys);

        while let Some(i) = iter.peek(&src_keys) {
            let k = src_keys.key_at(i);
            let side = usize::from(src_keys.unpack_pos(i) > pivot);
            let dst = &mut out[side];

            let room = &mut dst.buf[at[side]..];
            let k_f = if k.is_packed() { &src_contents.format } else { &BKEY_FORMAT_CURRENT };
            if bkey::transform(dst.format, room, k_f, k) {
                BkeyP::from_slice_mut(room).expect("a transformed key").set_format(c::KEY_FORMAT_LOCAL_BTREE as u8);
            } else {
                k.unpack_into(src_contents, room);
            }

            let k = BkeyP::from_slice_mut(&mut dst.buf[at[side]..]).expect("a packed key");
            k.set_needs_whiteout(false);

            let k = &*k;
            dst.nr.account_key_add(0, k.cast());
            at[side] += k.u64s();

            iter.advance(&src_keys);
        }
    }

    for (side, mut keys) in out.into_iter().enumerate() {
        let t = keys.sets[0];
        keys.bset_head_mut(t.data_offset).set_u64s(at[side] - t.first_offset() as usize);
        keys.set_bset_end(0);
    }

    for n in dsts.iter_mut() {
        n.contents().reset_sib_u64s();
        n.keys_mut().verify_btree_nr_keys();

        assert!(check_topology(trans, n.locked()).is_ok(), "a packed node's topology bad:\n{}", n.to_text(trans.fs()));
    }
}

fn btree_node_op_log_one(out: &mut Printbuf, fs: &Fs, b: Node<'_>) {
    let max = fs.btree_max_u64s();
    let contents = b.contents();
    let live = usize::from(contents.nr.live_u64s);

    prt!(out, "{}\n", b.pos_to_text(fs));
    prt!(out, " live={} ({}%) whiteout={} remaining={} sib_u64s=[{},{}]\n",
         live, live * 100 / max, b.whiteout_u64s.get(), b.keys_u64s_remaining(contents),
         contents.sib_u64s[0].get(), contents.sib_u64s[1].get());
    out.indent_add(2);
    prt!(out, "format: {}\n", contents.format);
    out.indent_sub(2);
}

fn btree_node_op_log(out: &mut Printbuf, fs: &Fs, srcs: &[Node<'_>], dsts: &[NewNode<'_>]) {
    for (i, s) in srcs.iter().enumerate() {
        prt!(out, "src {i}: ");
        btree_node_op_log_one(out, fs, *s);
    }
    for (i, d) in dsts.iter().enumerate() {
        prt!(out, "dst {i}: ");
        btree_node_op_log_one(out, fs, d.locked());
    }
}

/// @b, @path's at its level, split in two - or compacted, if that leaves room
/// - and the new nodes made visible, in the parent or as a new root. With
/// @insert_parent_keys, the update's parent keys go in first: @b is a parent
/// they didn't fit in.
fn btree_split(update: &InteriorUpdate<'_>, paths: &mut Paths<'_>, path: PathIdx, b: Node<'_>,
               insert_parent_keys: bool) -> Result<(), BchError> {
    let ret = btree_split_in(update, paths, path, b, insert_parent_keys);
    update.trans.verify_locks(paths);
    ret
}

fn btree_split_in(update: &InteriorUpdate<'_>, paths: &mut Paths<'_>, path: PathIdx, b: Node<'_>,
                  insert_parent_keys: bool) -> Result<(), BchError> {
    let trans = update.trans;
    let fs = trans.fs();
    let level = usize::from(b.c.level);
    let has_parent = paths[path].locked_btree_node(level + 1).is_some();

    trans.verify_paths(paths);

    b.keys(b.contents()).verify_btree_nr_keys();
    assert!(has_parent || fs.btree_id_root(b.c.btree_id.into()).is_some_and(|r| ptr::eq(r, b.node())),
            "splitting a node with no parent that isn't the root:\n{}", b.to_text(fs));
    assert!(!has_parent || paths[path].intent_locked(level + 1),
            "splitting a node whose parent isn't intent locked:\n{}", b.to_text(fs));

    check_topology(trans, b)?;

    /* If we're splitting because an insert hit btree_node_full, compact
     * is only useful if the failed key would actually fit afterwards.
     * Otherwise we'd loop: compact produces same live_u64s, retry hits
     * btree_node_full again. Two distinct cases:
     *  - parent recursion (@insert_parent_keys): the new keys are merged into
     *    the post-compact bset directly, so we just need the combined
     *    data to fit in the buffer.
     *  - leaf retry (!@insert_parent_keys): the failed key arrives via trans
     *    restart, going into a follow-on bset on the new node. Account
     *    for write rounding (each bset write rounds up to block_bytes,
     *    so a node whose live data rounds up to fill the whole sector
     *    budget leaves no room for the follow-on bset).
     */
    let contents = b.contents();
    let live_u64s = usize::from(contents.nr.live_u64s);
    let must_split = if insert_parent_keys {
        live_u64s + update.parent_keys_u64s() > fs.btree_max_u64s()
    } else {
        update.new_key_u64s() != 0 && !b.compact_fits(contents, fs, update.new_key_u64s())
    };

    /*
     * Also split (vs compact) if a shard boundary lies inside this
     * leaf — compact would preserve the cross-shard layout that the
     * pid-hash inode sharding is trying to escape. Splitting drives
     * leaves toward shard-alignment automatically over normal growth;
     * no separate pre-split pass needed.
     */
    let split = must_split ||
        live_u64s > split_threshold(fs) ||
        b.shard_pivot(contents, fs) != POS_MIN;

    let stat = if split { c::bch_time_stats::btree_node_split } else { c::bch_time_stats::btree_node_compact };
    let _timer = fs.times[stat as usize].timer();

    let nodes: ArrayVec<NewNode<'_>, 2> = if split {
        let layout = split_layout(trans, b, insert_parent_keys.then_some(update));
        let data = b.contents().data();
        let mut dsts = layout.dsts(data.min_key, data.max_key).map(|l| alloc_dst(update, paths, &[b], l));

        pack_into_dsts(trans, &[b], &mut dsts);
        dsts.into_iter().collect()
    } else {
        [update.alloc_replacement(paths, &b)].into_iter().collect()
    };

    if insert_parent_keys {
        for n in &nodes {
            update.insert_parent_keys(paths, path, n)?;
        }
        assert!(update.parent_keys_u64s() == 0, "parent keys left over from a split");
    }

    for n in nodes.iter().rev() {
        bch2_btree_build_aux_trees(n);
    }

    let new_paths: ArrayVec<PathIdx, 2> = nodes.iter().map(|n| update.take_path(paths, n)).collect();

    /*
     * Note that on recursive we're inserting parent_keys, so we
     * can't start adding new keys to parent_keys before emptying it
     * out (which we did with insert_parent_keys() above)
     */
    if split || has_parent {
        for n in &nodes {
            update.link(n);
        }
    }

    /* New nodes all written, now make them visible: */

    let n3 = if has_parent {
        /* Split a non root node */
        update.insert_into_parent(paths, path, level)?;
        None
    } else if split {
        /* Depth increases, make a new root */
        let n3 = update.alloc_root(paths, new_paths[1], level + 1);
        update.insert_parent_keys(paths, path, &n3)?;
        update.set_root(paths, path, &n3);
        Some(n3)
    } else {
        /* Root filled up but didn't need to be split */
        update.set_root(paths, path, &nodes[0]);
        None
    };

    let log = |out: &mut Printbuf| btree_node_op_log(out, fs, &[b], &nodes);
    if nodes.len() == 2 {
        fs.event_inc_trace(events::btree_node_split, log);
    } else {
        fs.event_inc_trace(events::btree_node_compact, log);
    }

    let new: ArrayVec<_, 3> = n3.into_iter().chain(nodes.into_iter().rev()).collect();
    update.replace(paths, &[(path, b)], &new);
    Ok(())
}

/*
 * Inserts as many keys as it can into a given btree node, splitting it if full.
 * If a split occurred, this function will return early. This can only happen
 * for leaf nodes -- inserts into interior nodes have to be atomic.
 */
pub(crate) fn insert_node(update: &InteriorUpdate<'_>, paths: &mut Paths<'_>, path: PathIdx,
                          b: Node<'_>) -> Result<(), BchError> {
    let trans = update.trans;
    let fs = trans.fs();
    let level = usize::from(b.c.level);

    let u64s_and_live = || {
        let contents = b.contents();
        (b.keys(contents).bset(contents.last_set()).u64s(), contents.nr.live_u64s)
    };
    let (old_u64s, old_live_u64s) = u64s_and_live();

    assert!(level > 0, "inserting pointers into a leaf");
    assert!(!update.visible(), "an interior update inserting into a second existing node");
    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    update.verify_parent_keys_sorted();

    if !paths[path].intent_locked(level) {
        let mut msg = LogMsg::new(fs);
        prt!(msg, "insert_node(): node not locked at level {level}\n");
        update.to_text(&mut msg);
        paths[path].to_text(&mut msg, path);
        fs.emergency_read_only(&mut msg);
        return Err(BchError::from(c::EIO));
    }

    trans.node_lock_write(paths, path, level)?;

    trans.node_prep_for_write(&mut paths.paths_and_updates(), &b);

    let u64s = update.parent_keys_u64s();
    if !b.insert_fits(b.contents(), u64s) {
        /*
         * We could attempt to avoid the transaction restart, by calling
         * bch2_btree_path_upgrade() and allocating more nodes:
         */
        let ret = if level >= update.level_end() {
            fs.event_inc_trace(events::trans_restart_split_race, |out| {
                prt!(out, "{}\n", trans.fn_name());
                prt!(out, "l={} written {}/{} u64s remaining {}", level, b.written.get(),
                     fs.btree_sectors() / fs.block_sectors(), b.keys_u64s_remaining(b.contents()));
            });
            Err(trans.restart(bch_errcode::BCH_ERR_transaction_restart_split_race))
        } else {
            btree_split(update, paths, path, b, true)
        };
        trans.verify_locks(paths);
        return ret;
    }

    let ret = check_topology(trans, b).and_then(|()| update.insert_parent_keys_at(paths, path, b));
    if let Err(e) = ret {
        trans.verify_locks(paths);
        return Err(e);
    }

    if !b.flags.dirty() {
        fs.btree_node_set_dirty(b.node());
    }

    for idx in trans.path_idxs() {
        if trans.path_at_node(paths, idx, &b) {
            let keys = b.keys(b.contents());
            paths[idx].l[level].iter.peek(&keys);
        }
    }

    trans.verify_paths(paths);

    let (u64s, live_u64s) = u64s_and_live();
    let shrunk = old_live_u64s.saturating_sub(live_u64s);
    for sib in &b.contents().sib_u64s {
        if sib.get() != u16::MAX {
            sib.set(sib.get().saturating_sub(shrunk));
        }
    }

    let added_dead_u64s = u64s + usize::from(old_live_u64s) > old_u64s + usize::from(live_u64s);
    if added_dead_u64s && bch2_maybe_compact_whiteouts(fs, &b) {
        trans.node_reinit_iter(&mut paths.paths_and_updates(), b);
    }

    update.updated_node(&b);
    trans.node_unlock_write(paths, path, level);

    trans.revalidate_updates_in_node(&mut paths.paths_and_updates(), b);
    Ok(())
}

pub(crate) fn foreground_maybe_merge(trans: &BtreeTrans<'_>, paths: &mut Paths<'_>, path: PathIdx, level: usize,
                                     commit: CommitOpts, merge_count: Option<&mut u64>) -> Result<(), BchError> {
    trans.verify_not_unlocked_or_in_restart();

    let b = paths[path].locked_btree_node(level).expect("merging a node the path doesn't hold");
    if !b.needs_merge(b.contents(), trans.fs(), 0) {
        return Ok(());
    }

    merge(trans, paths, path, level, commit, merge_count)
}

/// The node at @iter merged with its siblings, if it needs it, or @iter moved
/// on past it: whether the walk is done.
fn merge_btree_node_one<'t>(t: &TransAttempt<'_, 't>, progress: &Progress<'_>, iter: &mut BtreeNodeIter<'t>,
                            merge_count: &mut u64) -> Result<bool, BchError> {
    iter.iter().traverse(t)?;

    let trans = t.trans();
    let path = iter.iter().path;
    let level = usize::from(trans.paths()[path].level());
    let Some(b) = trans.paths().held_btree_node(path, level) else { return Ok(true) };

    progress.update(t, iter.iter())?;

    if !b.needs_merge(b.contents(), t.fs(), 0) {
        let end = b.contents().key_i().k.p;
        if end == SPOS_MAX {
            return Ok(true);
        }

        iter.iter().set_pos(end.successor());
        return Ok(false);
    }

    let mut paths = trans.paths();
    trans.path_upgrade(&mut paths, path, level + 1, crate::ret_ip!())?;
    foreground_maybe_merge(trans, &mut paths, path, level, CommitOpts::new(), Some(merge_count))?;
    Ok(false)
}

fn merge_btree_nodes(fs: &Fs) -> Result<(), BchError> {
    let progress = Progress::recovery_masks(fs, c"bch2_merge_btree_nodes", !0, !0);

    let trans = btree_trans!(fs);

    for btree in (0..fs.btree_id_nr_alive()).map(Btree) {
        let mut merge_count = 0;

        for level in 0..c::BTREE_MAX_DEPTH as u32 {
            let mut iter = BtreeNodeIter::new(&trans, btree, POS_MIN, 0, level, BtreeIterFlags::PREFETCH);
            while !lockrestart_do(&trans, |t| merge_btree_node_one(t, &progress, &mut iter, &mut merge_count))? {}
        }

        if merge_count != 0 {
            bch_info!(fs, "merge_btree_nodes: {merge_count} merges in {btree} btree");
        }
    }

    Ok(())
}

crate::recovery_pass!(merge_btree_nodes => merge_btree_nodes);

const PREV_SIB: usize = c::btree_node_sibling::btree_prev_sib as usize;
const NEXT_SIB: usize = c::btree_node_sibling::btree_next_sib as usize;

fn merge_threshold(fs: &Fs) -> usize {
    fs.btree_max_u64s() / 3
}

fn merge_higher_threshold(fs: &Fs) -> usize {
    fs.btree_max_u64s() * 3 / 5
}

fn merge_hysteresis(fs: &Fs) -> usize {
    merge_threshold(fs) + (merge_threshold(fs) >> 2)
}

fn set_sib_u64s(b: Node<'_>, sib: usize, u64s: u16) {
    b.contents().sib_u64s[sib].set(u64s);
}

/// @path's node above @level, as btree_node_parent() has it: for identity.
fn parent(paths: &Paths<'_>, path: PathIdx, level: usize) -> *mut c::btree_bkey_cached_common {
    let p = paths[path].l[level + 1].b;
    if p.is_node() { p.as_ptr() } else { ptr::null_mut() }
}

/// A node a merge takes in: its path, which the merge holds a ref on, and its
/// live u64s - an estimate, from the evicted-size table, until it's read.
struct Src<'n> {
    b:         Option<Node<'n>>,
    path:      PathIdx,
    live_u64s: u16,
}

fn read_nodes<'n>(srcs: &[Src<'n>]) -> ArrayVec<Node<'n>, 3> {
    srcs.iter().map(|s| s.b.expect("a merge src not read")).collect()
}

/// @b's side toward @sib poisoned: @sib has another parent.
fn poison_side(b: Node<'_>, sib: Node<'_>) {
    let side = if sib.contents().data().max_key < b.contents().data().min_key { PREV_SIB } else { NEXT_SIB };
    set_sib_u64s(b, side, u16::MAX);
}

/*
 * Verify each pair of consecutive source nodes is contiguous: the
 * successor of prev's max_key must equal next's min_key. Anything
 * else is btree topology corruption and we bail before committing
 * anything.
 */
fn merge_topology_check(fs: &Fs, srcs: &[Node<'_>]) -> Result<(), BchError> {
    for w in srcs.windows(2) {
        let prev_end = w[0].contents().data().max_key;
        let next_start = w[1].contents().data().min_key;

        if prev_end.successor() == next_start {
            continue;
        }

        let mut msg = LogMsg::new(fs);
        prt!(msg, "btree node merge: end of prev node doesn't match start of next node\n");
        prt!(msg, "prev ends at   {prev_end}\n");
        prt!(msg, "next starts at {next_start}\n");
        return Err(fs.topology_error(&mut msg));
    }
    Ok(())
}

/*
 * Try to fetch @pivot's sibling on side @sib and push it onto @srcs:
 *   1. Skip if pivot is at the btree boundary on this side
 *   2. Skip if pivot->sib_u64s[sib] (cached estimate) exceeds the
 *      foreground merge threshold
 *   3. Get + traverse a path at the sibling's position. Try nofill first;
 *      on miss, look up the evicted-size hash table via the parent's
 *      now-locked iter. If we get an estimate, push with @b None and
 *      defer the real traverse until after the caller's size check.
 *      Otherwise pay for the real read.
 *   4. Skip if sibling has a different parent (only when @b is set —
 *      deferred srcs validate parent post-fill; the post-update_start
 *      recheck catches the rest).
 *
 * On boundary or different-parent skip, sets pivot->sib_u64s[sib] = U16_MAX
 * so future calls bail cheaply at step 2.
 *
 * Ok on push or any skip; Err a real error (trans restart from path
 * traverse), the path put before returning.
 */
fn merge_push_pos<'n>(trans: &BtreeTrans<'_>, paths: &mut Paths<'n>, srcs: &mut ArrayVec<Src<'n>, 3>,
                      level: usize, pivot_path: PathIdx, sib: usize) -> Result<(), BchError> {
    let fs = trans.fs();
    let pivot = paths.held_btree_node(pivot_path, level).expect("a merge's pivot, held by its path");
    let data = pivot.contents().data();

    if (sib == PREV_SIB && data.min_key == POS_MIN) ||
       (sib == NEXT_SIB && data.max_key == SPOS_MAX) {
        set_sib_u64s(pivot, sib, u16::MAX);
        return Ok(());
    }

    if pivot.contents().sib_u64s[sib].get() > fs.btree.foreground_merge_threshold {
        return Ok(());
    }

    let parent_b = parent(paths, pivot_path, level);
    let pos = if sib == PREV_SIB { data.min_key.predecessor() } else { data.max_key.successor() };
    let path = trans.path_get(paths, Btree(pivot.c.btree_id.into()), pos, level as u32 + 1, level as u32,
                              BtreeIterFlags::INTENT, crate::ret_ip!());

    match merge_sibling(trans, paths, path, level) {
        Ok(Some((b, live_u64s))) if b.is_none_or(|_| parent(paths, path, level) == parent_b) => {
            if b.is_some() {
                trans.path_set_should_be_locked(paths, path);
            }
            srcs.push(Src { b, path, live_u64s });
            Ok(())
        }
        ret => {
            trans.path_put(paths, path, true);
            if let Ok(Some(_)) = ret {
                set_sib_u64s(pivot, sib, u16::MAX);
            }
            ret.map(|_| ())
        }
    }
}

/// The node @path is at, traversed, with its live u64s - or with no node, an
/// estimate of them from the evicted-size table; None if its parent has no key
/// there.
fn merge_sibling<'n>(trans: &BtreeTrans<'_>, paths: &mut Paths<'n>, path: PathIdx,
                     level: usize) -> Result<Option<(Option<Node<'n>>, u16)>, BchError> {
    match trans.path_traverse(paths, path, BtreeIterFlags::NOFILL) {
        Err(e) if e.matches(bch_errcode::BCH_ERR_no_btree_node_nofill) => {
            /*
             * Nofill failure dropped all of the path's locks, but left
             * l[level + 1].b pointing at the parent. Relock the parent
             * so we can read its iter for the sibling's btree pointer
             * and check the evicted-size hash before paying for a real
             * read. If the relock fails (parent's lock_seq advanced) or
             * the hash misses, fall through to a real traverse.
             *
             * The sibling path's locks_want only covers @level, so the
             * level+1 lock we just acquired is outside its want range
             * and would trip bch2_btree_path_verify_locks. Drop it once
             * we've copied the sibling's btree_ptr off the parent.
             */
            if !trans.node_relock(paths, path, level + 1) {
                return Err(trans.restart(bch_errcode::BCH_ERR_transaction_restart_lock_node_reused));
            }

            let sib_parent = paths.held_btree_node(path, level + 1).expect("a sibling's parent, relocked");
            let hash = paths[path].l[level + 1].iter.peek_all().map(|i| {
                let keys = sib_parent.keys(sib_parent.contents());
                let mut u = c::bkey::new();
                btree_ptr_hash_val(keys.disassemble(i, &mut u))
            });

            trans.node_unlock(paths, path, level + 1);

            let Some(hash) = hash else { return Ok(None) };
            if let Some(est) = (hash != 0).then(|| trans.fs().btree.evicted_size.lookup(hash)).flatten() {
                return Ok(Some((None, est)));
            }
            trans.path_traverse(paths, path, BtreeIterFlags::empty())?;
        }
        ret => ret?,
    }

    let b = paths.held_btree_node(path, level).expect("a sibling, traversed");
    Ok(Some((Some(b), b.contents().nr.live_u64s)))
}

/// The srcs' u64s together: in the format they'd be packed into, if they've
/// all been read, and it - their estimates' sum if not.
fn merge_u64s_and_format(srcs: &[Src<'_>]) -> (usize, Option<c::bkey_format>) {
    let Some(nodes) = srcs.iter().map(|s| s.b).collect::<Option<ArrayVec<Node<'_>, 3>>>() else {
        return (srcs.iter().map(|s| usize::from(s.live_u64s)).sum(), None);
    };

    let mut s = c::bkey_format_state::new();
    s.add_pos(nodes[0].contents().data().min_key);
    for n in &nodes {
        n.format_add_keys(n.contents(), &mut s);
    }
    s.add_pos(nodes[nodes.len() - 1].contents().data().max_key);
    let new_f = s.done();

    let total_u64s = nodes.iter().map(|n| {
        let contents = n.contents();
        contents.nr.u64s_with_format(&contents.format, &new_f)
    }).sum();
    (total_u64s, Some(new_f))
}

fn merge_fail_reset_sib_u64s(fs: &Fs, paths: &Paths<'_>, b: Node<'_>, srcs: &[Src<'_>]) {
    let contents = b.contents();
    let hysteresis = merge_hysteresis(fs);

    for s in srcs {
        if s.b.is_some_and(|sb| ptr::eq(sb.node(), b.node())) {
            continue;
        }

        /*
         * For deferred srcs (b == None), use the path's pos to determine
         * which side it's on — pos was set to bpos_predecessor/successor of
         * the pivot's keyspace at path_get time, so it never overlaps.
         */
        let before = match s.b {
            Some(sb) => sb.contents().key.k.p < contents.key.k.p,
            None     => paths[s.path].pos < contents.data().min_key,
        };
        let mut sib_u64s = usize::from(contents.nr.live_u64s) + usize::from(s.live_u64s);
        if sib_u64s > hysteresis {
            sib_u64s -= (sib_u64s - hysteresis) / 2;
        }
        set_sib_u64s(b, if before { PREV_SIB } else { NEXT_SIB }, sib_u64s.min(usize::from(u16::MAX) - 1) as u16);
    }
}

/*
 * Decide how many dsts the merge should produce: with the srcs' total u64s,
 * and their format if they've all been read. None if the merge cannot fit
 * in fewer dsts than srcs.
 */
fn merge_fits(trans: &BtreeTrans<'_>, paths: &mut Paths<'_>, b: Node<'_>,
              srcs: &mut ArrayVec<Src<'_>, 3>) -> Option<(usize, usize, Option<c::bkey_format>)> {
    let fs = trans.fs();
    let (mut total_u64s, mut new_f) = merge_u64s_and_format(srcs);
    /*
     * Choose dst count: one node if everything packs into max, two nodes
     * (pack via pack_into_dsts) if 3 srcs overflow but fit in 2x max.
     * Anything else falls through to the bail check below.
     *
     * Strict less-than: bch2_btree_node_format_fits() and the write path
     * both treat btree_max_u64s as an open upper bound (the +8 varint slop
     * in btree_node_write tips a node packed at exact-max into overflow).
     */
    let mut nr_dsts = total_u64s.div_ceil(merge_higher_threshold(fs)).max(1);

    if nr_dsts >= srcs.len() && srcs.len() == 3 {
        let remove = if srcs[0].live_u64s > srcs[2].live_u64s { 0 } else { 2 };

        merge_fail_reset_sib_u64s(fs, paths, b, &srcs[remove..=remove]);
        let s = srcs.remove(remove);
        trans.path_put(paths, s.path, true);

        (total_u64s, new_f) = merge_u64s_and_format(srcs);
        nr_dsts = total_u64s.div_ceil(fs.btree_max_u64s() / 2).max(1);
    }

    if nr_dsts >= srcs.len() {
        merge_fail_reset_sib_u64s(fs, paths, b, srcs);
        return None;
    }
    Some((nr_dsts, total_u64s, new_f))
}

/*
 * The per-dst format and range the pack core will read, every src read.
 * None if the merge doesn't fit, or merge_layout found no candidate pivot
 * whose worst dst stays under split_thresh.
 */
fn merge_layouts(trans: &BtreeTrans<'_>, paths: &mut Paths<'_>, b: Node<'_>,
                 srcs: &mut ArrayVec<Src<'_>, 3>) -> Option<ArrayVec<DstLayout, 2>> {
    let (nr_dsts, total_u64s, new_f) = merge_fits(trans, paths, b, srcs)?;
    let nodes = read_nodes(srcs);
    let min_key = nodes[0].contents().data().min_key;
    let max_key = nodes[nodes.len() - 1].contents().data().max_key;

    if nr_dsts == 2 {
        if let Some(layout) = merge_layout(trans, &nodes) {
            return Some(layout.dsts(min_key, max_key).into_iter().collect());
        }

        /*
         * 2-way split is infeasible (no candidate keeps both sides
         * non-empty under split_thresh). Empty trailing srcs can force
         * total_u64s above HIGHER_THRESHOLD via format bloat from a
         * POS_MAX max_key, even though the actual key count fits in a
         * single node. If it does fit, fall through to a 1-dst merge
         * rather than abandon the consolidation.
         */
        if !b.fits_u64s(total_u64s) {
            merge_fail_reset_sib_u64s(trans.fs(), paths, b, srcs);
            return None;
        }
    }

    let format = new_f.expect("a merge's srcs read, and their format");
    Some([DstLayout { format, min_key, max_key }].into_iter().collect())
}

/// The node at @level of @path merged with its siblings, if together they're
/// small enough: into one node - or three into two. A merge that's done
/// counts in @merge_count.
pub(crate) fn merge(trans: &BtreeTrans<'_>, paths: &mut Paths<'_>, path: PathIdx, level: usize,
                    commit: CommitOpts, merge_count: Option<&mut u64>) -> Result<(), BchError> {
    let fs = trans.fs();
    let start_time = local_clock();
    let mut srcs = ArrayVec::new();

    let ret = merge_in(trans, paths, path, level, commit, &mut srcs);
    if matches!(ret, Ok(true)) {
        fs.times[c::bch_time_stats::btree_node_merge as usize].update(start_time);
        if let Some(n) = merge_count {
            *n += 1;
        }
    }

    trans.verify_locks(paths);

    let ret = match ret {
        Err(e) if e.matches(bch_errcode::BCH_ERR_journal_reclaim_would_deadlock) => Ok(()),
        ret => ret.map(|_| ()),
    };
    let ret = ret.and_then(|()| trans.relock_from(paths));

    for s in srcs {
        trans.path_put(paths, s.path, true);
    }
    ret
}

/// merge(), the srcs' paths it takes in @srcs, for merge() to put: whether it
/// merged.
fn merge_in<'n>(trans: &BtreeTrans<'_>, paths: &mut Paths<'n>, path: PathIdx, level: usize, commit: CommitOpts,
                srcs: &mut ArrayVec<Src<'n>, 3>) -> Result<bool, BchError> {
    let fs = trans.fs();

    trans.verify_not_unlocked_or_in_restart();
    assert!(paths[path].should_be_locked(), "merging from a path that needn't be locked");
    assert!(paths[path].locked(level), "merging from a path without its node locked");

    /*
     * Work around a deadlock caused by the btree write buffer not doing
     * merges and leaving tons of merges for us to do - we really don't need
     * to be doing merges at all from the interior update path, and if the
     * interior update path is generating too many new interior updates we
     * deadlock:
     */
    let watermark = commit.get_watermark();
    if watermark == c::bch_watermark::interior_updates {
        return Ok(false);
    }

    let commit = if watermark as u32 <= c::bch_watermark::reclaim as u32 {
        commit.watermark(Watermark::BTREE).flags(CommitFlags::JOURNAL_RECLAIM)
    } else {
        commit
    };

    let b: Node<'n> = paths.held_btree_node(path, level).expect("a merge's node, held by its path");
    let btree = Btree(b.c.btree_id.into());
    let data = b.contents().data();

    if data.min_key == POS_MIN ||
       (level == 0 && key_is_shard_boundary(fs, btree, data.min_key)) {
        set_sib_u64s(b, PREV_SIB, u16::MAX);
    }
    if data.max_key == SPOS_MAX ||
       (level == 0 && key_is_shard_boundary(fs, btree, data.max_key.successor())) {
        set_sib_u64s(b, NEXT_SIB, u16::MAX);
    }

    /*
     * Push srcs in left-to-right order so srcs is naturally sorted: prev
     * sibling first (if merging left), then caller's node, then next
     * sibling (if merging right). The caller's path takes an extra ref so
     * the cleanup can put it uniformly with helper-acquired paths.
     */
    merge_push_pos(trans, paths, srcs, level, path, PREV_SIB)?;

    paths.get_ll(path, true);
    srcs.push(Src { b: Some(b), path, live_u64s: b.contents().nr.live_u64s });

    merge_push_pos(trans, paths, srcs, level, path, NEXT_SIB)?;

    if srcs.len() == 1 {
        return Ok(false);
    }

    fs.event_inc_trace(events::btree_node_merge_attempt, |out| {
        let max = fs.btree_max_u64s();
        let mut total_u64s = 0;
        for s in srcs.iter() {
            match s.b {
                Some(sb) => prt!(out, "{}", sb.pos_to_text(fs)),
                None     => prt!(out, "(evicted node)"),
            }
            prt!(out, "\nlive u64s {} ({}% full)\n", s.live_u64s, usize::from(s.live_u64s) * 100 / max);
            total_u64s += usize::from(s.live_u64s);
        }

        let contents = b.contents();
        prt!(out, "Pivot sib_u64s {} {} threshold {}\n",
             contents.sib_u64s[PREV_SIB].get(), contents.sib_u64s[NEXT_SIB].get(), fs.btree.foreground_merge_threshold);
        prt!(out, "{}", b.pos_to_text(fs));

        let per_node = merge_higher_threshold(fs);
        prt!(out, "\ntotal_u64s {total_u64s} per-node max {per_node} nr_dsts {}\n",
             total_u64s.div_ceil(per_node).max(1));
    });

    if merge_fits(trans, paths, b, srcs).is_none() {
        return Ok(false);
    }

    /*
     * Estimate-based gate said the merge will fit — fill (or re-traverse)
     * any srcs whose path isn't currently should_be_locked. Deferred srcs
     * pushed without a real read fall here; so do any srcs whose locks
     * may have been dropped since push (a !should_be_locked path won't
     * get relocked by unlock/relock cycles).
     *
     * On parent-identity mismatch, poison that side's cached estimate
     * and drop the src — saves us the update_start cost that the
     * post-update_start recheck would otherwise discover.
     */
    let mut i = 0;
    while i < srcs.len() {
        let src_path = srcs[i].path;

        if paths[src_path].should_be_locked() {
            i += 1;
            continue;
        }

        trans.path_traverse(paths, src_path, BtreeIterFlags::empty())?;

        let sb = paths.held_btree_node(src_path, level).expect("a merge src, traversed");

        if parent(paths, src_path, level) != parent(paths, path, level) {
            poison_side(b, sb);

            let s = srcs.remove(i);
            trans.path_put(paths, s.path, true);
            continue;
        }

        srcs[i].b = Some(sb);
        srcs[i].live_u64s = sb.contents().nr.live_u64s;
        trans.path_set_should_be_locked(paths, src_path);
        i += 1;
    }

    /*
     * Post deferred-fill, every surviving src must have ->b set so that
     * merge_layouts() / merge_u64s_and_format() takes the precise
     * format-aware path. The estimate-only fallback (sum of live_u64s)
     * doesn't account for format growth on repack and would let an
     * infeasible merge through, blowing up later in pack_into_dsts or
     * sort_into.
     */
    let nodes = read_nodes(srcs);
    merge_topology_check(fs, &nodes)?;

    if srcs.len() == 1 {
        return Ok(false);
    }

    let Some(layouts) = merge_layouts(trans, paths, b, srcs) else { return Ok(false) };

    let nodes = read_nodes(srcs);
    let range = (nodes[0].contents().data().min_key, nodes[nodes.len() - 1].contents().data().max_key);
    let update = InteriorUpdate::start(trans, paths, path, level, StartOpts {
        split:  layouts.len() == 2,
        commit: commit.flags(CommitFlags::NO_ENOSPC),
        range:  Some(range),
        ..Default::default()
    })?;

    /*
     * update_start upgraded path's locks to cover parent nodes; re-read
     * parent and re-verify all srcs still share it. The earlier check in
     * merge_push_pos() was racy because parents weren't locked.
     *
     * On mismatch, identify which side the bad sibling is on by bpos
     * comparison and poison just that side's cached estimate.
     */
    let parent_b = parent(paths, path, level);
    for (s, &sb) in srcs.iter().zip(&nodes) {
        if s.path != path && parent(paths, s.path, level) != parent_b {
            poison_side(b, sb);
            return Ok(false);
        }
    }

    if let Err(e) = merge_locked(&update, paths, path, level, srcs, &layouts) {
        for s in srcs.iter().rev() {
            if paths[s.path].write_locked(level) {
                trans.node_unlock_write(paths, s.path, level);
            }
        }
        return Err(e);
    }

    update.done(paths);
    Ok(true)
}

fn merge_locked(update: &InteriorUpdate<'_>, paths: &mut Paths<'_>, path: PathIdx, level: usize,
                srcs: &[Src<'_>], layouts: &[DstLayout]) -> Result<(), BchError> {
    let trans = update.trans;
    let fs = trans.fs();

    for s in srcs {
        trans.node_lock_write(paths, s.path, level)?;
    }

    let nodes = read_nodes(srcs);

    /*
     * Allocate destination nodes: 1 for plain N->1, 2 for the 3->2 case,
     * each set up with the format and range merge_layouts() gave it.
     */
    let mut dsts: ArrayVec<NewNode<'_>, 2> = layouts.iter().map(|&l| alloc_dst(update, paths, &nodes, l)).collect();

    if let [n] = &mut dsts[..] {
        for s in &nodes {
            bch2_btree_sort_into(fs, n, s);
        }

        assert!(check_topology(trans, n.locked()).is_ok(), "a merged node's topology bad:\n{}", n.to_text(fs));
        n.contents().reset_sib_u64s();
    } else {
        let dsts: &mut [NewNode<'_>; 2] = dsts.as_mut_slice().try_into().expect("a merge into two nodes");
        pack_into_dsts(trans, &nodes, dsts);
    }

    /*
     * Diagnostic: each dst's packed content must fit in btree_buf_bytes
     * with room for the +8 varint slop in btree_node_write. Mirrors
     * bch2_btree_node_format_fits()'s strict-less-than check.
     */
    for n in &dsts {
        let u64s = n.contents().data().keys.u64s();
        assert!(n.fits_u64s(u64s), "a merged node of {u64s} u64s past its buffer:\n{}", n.to_text(fs));
    }

    for n in &dsts {
        bch2_btree_build_aux_trees(n);
        update.take_path(paths, n);
    }

    /*
     * Conceptually: every src becomes a delete, every dst becomes a new
     * key, sorted and deduped (a new key at the same .p as a delete
     * subsumes the delete). For N -> 1 the last src's .p equals the dst's
     * .p (the dst's max_key was set to the last src's max_key), so the
     * last src's delete is the only one that gets dropped.
     *
     * Both parent_keys (the in-memory parent update) and new_nodes (the
     * journal record) need each surviving delete; unlink() emits them
     * together.
     */
    let pos = |b: Node<'_>| b.contents().key.k.p;
    let (mut si, mut di) = (0, 0);
    while si < nodes.len() || di < dsts.len() {
        let src_p = nodes.get(si).map(|&s| pos(s));
        let dst_p = dsts.get(di).map(|n| pos(n.locked()));

        match (src_p, dst_p) {
            (Some(s), Some(d)) if s == d => {
                update.link(&dsts[di]);
                si += 1;
                di += 1;
            }
            (Some(s), d) if d.is_none_or(|d| s < d) => {
                update.unlink(s, level);
                si += 1;
            }
            _ => {
                update.link(&dsts[di]);
                di += 1;
            }
        }
    }

    trans.verify_paths(paths);

    update.insert_into_parent(paths, path, level)?;

    fs.event_inc_trace(events::btree_node_merge, |out| btree_node_op_log(out, fs, &nodes, &dsts));

    let old: ArrayVec<_, 3> = srcs.iter().map(|s| s.path).zip(nodes.iter().copied()).collect();
    update.replace(paths, &old, &dsts);

    trans.verify_paths(paths);
    Ok(())
}

/// # Safety
/// @merge_count is null, or a counter merges are counted in.
#[c_export]
pub unsafe fn __bch2_foreground_maybe_merge(trans: &BtreeTrans<'_>, #[lent(trans)] paths: &mut Paths<'_>,
                                            path: PathIdx, level: c_uint, flags: c::bch_trans_commit_flags,
                                            merge_count: *mut u64) -> c_int {
    ret_to_c(merge(trans, paths, path, level as usize, flags.into(), unsafe { merge_count.as_mut() }))
}

/// @path's leaf split - or compacted - so a key of @new_key_u64s fits, and
/// the nodes above it merged if they've gotten small.
pub(crate) fn split_leaf(trans: &BtreeTrans<'_>, paths: &mut Paths<'_>, path: PathIdx, new_key_u64s: u32,
                         commit: CommitOpts) -> Result<(), BchError> {
    let level = usize::from(paths[path].level());
    /* btree_split & merge may both cause paths array to be reallocated */
    let b = paths.held_btree_node(path, level).expect("a leaf its path holds");

    let update = InteriorUpdate::start(trans, paths, path, level,
                                       StartOpts { split: true, commit, new_key_u64s, ..Default::default() })?;

    trans.node_lock_write(paths, path, level)?;
    btree_split(&update, paths, path, b, false)?;
    update.done(paths);

    let mut l = level + 1;
    while paths[path].intent_locked(l) {
        foreground_maybe_merge(trans, paths, path, l, commit, None)?;
        l += 1;
    }
    Ok(())
}

pub(crate) fn increase_depth(trans: &BtreeTrans<'_>, paths: &mut Paths<'_>, path: PathIdx,
                             commit: CommitOpts) -> Result<(), BchError> {
    let fs = trans.fs();
    let b = fs.btree_id_root(paths[path].btree_id().into()).expect("a btree with a root");

    if b.is_fake() {
        return split_leaf(trans, paths, path, 0, commit);
    }

    let level = usize::from(b.c.level);
    let update = InteriorUpdate::start(trans, paths, path, level,
                                       StartOpts { split: true, commit, ..Default::default() })?;

    trans.node_lock_write(paths, path, level)?;
    let b = paths.held_btree_node(path, level).expect("the old root, write locked");

    let n = update.alloc_root(paths, path, level + 1);

    update.push_parent_key(b.contents().key_i());
    if let Err(e) = update.insert_parent_keys(paths, path, &n) {
        trans.node_unlock(paths, path, level + 1);
        return Err(e);
    }

    update.set_root(paths, path, &n);
    update.replace(paths, &[], &[n]);

    trans.node_unlock_write(paths, path, level);

    /*
     * Old root is no longer a root: clearing permanent makes it
     * eligible for reclaim again. The node stayed hashed and on
     * bc->list throughout its tenure as root (it was protected by
     * the flag, not by being off-list), so no list/hash work is
     * needed here.
     */
    {
        let _noio = MemallocFlags::noio();
        let _lists = fs.btree.cache.lock.lock();
        b.flags.set_permanent(false);
    }

    trans.verify_locks(paths);
    update.done(paths);
    Ok(())
}

/// @b, which @iter's path holds, replaced by a compacted copy - written to
/// @target, if not 0.
pub(crate) fn node_rewrite<'t>(trans: &'t BtreeTrans<'t>, paths: &mut Paths<'_>, iter: &mut BtreeIter<'t>,
                               b: &BtreeNode, target: u32, commit: CommitOpts,
                               write: c::bch_write_flags) -> Result<(), BchError> {
    assert!(!b.is_fake(), "rewriting a fake btree node");

    let ret = node_rewrite_in(trans, paths, iter, b,
                              StartOpts { target, commit: commit.flags(CommitFlags::NO_ENOSPC), write,
                                          ..Default::default() });
    trans.downgrade(paths);
    ret
}

fn node_rewrite_in<'t>(trans: &'t BtreeTrans<'t>, paths: &mut Paths<'_>, iter: &mut BtreeIter<'t>,
                       node: &BtreeNode, opts: StartOpts) -> Result<(), BchError> {
    let fs = trans.fs();
    let level = usize::from(node.c.level);

    let update = InteriorUpdate::start(trans, paths, iter.path, level, opts)?;

    trans.node_lock_write(paths, iter.path, level)?;
    let b = paths.held_btree_node(iter.path, level).expect("a rewrite's node, held by its path");
    ebug_assert!(ptr::eq(b.node(), node), "rewriting a node its path doesn't hold");

    let n = update.alloc_replacement(paths, &b);
    bch2_btree_build_aux_trees(&n);
    update.take_path(paths, &n);

    if paths[iter.path].locked_btree_node(level + 1).is_some() {
        update.link(&n);
        update.insert_into_parent(paths, iter.path, level)?;
    } else {
        update.set_root(paths, iter.path, &n);
    }

    fs.event_inc_trace(events::btree_node_rewrite, |out| crate::prt!(out, "{}", b.pos_to_text(fs)));

    update.replace(paths, &[(iter.path, b)], &[n]);
    update.done(paths);
    Ok(())
}
