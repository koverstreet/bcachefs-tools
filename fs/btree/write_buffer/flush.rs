// SPDX-License-Identifier: GPL-2.0

use super::keys::{Shard, WbKeyMut};
use super::{insert_checks, resize_gfp, wb_btree_to_btree, Buf, Flushing, FlushingGuard, IncGuard, WB_RESIZE_GFP};
use crate::accounting;
use crate::alloc::accounting_mem;
use crate::btree::async_ops::AsyncBtreeOp;
use crate::btree::bkey::{BkeySC, BKEY_U64S, POS_MIN};
use crate::btree::iter::{commit_do, is_restart, BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags, CommitOpts,
                         TransAttempt, UpdateTriggerFlags, Watermark};
use crate::c;
use crate::c::bch_fs_flags as Flag;
use crate::c::journal_flags as Journal;
use crate::debug::trace::events;
use crate::errcode::{bch_errcode, BchError};
use crate::format::{BkeyType, Btree};
use crate::util::vstructs::plain_bytes;
use crate::locking::MemallocFlags;
use crate::data_structures::sort::heapsort_by_lt;
use crate::util::alloc::KVVec;
use crate::util::kernel::{local_clock, num_online_cpus, race_fault};
use crate::util::par::{self, Backend};
use crate::util::projections::VolatileWriter;
use crate::util::time_stats::TimeStats;
use crate::{btree_trans, fs_fatal_error, prt};
use core::ptr;
use core::sync::atomic::fence;

/*
 * The refs sort as one 192 bit integer, hi:mi:lo - pos, then idx: a sub/sbb
 * chain, with no branches.
 */
fn wb_key_ref_lt(l: &c::wb_key_ref, r: &c::wb_key_ref) -> bool {
    let ((l_hi, l_mi, l_lo), (r_hi, r_mi, r_lo)) = (l.words(), r.words());
    let (_, lo_borrow) = l_lo.overflowing_sub(r_lo);
    let (mi, b1) = l_mi.overflowing_sub(r_mi);
    let (_, b2) = mi.overflowing_sub(lo_borrow as u64);
    let (hi, b3) = l_hi.overflowing_sub(r_hi);
    let (_, b4) = hi.overflowing_sub((b1 | b2) as u64);
    b3 | b4
}

/* Compare excluding idx, the low 32 bits: */
fn wb_key_eq(l: &c::wb_key_ref, r: &c::wb_key_ref) -> bool {
    let (l, r) = (l.words(), r.words());

    ((l.0 ^ r.0) |
     (l.1 ^ r.1) |
     ((l.2 >> 32) ^ (r.2 >> 32))) == 0
}

#[derive(Clone, Copy, Default)]
struct FlushCounters {
    fast:     usize,
    noop:     usize,
    slowpath: usize,
}

pub(super) fn move_keys_from_inc_to_flushing(wb: &c::bch_fs_btree_write_buffer, inc: &mut IncGuard<'_>,
                                             flushing: &mut FlushingGuard<'_>) {
    let fs = wb.fs();
    let j = &fs.journal;
    let (inc, inc_live) = inc.parts();
    let (Flushing { keys: flushing, sorted }, live) = flushing.parts();

    if inc.keys.is_empty() {
        return;
    }
    let inc = &mut inc.keys;

    wb.pin_add(Buf::Flushing, inc.seq(0));

    /* Best-effort resizes; may fail under memory pressure - see WB_RESIZE_GFP */
    let _ = flushing.resize(&live.keys, (1 << 20).min(flushing.len() + inc.len()), WB_RESIZE_GFP);
    let _ = resize_gfp(sorted, flushing.capacity(), WB_RESIZE_GFP);
    live.sorted.set(sorted.len(), sorted.capacity());

    /*
     * Each sorted entry references one key, and each key is at least
     * BKEY_U64s u64s, so this is a conservative bound on the number
     * of u64s we can put in flushing while still being able to sort them.
     */
    let sorted_can_address = sorted.capacity() * BKEY_U64S;
    let room = flushing.capacity().min(sorted_can_address);

    if flushing.is_empty() && sorted_can_address >= inc.len() {
        /* Fast path: flushing is empty, just swap the buffers */
        flushing.swap(&live.keys, inc, inc_live);
    } else if flushing.len() + inc.len() <= room {
        /* All of inc fits: bulk copy */
        flushing.append(&live.keys, inc, inc_live);
    } else {
        /* Partial copy: move as many keys as will fit */
        flushing.move_from(&live.keys, inc, inc_live, room);
    }

    /*
     * wb_pin_le() reads inc before flushing without taking locks; make sure
     * clearing inc cannot become visible before the flushing pin is visible.
     */
    fence(core::sync::atomic::Ordering::Release);

    if inc.is_empty() {
        wb.pin_drop(Buf::Inc);
    } else {
        wb.pin_update(Buf::Inc, inc.seq(0));
    }

    if j.flags.get(Journal::low_on_wb) && !fs.btree_write_buffer_must_wait() {
        j.set_watermark(&mut j.lock.lock());
    }

    assert!(sorted.capacity() * BKEY_U64S >= flushing.len());
}

#[inline(never)]
fn flush_one_slowpath<'a>(t: &TransAttempt<'a, 'a>, iter: &BtreeIter<'a>, wb: &mut WbKeyMut<'_>)
    -> Result<(), BchError>
{
    let trans = t.trans();

    trans.node_unlock_write(&mut trans.paths(), iter.path, 0);

    t.set_journal_seq_to_pin(wb.seq());

    t.update(iter, &t.bkey_copy(wb.k())?, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)?;
    t.commit(None, CommitOpts::new()
             .watermark(Watermark::RECLAIM)
             .flags(CommitFlags::NO_ENOSPC |
                    CommitFlags::NO_CHECK_RW |
                    CommitFlags::NO_JOURNAL_RES |
                    CommitFlags::NO_SKIP_NOOPS |
                    CommitFlags::JOURNAL_RECLAIM))
}

#[inline(always)]
fn flush_one<'a>(
    t:                      &TransAttempt<'a, 'a>,
    wbb:                    &c::bch_fs_btree_write_buffer,
    iter:                   &mut BtreeIter<'a>,
    wb:                     &mut WbKeyMut<'_>,
    write_locked:           &mut bool,
    accounting_accumulated: &mut bool,
    cnt:                    &mut FlushCounters,
) -> Result<(), BchError> {
    let trans = t.trans();
    let fs = trans.fs();

    ebug_assert!(wb.seq() != 0);
    ebug_assert!(wbb.pin(Buf::Flushing).seq() != 0);
    ebug_assert!(wbb.pin(Buf::Flushing).seq() <= wb.seq());

    iter.traverse(t)?;

    if !*accounting_accumulated && wb.k().k.key_type() == BkeyType::accounting {
        let mut u = c::bkey::new();
        let k = iter.path_peek_slot(t, &mut u);

        if k.k.key_type() == BkeyType::accounting {
            accounting::accumulate(wb.k_mut(), k);
            if accounting::is_zero(BkeySC::from(wb.k())) {
                accounting_mem::maybe_kill(fs, wb.k().k.p);
            }
        }
    }
    *accounting_accumulated = true;

    let mut paths = trans.paths();

    /*
     * We can't clone a path that has write locks: unshare it now, before
     * set_pos and traverse():
     */
    if paths[iter.path].ref_ > 1 {
        iter.path = trans.path_make_mut_clone(&mut paths, iter.path, true, crate::ret_ip!());
    }

    let path = &paths[iter.path];
    let level = path.level() as usize;
    let l = path.locked_btree_node(level).expect("a traversed path has its node");
    let mut old_u = c::bkey::new();
    let old = path.l[level].iter.peek_all()
        .map(|p| l.keys(l.contents()).disassemble(p, &mut old_u))
        .filter(|old| old.k.p == wb.k().k.p);

    if old.is_some_and(|old| old.and_val_eq(BkeySC::from(wb.k()))) {
        cnt.noop += 1;
        return Ok(());
    }

    let b = unsafe { &*ptr::from_ref(path.locked_btree_node(0).expect("a traversed path has its leaf").node()) };
    drop(paths);

    if !*write_locked {
        trans.node_lock_write(&mut trans.paths(), iter.path, 0)?;

        trans.node_prep_for_write(&mut trans.paths(), b);
        *write_locked = true;
    }

    if !b.insert_fits(unsafe { b.contents() }, usize::from(wb.k().k.u64s)) {
        *write_locked = false;
        return flush_one_slowpath(t, iter, wb);
    }

    ebug_assert!(wb.k().k.p == trans.paths()[iter.path].pos);

    let seq = wb.seq();
    unsafe { trans.insert_key_leaf(&mut trans.paths(), iter.path, wb.k_mut(), seq) };
    cnt.fast += 1;

    let contents = unsafe { b.contents() };
    if b.needs_merge(contents, fs, 0) {
        fs.async_btree_op(b, contents, AsyncBtreeOp::Merge);
    }

    Ok(())
}

/*
 * Update a btree with a write buffered key using the journal seq of the
 * original write buffer insert.
 *
 * It is not safe to rejournal the key once it has been inserted into the write
 * buffer because that may break recovery ordering. For example, the key may
 * have already been modified in the active write buffer in a seq that comes
 * before the current transaction. If we were to journal this key again and
 * crash, recovery would process updates in the wrong order.
 */
fn btree_write_buffered_insert(t: &TransAttempt<'_, '_>, btree: Btree, wb: &mut WbKeyMut<'_>)
    -> Result<(), BchError>
{
    let mut iter = BtreeIter::new(t, btree, wb.k().k.start_pos(), BtreeIterFlags::CACHED | BtreeIterFlags::INTENT);

    t.set_journal_seq_to_pin(wb.seq());

    iter.traverse(t)?;
    t.update(&iter, &t.bkey_copy(wb.k())?, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)
}

/*
 * Flush a contiguous slice of wb->sorted ([start, end)) via the fastpath.
 *
 * The slice is keyspace-disjoint by construction (sorted by btree, pos), so
 * multiple calls with non-overlapping ranges and independent btree_trans's
 * may safely run in parallel: the only shared writes are to
 * wb->flushing.keys.data[i->idx] for i in [start, end), which is per-slice.
 *
 * Same-pos coalesce-and-accumulate is done by the caller before this runs
 * (and only for accounting), so each slice is duplicate-free and sharding is
 * straightforward.
 */
fn flush_sorted_range(
    trans:                  &BtreeTrans<'_>,
    wb:                     &c::bch_fs_btree_write_buffer,
    mut shard:              Shard<'_>,
    accounting_replay_done: bool,
    cnt:                    &mut FlushCounters,
) -> Result<(), BchError> {
    let fs = trans.fs();
    let btree = wb_btree_to_btree(wb.wb_btree());
    let mut write_locked = false;
    let mut ret = Ok(());

    let mut t = trans.begin();

    let mut iter = BtreeIter::new(&t, btree, POS_MIN, BtreeIterFlags::INTENT | BtreeIterFlags::ALL_SNAPSHOTS);

    let range = shard.range();
    for i in range.clone() {
        for n in i + 1..(i + 4).min(range.end) {
            shard.prefetch(n);
        }

        let mut k = shard.key(i);

        ret = insert_checks(fs, btree, k.k());
        if ret.is_err() {
            break;
        }

        assert!(k.seq() != 0);

        if !accounting_replay_done &&
            k.k().k.key_type() == BkeyType::accounting {
            cnt.slowpath += 1;
            continue;
        }

        if write_locked {
            let mut paths = trans.paths();
            let leaf_end = paths[iter.path].locked_btree_node(0).expect("write locked").contents().key.k.p;

            if k.k().k.p > leaf_end {
                trans.node_unlock_write(&mut paths, iter.path, 0);
                write_locked = false;
            }
        }

        iter.set_pos(k.k().k.p);
        trans.paths()[iter.path].bits.set_preserve(false);

        let mut accounting_accumulated = false;
        let r = loop {
            if race_fault() {
                break fs.throw(bch_errcode::BCH_ERR_journal_reclaim_would_deadlock);
            }

            let r = flush_one(&t, wb, &mut iter, &mut k, &mut write_locked,
                              &mut accounting_accumulated, cnt);
            if !write_locked {
                t = trans.begin();
            }
            match r {
                Err(e) if is_restart(&e) => continue,
                r => break r,
            }
        };

        match r {
            Ok(()) => k.set_seq(0),
            Err(e) if e == bch_errcode::BCH_ERR_journal_reclaim_would_deadlock.into() => cnt.slowpath += 1,
            Err(e) => {
                ret = Err(e);
                break;
            }
        }
    }

    if write_locked {
        trans.node_unlock_write(&mut trans.paths(), iter.path, 0);
    }

    ret
}

/*
 * Sharded fastpath. Threshold and shard cap are deliberately conservative:
 * fork/join + per-shard btree_trans setup isn't free, and we need each
 * shard to do enough work to amortize that. Tune empirically.
 */
const WB_FLUSH_SHARD_MIN_KEYS: usize = 1 << 11;

fn flush_n_shards(n_keys: usize) -> usize {
    (n_keys / WB_FLUSH_SHARD_MIN_KEYS).clamp(1, num_online_cpus() as usize)
}

fn flush_sorted_sharded(
    trans:                  &BtreeTrans<'_>,
    wb:                     &c::bch_fs_btree_write_buffer,
    mut sorted:             Shard<'_>,
    shards_total:           &VolatileWriter<u64>,
    accounting_replay_done: bool,
    cnt:                    &mut FlushCounters,
) -> Result<(), BchError> {
    let fs = trans.fs();
    let n_shards = flush_n_shards(sorted.range().len());

    *shards_total.place() += n_shards as u64;

    let cnts = if n_shards > 1 { KVVec::from_elem(FlushCounters::default(), n_shards).ok() } else { None };

    if let Some(mut cnts) = cnts {
        let wb_idx = wb.idx.get() as usize;
        let mut spawn_failed = false;
        let ret = par::scope(Backend::Workqueue(&fs.btree.write_buffer_shard_wq), |sc| {
            for (shard, cnt) in sorted.split(n_shards).zip(cnts.iter_mut()) {
                let queued_time = local_clock();

                sc.spawn(format_args!("btree_write_buffer_flush_shard"), move || {
                    let trans = btree_trans!(fs);
                    let wb = &fs.btree.write_buffer[wb_idx];

                    fs.times[c::bch_time_stats::btree_write_buffer_flush_shard_sched_delay as usize]
                        .update(queued_time);

                    fs.times[c::bch_time_stats::btree_write_buffer_flush_shard_work as usize]
                        .time(|| flush_sorted_range(&trans, wb, shard, accounting_replay_done, cnt))
                }).inspect_err(|_| spawn_failed = true)?;
            }
            Ok(())
        });

        if !spawn_failed {
            for s in cnts.iter() {
                cnt.fast     += s.fast;
                cnt.noop     += s.noop;
                cnt.slowpath += s.slowpath;
            }
            return ret;
        }
    }

    /* One shard, or OOM: the single-shard inline path. */
    flush_sorted_range(trans, wb, sorted, accounting_replay_done, cnt)
}

pub(super) fn flush_locked(trans: &BtreeTrans<'_>, wb: &c::bch_fs_btree_write_buffer, g: &mut FlushingGuard<'_>,
                           caller: c::wb_flush_caller) -> Result<(), BchError> {
    let fs = trans.fs();
    let j = &fs.journal;
    let btree = wb_btree_to_btree(wb.wb_btree());
    let mut cnt = FlushCounters::default();
    let mut could_not_insert = 0;
    let accounting_replay_done = fs.flags.get(Flag::accounting_replay_done);

    /*
     * The flush allocates under the write buffer locks (flush shards, key
     * array resizes), and with swap on bcachefs those locks are on the
     * reclaim path - all of flush runs NOIO so allocations can't recurse
     * into the IO this flush is needed to complete:
     */
    let _noio = MemallocFlags::noio();

    j.error()?;

    if g.keys.is_empty() {
        move_keys_from_inc_to_flushing(wb, &mut wb.inc.lock(), g);
    } else if let Some(mut inc) = wb.inc.try_lock() {
        move_keys_from_inc_to_flushing(wb, &mut inc, g);
    }

    if g.keys.is_empty() {
        return Ok(());
    }

    let start_time = local_clock();
    let nr_flushing = g.keys.len();

    let (Flushing { keys, sorted }, live) = g.parts();

    assert!(keys.idxs().all(|i| keys.seq(i) <= fs.journal_cur_seq()));

    sorted.clear();
    for i in keys.idxs() {
        let mut r = c::wb_key_ref::default();
        let p = keys.key(i).k.p;
        r.key.pos_idx.idx = i as u32;
        r.key.pos_idx.pos = plain_bytes(&p).try_into().expect("a bpos's bytes");

        if sorted.push_within_capacity(r).is_err() {
            panic!("Overflowed wb->sorted at {}, flushing size {}", sorted.len(), keys.len());
        }
    }

    /*
     * We first sort so that we can detect and skip redundant updates, and
     * then we attempt to flush in sorted btree order, as this is most
     * efficient.
     *
     * However, since we're not flushing in the order they appear in the
     * journal we won't be able to drop our journal pin until everything is
     * flushed - which means this could deadlock the journal if we weren't
     * passing BCH_TRANS_COMMIT_journal_reclaim. This causes the update to fail
     * if it would block taking a journal reservation.
     *
     * If that happens, simply skip the key so we can optimistically insert
     * as many keys as possible in the fast path.
     */
    heapsort_by_lt(sorted, wb_key_ref_lt);

    /*
     * Pre-flush dedup: collapse adjacent same-pos entries.
     *
     * After wb_sort, same-pos keys sit adjacent in wb->sorted, ordered
     * by idx (which is journal-insertion order: lower idx = earlier
     * write). For all btrees we drop the older entry in each same-pos
     * run and keep the newest. For accounting, where each entry is a
     * delta, we first accumulate the older value into the newer one,
     * yielding a single combined delta per pos.
     *
     * The dropped entry's journal_seq must be zeroed: the slowpath and
     * could_not_insert compaction iterate wb->flushing.keys directly
     * (not wb->sorted), and retain anything with non-zero journal_seq.
     * If we leave the dropped accounting entry alive in flushing, the
     * next flush's dedup would re-accumulate the already-accumulated
     * value into the survivor, multiplying the delta.
     *
     * This must run single-threaded before the sharded fastpath: the
     * partitioner splits wb->sorted by index, so a same-pos run
     * straddling a shard seam would otherwise be flushed in
     * non-deterministic order — an older write could clobber a newer
     * one. Collapsing same-pos runs here removes the constraint
     * entirely; shards see a slice with at most one entry per pos and
     * don't need to depend on sort-order semantics.
     */
    sorted.dedup_by(|newer, older| {
        if !wb_key_eq(newer, older) {
            return false;
        }
        let (mut k, mut n) = keys.pair_mut(older.idx(), newer.idx());

        /* Accounting: accumulate older delta into newer entry. */
        if k.k().k.key_type() == BkeyType::accounting &&
            n.k().k.key_type() == BkeyType::accounting {
            accounting::accumulate(n.k_mut(), BkeySC::from(k.k()));
            n.set_seq(n.seq().min(k.seq()));
        }
        /* Drop older entry; newer wins. */
        k.set_seq(0);
        *older = *newer;
        true
    });
    live.sorted.set(sorted.len(), sorted.capacity());

    let ret = 'err: {
        let s = unsafe { Shard::new(keys, sorted) };
        if let Err(e) = flush_sorted_sharded(trans, wb, s, &live.nr_shards_total, accounting_replay_done, &mut cnt) {
            break 'err Err(e);
        }

        if cnt.slowpath != 0 {
            assert!(keys.idxs().all(|i| keys.seq(i) <= fs.journal_cur_seq()));

            /*
             * Flush in the order they were present in the journal, so that
             * we can release journal pins:
             * The fastpath zapped the seq of keys that were successfully flushed so
             * we can skip those here.
             */
            fs.event_inc_trace(events::write_buffer_flush_slowpath, |out| {
                prt!(out, "{}/{}", cnt.slowpath, keys.len())
            });

            heapsort_by_lt(sorted, |l, r| keys.seq(l.idx()) < keys.seq(r.idx()));

            for r in sorted.iter() {
                let mut k = keys.get_mut(r.idx());
                if k.seq() == 0 {
                    continue;
                }

                assert!(k.seq() <= fs.journal_cur_seq());

                if !accounting_replay_done &&
                    k.k().k.key_type() == BkeyType::accounting {
                    could_not_insert += 1;
                    continue;
                }

                if could_not_insert == 0 {
                    wb.pin_update(Buf::Flushing, k.seq());
                }

                let ret = commit_do(trans, None,
                                    CommitOpts::new()
                                    .watermark(Watermark::RECLAIM)
                                    .flags(CommitFlags::JOURNAL_RECLAIM |
                                           CommitFlags::NO_CHECK_RW |
                                           CommitFlags::NO_ENOSPC |
                                           CommitFlags::NO_JOURNAL_RES),
                                    |t| btree_write_buffered_insert(t, btree, &mut k));
                if ret.is_err() {
                    break 'err ret;
                }

                k.set_seq(0);
            }

            /*
             * If journal replay hasn't finished with accounting keys we
             * can't flush accounting keys at all - condense them and leave
             * them for next time.
             *
             * Q: Can the write buffer overflow?
             * A Shouldn't be any actual risk. It's just new accounting
             * updates that the write buffer can't flush, and those are only
             * going to be generated by interior btree node updates as
             * journal replay has to split/rewrite nodes to make room for
             * its updates.
             *
             * And for those new acounting updates, updates to the same
             * counters get accumulated as they're flushed from the journal
             * to the write buffer - see the patch for eytzingcer tree
             * accumulated. So we could only overflow if the number of
             * distinct counters touched somehow was very large.
             */
            if could_not_insert != 0 {
                keys.retain_unflushed(&live.keys);
            }
        }
        Ok(())
    };

    if ret.is_err() || could_not_insert == 0 {
        wb.pin_drop(Buf::Flushing);
        keys.clear(&live.keys);
    }

    if let Err(e) = ret {
        fs_fatal_error!(fs, "{}", e.msg());
    }

    fs.times[c::bch_time_stats::btree_write_buffer_flush as usize].update(start_time);

    *live.nr_flushes.place()                     += 1;
    *live.nr_flushes_caller[caller as usize].place() += 1;
    *live.nr_keys_flushed.place()                += nr_flushing as u64;
    *live.nr_keys_fast.place()                   += cnt.fast as u64;
    *live.nr_keys_slowpath.place()               += cnt.slowpath as u64;

    fs.event_inc_trace(events::write_buffer_flush, |out| {
        prt!(out, "flushed {} fast {} noop {}", nr_flushing, cnt.fast, cnt.noop)
    });

    if j.flags.get(Journal::low_on_wb) &&
        !fs.btree_write_buffer_must_wait() {
        j.set_watermark(&mut j.lock.lock());
    }

    ret
}
