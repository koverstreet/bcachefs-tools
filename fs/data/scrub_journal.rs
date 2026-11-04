// SPDX-License-Identifier: GPL-2.0

//! The journal scrub: at recovery, what the newest journal entries point to
//! read back, to catch a device that didn't honor flush/FUA. It runs with
//! journal keys frozen - nothing may commit - so what it finds is queued, and
//! repaired or recorded as lost once we're rw and any rewind has happened.

use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{BkeyI, BkeySC};
use crate::btree::bkey_buf::BtreeKeyBuf;
use crate::btree::iter::{is_restart, lockrestart_do, BtreeIter, BtreeIterFlags, BtreeNodeIter, CommitFlags,
                         TransAttempt};
use crate::btree::read::{read_replicas, ReplicaRead, Replicas};
use crate::c;
use crate::data::extents::{BkeyVExtents, Extents};
use crate::data::moving::{key_sectors, MoveStats, Moving, MovingCtxt};
use crate::data::ptrs::{extents_match, BkeyVPtrs, Ptrs};
use crate::data::update_index::remap_bits;
use crate::errcode::BchError;
use crate::fs::{writepoint_ptr, Fs};
use crate::init::damage;
use crate::init::error::id;
use crate::init::recovery::btree_lost_data;
use crate::inode_opts::{get_fs, get_io_opts};
use crate::sb::counters::events;
use crate::util::alloc::KVVec;
use crate::util::log::LogMsg;
use crate::locking::Locked;
use crate::util::gather::{Fold, Gather, GatherRef};
use crate::util::backref::BackRef;
use crate::util::task::{waiter, Spawn, TaskBox};
use crate::{bch_err, bch_err_ratelimited, bch_info, new_lock, prt};
use core::ffi::c_int;
use cstruct_macros::c_extern;
use pin_init::{stack_pin_init, PinInit};
use crate::format::Btree;

c_extern! {
    fn bch2_data_read_sb_err(err: c_int) -> c::bch_sb_error_id;
}

/// What the journal scrub found wrong with an extent, for do_repairs() once
/// we're rw: the scrub runs with journal keys frozen, so it can neither repair
/// nor record damage itself.
///
/// @bad_devs is a mask of pointers by their position in @key, like
/// ptrs_io_error, not of device indices. @read_err is set when reading them
/// failed outright, rather than a replica being found bad on a read that
/// succeeded.
pub struct Repair {
    key:      BtreeKeyBuf,
    bad_devs: u32,
    read_err: c_int,
}

/// Queued for do_repairs(): see Repair. What's bad, for the scrub's Gather.
pub fn queue(fs: &Fs, btree: Btree, level: u32, k: BkeySC<'_>, bad_devs: u32, read_err: c_int) -> Bad {
    let r = Repair { key: BtreeKeyBuf::new(btree, level, k), bad_devs, read_err };
    let repair = fs.scrub_journal_repairs.lock().push(r).is_ok();
    if !repair {
        bch_err!(fs, "journal scrub: no memory to queue a repair, not repairing:\n{}", k.to_text(fs));
    }

    /* uncorrected: every replica failed, so only a rewind fixes it */
    Bad { lost: read_err != 0, repair, devs: mask_devs(fs, k, bad_devs), ..Default::default() }
}

/// The devices of @k's pointers in @mask, a mask of them by position.
fn mask_devs(fs: &Fs, k: BkeySC<'_>, mask: u32) -> c::DevsMask {
    let mut devs = c::DevsMask::default();
    let k = k.arm::<Ptrs>().expect("a key with pointers");
    for (_, p) in k.ptrs(fs).enumerate().filter(|(i, _)| mask & (1 << i) != 0) {
        devs.set(p.dev().into());
    }
    devs
}

// ── The scrub ────────────────────────────────────────────────────────────

/// What the scrub found wrong, and on which devices: an extent's update's, a
/// node's task's, or a flush range's, gathered from those.
#[derive(Default)]
pub struct Bad {
    /// Data with no good replica: only a rewind fixes it
    lost:   bool,
    /// Bad replicas, queued for repair from good ones
    repair: bool,
    /// Btree nodes with no good replica: no rewind fixes those - see Node
    nodes:  u32,
    btrees: u64,
    devs:   c::DevsMask,
}

impl Bad {
    fn add(&mut self, o: Self) {
        self.lost |= o.lost;
        self.repair |= o.repair;
        self.nodes += o.nodes;
        self.btrees |= o.btrees;
        self.devs.or(&o.devs);
    }
}

impl Fold for Bad {
    type Item = Self;
    type Acc = Locked<c::spinlock_t, Self>;

    fn acc() -> impl PinInit<Self::Acc> {
        new_lock!(Self::default())
    }

    fn fold(acc: &Self::Acc, v: Self) {
        acc.lock().add(v);
    }

    fn result(acc: &Self::Acc) -> Self {
        core::mem::take(&mut *acc.lock())
    }
}

/// A ref on a flush range's Gather: what an extent's update or a node's task
/// finds is put to it.
pub type BadRef = GatherRef<Bad, waiter::Sync>;

/*
 * A btree node the journal scrub found a pointer to in the window.
 *
 * Extents get their verdict from the read path: a replica that fails is
 * retried off the others, so "uncorrected" already means no replica was good.
 * Node scrub has no retry - each replica is checked on its own - so every
 * replica is checked here, and the verdict made once they've all reported.
 * Some good: the node is rewritten from one once we're rw. None good: a rewind
 * won't help - it only rolls back leaf keys, interior pointers stay (see
 * bch2_journal_keys_sort()), so the node would still be reached. The btree is
 * flagged as having lost data instead, which gets topology repair run before
 * journal replay: found by replay, it would be too late - we're rw by then.
 */

/// The node @key points to, its replicas on the devices that are there read,
/// and the verdict made: what's bad queued for repair, or the node lost if
/// every one read is.
async fn node_verdict(fs: BackRef<Fs>, key: BtreeKeyBuf, mut reads: Replicas<ReplicaRead<'static>>) -> Bad {
    let fs = &*fs;
    let read = read_replicas(&mut reads).await;

    let (mut checked, mut bad) = (0, 0);
    for (r, read) in reads.iter().zip(read) {
        checked |= r.ptr_bit;
        if !r.good(read, &key, "") {
            bad |= r.ptr_bit;
        }
    }

    if bad != 0 && bad == checked {
        /* just the devices, for reporting: uncorrected sectors mean a rewind */
        Bad { nodes: 1, btrees: 1 << key.btree.0, devs: mask_devs(fs, key.key(), bad), ..Default::default() }
    } else if bad != 0 {
        queue(fs, key.btree, key.level, key.key(), bad, 0)
    } else {
        Bad::default()
    }
}

/// The node @k points to, at @level of @btree, its replicas on the devices
/// that are there read back - in a task whose verdict @found gathers.
///
/// On read_complete_wq, not the moving context's run queue, which runs only
/// in its waits: the replicas' buffers can be the bounce pool's, which this
/// thread may wait on for the next node - and a task the thread runs would
/// never give them back.
fn scrub_node(fs: &Fs, found: &Gather<Bad, waiter::Sync>, btree: Btree, level: u32, k: BkeySC<'_>)
    -> Result<(), BchError>
{
    // A v1 pointer carries nothing to check a replica against
    if k.arm::<c::BtreePtrV2>().is_none() {
        return Ok(());
    }

    // the task holds it: @found's wait drains the task
    let fs_ref = unsafe { BackRef::new(fs) };
    let fs = unsafe { fs_ref.get() };

    let mut reads = Replicas::new();
    let mut ret = Ok(());

    let kp = k.arm::<Ptrs>().expect("a btree node pointer has pointers");
    for dev in kp.ptrs(fs).map(|p| p.dev().into()) {
        /* same as extents: an absent device tells us nothing */
        if !fs.dev_idx_is_online(dev) {
            continue;
        }
        match ReplicaRead::new(fs, k, dev) {
            Ok(Some(r)) => reads.push(r),
            Ok(None)    => ret = ret.and(fs.throw(c::bch_errcode::BCH_ERR_no_device_to_read_from)),
            Err(e)      => ret = ret.and(Err(e)),
        }
    }

    if !reads.is_empty() {
        let (key, found) = (BtreeKeyBuf::new(btree, level, k), found.get());
        let task = TaskBox::new(Spawn::Queue(&fs.btree.read_complete_wq),
                                async move { found.put(node_verdict(fs_ref, key, reads).await) });
        ret = ret.and(task.map(TaskBox::start));
    }
    ret
}

/// The extent @k, at @level of @btree, its replica on @dev read back -
/// asynchronously, as a move: a bad one is queued for repair by the read path,
/// and what's bad put to @found.
fn scrub_extent(m: &Moving<'_>, found: &Gather<Bad, waiter::Sync>, io_opts: &c::bch_inode_opts, btree: Btree,
                level: u32, k: BkeySC<'_>, dev: u32) -> Result<(), BchError> {
    let data_opts = c::data_update_opts {
        type_:      c::bch_data_update_types::BCH_DATA_UPDATE_scrub_no_repair,
        read_flags: c::bch_read_flags::BCH_READ_hard_require_read_device,
        read_dev:   dev,
        ..Default::default()
    };

    lockrestart_do(m.trans(), |t| {
        let mut iter = BtreeNodeIter::new(t, btree, k.k.p, 0, level, BtreeIterFlags::ALL_SNAPSHOTS);
        let k2 = iter.iter().peek_slot(t)?.expect("a slot always has a key");
        assert!(k2.and_val_eq(k), "the journal's key is the btree's: journal keys are frozen");

        let ret = m.move_extent(t, None, io_opts, &data_opts, iter.iter(), level, k, Some(found.get()));
        if !matches!(&ret, Err(e) if is_restart(e)) {
            m.stats().walked(iter.iter(), key_sectors(t.fs(), k));
        }
        ret
    })
}

/// The seqs of the flush entries in the scrub's window - back from the
/// newest, as far as scrub_journal_max_rewind_secs - oldest first.
fn flush_window(fs: &Fs, jk: &c::journal_keys) -> Result<KVVec<u64>, BchError> {
    let flushes_of = || jk.entries().iter().filter(|i| !i.ignored() && !i.jset().no_flush());

    /*
     * The window is measured back from the newest flush entry, so we need
     * that before we can decide what falls inside it - the entries run
     * oldest-first, and the scrub loop requires @flushes in ascending seq
     * order, so take two passes rather than reversing.
     */
    let newest_time = flushes_of().map(|i| i.datetime()).max().unwrap_or(0);
    let max_rewind_secs = u64::from(fs.opts().scrub_journal_max_rewind_secs.get());

    let mut flushes = KVVec::new();
    for i in flushes_of() {
        /*
         * Entries with no datetime entry (t == 0) are always included:
         * we can't tell how old they are, and excluding them would
         * silently shrink the scrub window.
         */
        let t = i.datetime();
        if t != 0 && newest_time != 0 && t + max_rewind_secs < newest_time {
            continue;
        }
        flushes.push(i.jset().seq.get())?;
    }
    Ok(flushes)
}

/// What the journal entries in seq range (@start, @end] point to, read back:
/// None if it all was good.
fn scrub_range(m: &Moving<'_>, jk: &c::journal_keys, start: u64, end: u64) -> Option<Bad> {
    let fs = m.trans().fs();
    let io_opts = get_fs(fs, false);

    fs.event_inc_trace(events::journal_scrub_range, |out| prt!(out, "seq {}-{end}", start + 1));

    stack_pin_init!(let found = Gather::<Bad, waiter::Sync>::new());
    let mut move_ret = Ok(());

    for jkey in jk.keys().iter() {
        let Some(seq) = fs.journal_key_seq(jkey) else { continue };
        if seq <= start || seq > end {
            continue;
        }

        let k = BkeySC::from(unsafe { &*BkeyI::from_raw(jk.key_k(jkey)) });
        let Some(kp) = k.arm::<Ptrs>() else { continue };
        let (btree, level) = (jkey.btree_id(), u32::from(jkey.level()));

        /*
         * A pointer to a node checks the whole node, bsets
         * appended since included. New roots are btree_root
         * entries, not keys, so they're not seen here - they
         * don't need to be only because read_btree_roots() reads
         * every root before we run. If that ever changes, they
         * need checking here too.
         */
        if k.k.is_btree_ptr() {
            if let Err(e) = scrub_node(fs, &found, btree, level, k) {
                move_ret = Err(e);
            }
            continue;
        }

        for ptr in kp.ptrs(fs) {
            /*
             * A device that isn't there tells us nothing
             * about whether it honoured flush/FUA - don't
             * let it read as a checksum error and rewind.
             */
            let dev = ptr.dev().into();
            if !fs.dev_idx_is_online(dev) {
                continue;
            }
            if let Err(e) = scrub_extent(m, &found, &io_opts, btree, level, k, dev) {
                move_ret = Err(e);
            }
        }
    }

    // flush_all() first: the extents' updates run only in its waits
    m.flush_all();
    let bad = found.wait_sync();

    if let Err(e) = move_ret {
        bch_err!(fs, "journal scrub: move error {e} in flush range seq {}-{end}", start + 1);
    }

    (bad.lost || bad.repair || bad.nodes != 0).then_some(bad)
}

/// What range (@start, @end]'s scrub found, said, and counted: in the
/// superblock's errors, and its devices' flush errors.
fn report_range(fs: &Fs, start: u64, end: u64, bad: &Bad) {
    /* a lost node is counted when recovery reads it */
    if bad.lost {
        fs.sb_error_count(id::device_bad_flush_forced_rewind);
    } else if bad.repair {
        fs.sb_error_count(id::device_bad_flush_repaired_from_replica);
    }

    let what = match (bad.lost, bad.repair) {
        (true, _)     => "data with no good replica",
        (false, true) => "bad replicas, to repair from good ones",
        _             => "",
    };
    let nodes = match (bad.nodes != 0, what.is_empty()) {
        (false, _)    => "",
        (true, true)  => "btree nodes with no good replica",
        (true, false) => ", btree nodes with no good replica",
    };

    let mut msg = LogMsg::new(fs);
    prt!(&mut msg, "journal scrub: {what}{nodes} in flush range seq {}-{end}, \
                    device(s) not honoring flush/FUA:", start + 1);
    for i in bad.devs.iter() {
        match fs.dev_get(i as u32) {
            Some(ca) => prt!(&mut msg, " {}", &*ca),
            None     => prt!(&mut msg, " dev{i}"),
        }
    }

    let mut w = fs.sb_write();
    for i in bad.devs.iter() {
        if let Some(m) = w.member_mut(i as u32) {
            m.flush_errors.set(m.flush_errors.get() + 1);
        }
    }
    w.dirty();
}

/// The seq to rewind the journal to, if the scrub found a device didn't
/// honor flush/FUA: 0 if not.
pub fn scrub(fs: &Fs) -> Result<u64, BchError> {
    let jk_ref = fs.journal_keys_get().expect("the journal scrub runs before recovery is done with the journal keys");
    let jk = jk_ref.value();

    let flushes = flush_window(fs, jk)?;
    fs.event_inc_trace(events::journal_scrub, |out| prt!(out, "{} flush entries in the window", flushes.len()));

    if flushes.is_empty() {
        bch_info!(fs, "journal scrub: no journal flushes found");
        return Ok(0);
    }

    /* a range is the entries between two flushes */
    bch_info!(fs, "journal scrub: checking {} flush ranges", flushes.len() - 1);

    let mut found = Bad::default();
    let stats = MoveStats::default();
    stats.reset();

    let ctxt = MovingCtxt::new(fs, &stats, c::write_point_specifier::default(), false)?;

    /*
     * Scrub one flush range at a time, newest first. Within each range,
     * fire off all reads asynchronously, then flush once to check for
     * errors. Walk backwards until two ranges in a row read clean; rewind
     * to the start of the oldest range with data no replica could be read
     * back for - everything newer is tainted. A bad replica read back from
     * a good one is repaired from it instead, once we're rw.
     *
     * Stop if we've gone further back than scrub_journal_max_rewind_secs seconds.
     */
    let (mut rewind_seq, mut nr_good) = (0, 0);

    for f in (1..flushes.len()).rev() {
        let (start, end) = (flushes[f - 1], flushes[f]);

        match scrub_range(&ctxt, jk, start, end) {
            Some(bad) => {
                report_range(fs, start, end, &bad);
                if bad.lost {
                    rewind_seq = start;
                }
                found.add(bad);
                /* Not a good range: keep looking further back for lost data */
                nr_good = 0;
            }
            None => {
                nr_good += 1;
                /* Two consecutive good ranges — safe to stop */
                if nr_good >= 2 {
                    break;
                }
            }
        }
    }

    if found.btrees != 0 {
        let nr = found.nodes;
        fs.event_inc_trace(events::journal_scrub_lost_btree_nodes, |out| prt!(out, "{nr} btree nodes"));

        let mut msg = LogMsg::new(fs);
        prt!(&mut msg, "journal scrub: {nr} btree nodes with no good replica, \
                        repairing topology before journal replay\n");

        for btree in (0..64).filter(|b| found.btrees & (1 << b) != 0) {
            btree_lost_data(fs, &mut msg, Btree(btree))?;
        }
    }

    Ok(rewind_seq)
}

// ── The repairs, once we're rw ───────────────────────────────────────────

enum Outcome {
    Unchanged,
    Repaired,
    Rewound,
    Lost,
}

fn repair_one(m: &Moving<'_>, t: &TransAttempt<'_, '_>, r: &Repair) -> Result<Outcome, BchError> {
    let fs = t.fs();
    let (btree, level, stashed) = (r.key.btree, r.key.level, r.key.key());

    /*
     * A btree node with a bad replica: rewrite it from a good one - if the
     * pointer we found is still the node's. If not, it's been replaced
     * since, and there's nothing left to repair.
     */
    if level != 0 {
        return match t.btree_node_rewrite_key(btree, level - 1, stashed, CommitFlags::empty()) {
            Ok(())                         => Ok(Outcome::Repaired),
            Err(e) if e.matches(c::ENOENT) => Ok(Outcome::Unchanged),
            Err(e)                         => Err(e),
        };
    }

    let mut iter = BtreeIter::new(t, btree, stashed.k.start_pos(), BtreeIterFlags::INTENT);
    let k = t.bkey_reassemble(iter.peek_slot(t)?.expect("a slot always has a key"))?;
    let k = k.as_sc();

    /*
     * Gone: the rewind took it, along with the write it came from. If that
     * write was one we couldn't read back, the file lost it - say so.
     */
    if !extents_match(fs, k, stashed) {
        if r.read_err == 0 {
            return Ok(Outcome::Unchanged);
        }

        damage::record_key_if_inode(t, btree, stashed.k.p, id::device_bad_flush_forced_rewind)?;
        return t.commit(None, CommitFlags::NO_ENOSPC).map(|()| Outcome::Rewound);
    }

    /*
     * Trim both keys to the overlapping range before remapping,
     * same as data_update_index_update_key does.
     */
    let mut trimmed = t.bkey_reassemble(k)?;
    if let Some(mut e) = trimmed.as_mut().arm::<Extents>() {
        e.cut_front_in_place(fs, stashed.k.start_pos());
        e.cut_back_in_place(stashed.k.p);
    }

    let bad_devs = u32::from(remap_bits(fs, stashed, r.bad_devs, trimmed.as_sc()));
    if bad_devs == 0 {
        return Ok(Outcome::Unchanged);
    }

    /* No good replica, and no rewind took it: that's plain data loss */
    let nr_ptrs = trimmed.as_sc().arm::<Ptrs>().map_or(0, |k| k.ptrs(fs).count());
    let all_ptrs = (1u32 << nr_ptrs) - 1;

    if r.read_err != 0 && bad_devs == all_ptrs {
        let err = unsafe { bch2_data_read_sb_err(r.read_err) };
        damage::record_key(t, btree, stashed.k.p, err)?;
        return t.commit(None, CommitFlags::NO_ENOSPC).map(|()| Outcome::Lost);
    }

    /* Repair it from whatever replica is good */
    let io_opts = get_io_opts(t, None, k)?;

    let data_opts = c::data_update_opts {
        type_:         c::bch_data_update_types::BCH_DATA_UPDATE_self_heal,
        target:        io_opts.background_target.into(),
        ptrs_io_error: bad_devs as u16,
        ptrs_kill:     bad_devs as u16,
        ..Default::default()
    };

    let ret = m.move_extent(t, None, &io_opts, &data_opts, &iter, 0, k, None);
    if !matches!(&ret, Err(e) if is_restart(e)) {
        m.stats().walked(&iter, key_sectors(fs, k));
    }
    ret.map(|()| Outcome::Repaired)
}

/// What the scrub queued, repaired - or recorded as lost - now that we're rw.
pub fn do_repairs(fs: &Fs) {
    let repairs = core::mem::take(&mut *fs.scrub_journal_repairs.lock());
    if repairs.is_empty() {
        return;
    }

    let stats = MoveStats::default();
    stats.reset();

    let Ok(ctxt) = MovingCtxt::new(fs, &stats, writepoint_ptr(&fs.allocator.reconcile_write_point), true) else {
        bch_err!(fs, "journal scrub: no memory for a moving context, not repairing {} extents/btree nodes",
                 repairs.len());
        return;
    };

    let (mut repaired, mut rewound, mut lost, mut failed) = (0, 0, 0, 0);

    for r in repairs.iter() {
        let k = r.key.key();

        /* Errors are counted, not returned - don't want them blocking mount */
        match lockrestart_do(ctxt.trans(), |t| repair_one(&ctxt, t, r)) {
            Ok(Outcome::Unchanged) => {}
            Ok(Outcome::Repaired)  => {
                repaired += 1;
                fs.event_inc_trace(events::journal_scrub_repaired, |out| prt!(out, "{}", k.to_text(fs)));
            }
            Ok(Outcome::Rewound)   => rewound += 1,
            Ok(Outcome::Lost)      => lost += 1,
            Err(e) => {
                failed += 1;
                bch_err_ratelimited!(fs, "journal scrub: error {} repairing:\n{}", e.msg(), k.to_text(fs));
            }
        }
    }

    bch_info!(fs, "journal scrub: repairing {repaired} extents/btree nodes from good replicas; \
                   {rewound} lost to the rewind, {lost} with no good replica, {failed} failed");
}
