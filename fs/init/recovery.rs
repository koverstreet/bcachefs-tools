// SPDX-License-Identifier: GPL-2.0

//! Recovery: a filesystem brought up from its superblock and journal, or a
//! new one initialized.

use crate::init::passes::RecoveryPass;
use crate::accounting;
use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{BkeySC, POS_MIN, SPOS_MAX};
use crate::btree::bkey_buf::BkeyBuf;
use crate::btree::iter::{commit_do, trans_commit_do, BtreeIterFlags, BtreeNodeIter, CommitFlags, CommitOpts,
                         TransAttempt, UpdateTriggerFlags, Watermark};
use crate::btree::journal_overlay::{journal_keys_put_initial, keys_sort, shoot_down_journal_keys};
use crate::c;
use crate::errcode::{Errcode, ret_to_c, BchError};
use crate::fs::{btree_id_can_reconstruct, btree_id_is_alloc, Fs};
use crate::init::error::{self, id};
use crate::init::passes::{self, run_explicit_sb};
use crate::journal::{journal_pos_from_member_info_resume, journal_read, journal_reread_for_rewind};
use crate::journal::sb as journal_sb;
use crate::journal::seq_blacklist::blacklist_entries_gc;
use crate::opts::opts_hooks_pre_set;
use crate::sb::io::{check_version_downgrade, sb_upgrade, SbWrite};
use crate::snapshots::snapshot::snapshots_read;
use crate::snapshots::subvolume;
use crate::types::Le64;
use crate::locking::Guard as _;
use crate::util::alloc::KVVec;
use crate::os::kernel::{cond_resched, sort_nonatomic, unlikely};
use crate::util::log::LogMsg;
use crate::util::os_str::{OsStr, OsStrExt};
use crate::util::Printbuf;
use crate::{inode, logged_ops, namei, quota};
use crate::{bch_err, bch_err_msg, bch_info, bch_notice, bch_verbose, btree_trans, fs_fatal_error, fsck_err_on,
            message, mustfix_fsck_err, mustfix_fsck_err_on, prt};
use c::FsFlag as Flag;
use c::ProgressUnit as Units;
use core::cmp::Ordering;
use core::sync::atomic::Ordering::Relaxed;
use crate::format::{Btree, MemberInitialized, BkeyType, SbFeature};
use zerocopy::IntoBytes;

type Pass = RecoveryPass;

/// The filesystem before it can go rw: the recovery task alone runs, so what
/// this hands out is had exclusively. Not Send: the recovery task's.
pub struct NotRw<'a> {
    pub(crate) fs: &'a Fs,
    _task:         core::marker::PhantomData<*mut ()>,
}

/// What's written only before the filesystem can go rw is a LockedBy of
/// NotRw: had through one, alone - nothing else runs.
unsafe impl crate::locking::Guard for NotRw<'_> {
    type Name = NotRw<'static>;
    type Root = Fs;

    fn root(&self) -> &Fs {
        self.fs
    }
}

unsafe impl crate::locking::Alone for NotRw<'_> {}

// Recovery's ref on the journal keys - their AtomicRef's first - is put by
// journal_keys_put_initial(): in journal_replay, a pass after set_may_go_rw,
// or at the filesystem's release
unsafe impl crate::locking::HoldsRef<crate::at!(Fs.journal_keys)> for NotRw<'_> {}

impl Fs {
    /// The asserts are that nothing else runs: called before the filesystem
    /// can go rw, on the recovery task.
    // XXX: NotRw isn't linear - nothing stops one being held past going rw
    pub(crate) fn not_rw(&self) -> NotRw<'_> {
        assert!(!self.flags.get(Flag::may_go_rw));
        assert!(!self.flags.get(Flag::rw));
        /* the journal keys are frozen for the scrub, whose reads run on workqueues */
        assert!(!self.flags.get(Flag::scrub_journal));
        assert!(self.recovery_task.load(Relaxed) == crate::util::sched::current());

        NotRw { fs: self, _task: core::marker::PhantomData }
    }
}

macro_rules! passes {
    ($($p:ident),* $(,)?) => { &[$(Pass::$p),*] };
}

/// What repairs btree @btree having lost data, besides check_topology and
/// check_allocations, which every btree's loss needs. A content btree's keys
/// are checked against what refers to them, and what they refer to: damage to
/// one leaves dangling references in the others, which otherwise surface later
/// as runtime errors - a dirents btree flagged lost in one boot is a dirent to
/// a missing inode weeks after.
fn lost_data_passes(btree: Btree) -> &'static [Pass] {
    match btree {
        Btree::alloc |
        Btree::need_discard |
        Btree::freespace |
        Btree::bucket_gens |
        Btree::lru                 => passes![check_alloc_info],
        Btree::backpointers        => passes![check_btree_backpointers, check_extents_to_backpointers],
        Btree::accounting          => passes![check_allocations],
        Btree::snapshots           => passes![reconstruct_snapshots, check_topology, scan_for_btree_nodes],
        Btree::snapshot_trees      => passes![check_snapshot_trees, check_snapshots],
        Btree::extents             => passes![check_extents, check_inodes, check_extents_to_backpointers],
        Btree::inodes              => passes![check_inodes, check_extents, check_dirents, check_xattrs,
                                              check_nlinks],
        Btree::dirents             => passes![check_dirents, check_inodes, check_nlinks,
                                              check_directory_structure],
        Btree::xattrs              => passes![check_xattrs],
        Btree::reflink             => passes![check_indirect_extents, check_extents],
        Btree::subvolumes          => passes![check_subvols, check_subvol_children,
                                              check_subvolume_structure, check_root],
        Btree::subvolume_children  => passes![check_subvol_children, check_subvols],
        Btree::deleted_inodes      => passes![check_inodes],
        /* check_allocations re-derives bucket state from the stripes: */
        Btree::stripes |
        Btree::bucket_to_stripe |
        Btree::stripe_backpointers => passes![check_extents_to_backpointers],
        _                          => passes![check_topology, scan_for_btree_nodes],
    }
}

/// Btree @btree lost data: flagged - and in the forensic record, never cleared -
/// with its repairs scheduled and the errors they'll find, which the loss
/// explains, silenced. @msg is the report: unsuppressed if any of this was new,
/// so damage already reported doesn't print on every read of a bad node.
pub fn btree_lost_data(fs: &Fs, msg: &mut Printbuf, btree: Btree) -> Result<(), BchError> {
    let mut w = fs.sb_write();

    w.set_btrees_lost_data_ever(btree);
    if !fs.btree_lost_data(btree) {
        prt!(msg, "flagging btree {btree} lost data\n");
        w.set_btrees_lost_data(btree);
    }

    /* The last error, as C's ?: chain kept it: every pass is scheduled */
    let mut ret = Ok(());
    let mut run = |w: &mut SbWrite<'_>, pass| {
        if let Err(e) = run_explicit_sb(fs, msg, pass, c::RunRecoveryPassFlags::empty(), Some(w)) {
            ret = Err(e);
        }
    };

    /* Once we have runtime self healing for topology errors we won't need this: */
    run(&mut w, Pass::check_topology);

    /* Btree node accounting will be off: */
    w.set_err_silent(id::accounting_mismatch);
    run(&mut w, Pass::check_allocations);

    /* Minor, and not urgent, but debug wants the next fsck clean: */
    if cfg!(CONFIG_BCACHEFS_DEBUG) {
        run(&mut w, Pass::check_lrus);
    }

    for e in [id::lru_entry_bad, id::alloc_key_to_missing_lru_entry, id::backpointer_to_missing_ptr,
              id::alloc_key_data_type_wrong, id::alloc_key_dirty_sectors_wrong, id::need_discard_key_wrong,
              id::freespace_key_wrong, id::reconcile_work_phys_incorrectly_set,
              id::reconcile_work_incorrectly_set] {
        w.set_err_silent(e);
    }
    if btree == Btree::alloc {
        for e in [id::alloc_key_gen_wrong, id::alloc_key_cached_sectors_wrong, id::alloc_key_stripe_wrong,
                  id::alloc_key_stripe_redundancy_wrong] {
            w.set_err_silent(e);
        }
    }

    for &pass in lost_data_passes(btree) {
        run(&mut w, pass);
    }

    if w.is_dirty() {
        msg.set_suppressed(false);
    }
    ret
}

/// The alloc info dropped, to be rebuilt: the passes that rebuild it required,
/// the errors they'll find silenced, and the alloc btrees killed. For
/// -o reconstruct_alloc, or a filesystem with no alloc info going read-write.
fn reconstruct_alloc(fs: &Fs) {
    {
        let mut w = fs.sb_write();

        let required = &mut w.ext_mut().recovery_passes_required[0];
        let mut v = required.get();
        for &pass in passes![check_allocations, check_alloc_info, check_lrus, check_extents_to_backpointers,
                            check_alloc_to_lru_refs] {
            v |= 1 << pass.stable().0;
        }
        required.set(v);

        for e in [id::ptr_to_missing_alloc_key, id::ptr_gen_newer_than_bucket_gen, id::stale_dirty_ptr,
                  id::dev_usage_buckets_wrong, id::dev_usage_sectors_wrong, id::dev_usage_fragmented_wrong,
                  id::fs_usage_btree_wrong, id::fs_usage_cached_wrong, id::fs_usage_persistent_reserved_wrong,
                  id::fs_usage_replicas_wrong, id::alloc_key_to_missing_lru_entry,
                  id::alloc_key_data_type_wrong, id::alloc_key_gen_wrong, id::alloc_key_dirty_sectors_wrong,
                  id::alloc_key_cached_sectors_wrong, id::alloc_key_stripe_wrong,
                  id::alloc_key_stripe_redundancy_wrong, id::need_discard_key_wrong, id::freespace_key_wrong,
                  id::bucket_gens_key_wrong, id::freespace_hole_missing, id::ptr_to_missing_backpointer,
                  id::lru_entry_bad, id::accounting_mismatch] {
            w.set_err_silent(e);
        }

        let compat = &w.published().compat;
        compat.set(compat.get() & !(1 << c::SbCompat::alloc_info.0));

        let opts = fs.opts_writer(&mut w);
        opts.recovery_passes.set(opts.recovery_passes.get() | passes::passes_from_stable(v));

        w.sb_mut().features[0] &= !(1 << SbFeature::no_alloc_info.0);
        w.dirty();
    }

    for btree in (0..fs.btree_id_nr_alive()).filter(|&b| btree_id_is_alloc(b)) {
        fs.btree.cache.roots.lock().get_mut(btree).expect("a btree below btree_id_nr_alive()").alive = 0;
        shoot_down_journal_keys(fs, Btree(btree), 0, c::BTREE_MAX_DEPTH, POS_MIN, SPOS_MAX);
    }
}

/// After a journal rewind, the alloc info is stale for buckets whose state
/// changed between the rewind point and the original journal head: what
/// check_alloc_info finds of that, silenced.
fn ignore_journal_rewind_errors(fs: &Fs) {
    let mut w = fs.sb_write();
    for e in [id::alloc_key_data_type_wrong, id::alloc_key_gen_wrong, id::alloc_key_dirty_sectors_wrong,
              id::alloc_key_cached_sectors_wrong, id::alloc_key_stripe_wrong,
              id::alloc_key_stripe_redundancy_wrong, id::need_discard_key_wrong, id::freespace_key_wrong,
              id::freespace_hole_missing, id::bucket_gens_key_wrong, id::alloc_key_to_missing_lru_entry,
              id::lru_entry_bad, id::accounting_mismatch, id::backpointer_to_missing_ptr] {
        w.set_err_silent(e);
    }
}

/* journal replay: */

fn journal_replay_accounting_key(t: &TransAttempt<'_, '_>, jk: &c::journal_keys, k: &c::journal_key)
    -> Result<(), BchError>
{
    let fs = t.fs();
    let bk = jk.key(k);

    let mut iter = BtreeNodeIter::new(t, k.btree_id(), bk.k.p, c::BTREE_MAX_DEPTH, k.level().into(),
                                      BtreeIterFlags::INTENT);
    iter.iter().traverse(t)?;

    let mut u = c::bkey::default();
    let old = iter.iter().path_peek_slot(t, &mut u);

    /* Has this delta already been applied to the btree? */
    if old.k.bversion >= bk.k.bversion {
        return Ok(());
    }

    let mut new = t.bkey_make_mut_noupdate(BkeySC::from(bk))?;
    if old.k.key_type() == BkeyType::accounting {
        accounting::accumulate(new.as_mut(), old);
    }

    if let Some(seq) = fs.journal_key_seq(k) {
        t.set_journal_seq_to_pin(seq);
    }

    t.update(iter.iter(), &new, UpdateTriggerFlags::NORUN)
}

fn journal_replay_key(t: &TransAttempt<'_, '_>, jk: &c::journal_keys, idx: usize) -> Result<(), BchError> {
    let fs = t.fs();
    let k = &jk.keys()[idx];
    let mut iter_flags = BtreeIterFlags::INTENT | BtreeIterFlags::NOT_EXTENTS;
    let mut update_flags = UpdateTriggerFlags::NORUN;

    if k.is_overwritten() {
        return Ok(());
    }

    if let Some(seq) = fs.journal_key_seq(k) {
        t.set_journal_seq_to_pin(seq);
    }

    /*
     * BTREE_UPDATE_key_cache_reclaim disables key cache lookup/update to
     * keep the key cache coherent with the underlying btree. Nothing
     * besides the allocator is doing updates yet so we don't need key cache
     * coherency for non-alloc btrees, and key cache fills for snapshots
     * btrees use BTREE_ITER_filter_snapshots, which isn't available until
     * the snapshots recovery pass runs.
     */
    if k.level() == 0 && k.btree_id() == Btree::alloc {
        iter_flags |= BtreeIterFlags::CACHED;
    } else {
        update_flags |= UpdateTriggerFlags::KEY_CACHE_RECLAIM;
    }

    let bk = jk.key(k);
    let mut iter = BtreeNodeIter::new(t, k.btree_id(), bk.k.p, c::BTREE_MAX_DEPTH, k.level().into(), iter_flags);
    iter.iter().traverse(t)?;

    if unlikely(!iter.iter().has_node_at_iter_level(t)) {
        drop(iter);
        return journal_replay_key_no_node(t, jk, idx, iter_flags);
    }

    /* Must be checked with btree locked: */
    if k.is_overwritten() {
        return Ok(());
    }

    if bk.k.key_type() == BkeyType::accounting {
        return t.subbuf_append(&t.trans().accounting, bk.k.u64s.into(),
                               |buf| buf.as_mut_bytes().copy_from_slice(bk.as_bytes()));
    }

    let new = t.bkey_make_mut_noupdate(BkeySC::from(bk))?;
    t.update(iter.iter(), &new, update_flags)
}

#[cold]
#[inline(never)]
fn journal_replay_key_no_node(t: &TransAttempt<'_, '_>, jk: &c::journal_keys, idx: usize,
                              iter_flags: BtreeIterFlags) -> Result<(), BchError>
{
    let fs = t.fs();
    let k = &jk.keys()[idx];
    let bk = jk.key(k);

    let mut buf = Printbuf::new();
    prt!(&mut buf, "btree={} level={} {}", k.btree_id(), k.level(), BkeySC::from(bk).to_text(fs));

    let scanned = passes::bit(Pass::scan_for_btree_nodes) |
                  passes::bit(Pass::check_topology);
    if fs.recovery.runner.passes_complete.get() & scanned == 0 {
        bch_err!(fs, "have key in journal replay for btree depth that does not exist, confused\n{}", buf);
        return fs.throw(Errcode::BCH_ERR_EINVAL_journal_replay_key_bad_btree_depth);
    }

    if !k.allocated() {
        bch_notice!(fs, "dropping key in journal replay for depth that does not exist because we're recovering from scan\n{}",
                    buf);
        jk.mark_overwritten(idx);
        return Ok(());
    }

    let mut iter = BtreeNodeIter::new(t, k.btree_id(), bk.k.p, c::BTREE_MAX_DEPTH, 0, iter_flags);
    iter.iter().traverse(t)?;
    t.btree_increase_depth(iter.iter())?;
    Err(t.restart(Errcode::BCH_ERR_transaction_restart_nested))
}

fn journal_sort_seq_cmp(l: &c::journal_key, r: &c::journal_key) -> Ordering {
    match (l.journal_pos(), r.journal_pos()) {
        (Some(l), Some(r)) => l.journal_seq_offset.cmp(&r.journal_seq_offset),
        _                  => l.allocated().cmp(&r.allocated()),
    }
}

fn journal_replay(fs: &Fs) -> Result<(), BchError> {
    let jk = fs.journal_keys_get().expect("journal replay after recovery was done with the journal keys");
    let jk = jk.value();
    let keys = jk.keys();
    let start_seq = fs.journal_replay_seq_start.get();
    let end_seq = fs.journal_replay_seq_end.get();
    let mut immediate_flush = false;

    let replay_flags = CommitFlags::JOURNAL_REPLAY | CommitFlags::NO_ENOSPC | CommitFlags::NO_SKIP_NOOPS |
                       CommitFlags::SKIP_ACCOUNTING_APPLY;

    assert_eq!(keys.gap, keys.len());

    if !keys.is_empty() {
        fs.journal_log_msg(message!("Starting journal replay ({} keys in entries {}-{})",
                                    keys.len(), start_seq, end_seq))?;
    }

    let trans = btree_trans!(fs);

    let replays_accounting = |k: &c::journal_key| {
        jk.key(k).k.key_type() == BkeyType::accounting && !k.allocated()
    };
    let accounting_total = keys.iter().filter(|k| replays_accounting(k)).count();

    /*
     * Replay accounting keys first: we can't allow the write buffer to
     * flush accounting keys until we're done
     */
    fs.recovery.progress.init_count(Some(c"journal replay: accounting"), Units::keys,
                                    accounting_total as u64);

    for (idx, k) in keys.iter().enumerate() {
        if !replays_accounting(k) {
            continue;
        }

        cond_resched();

        let flags = CommitOpts::new()
            .flags(replay_flags | CommitFlags::JOURNAL_RECLAIM | CommitFlags::NO_JOURNAL_RES)
            .watermark(Watermark::reclaim);
        if let Err(e) = commit_do(&trans, None, flags, |t| journal_replay_accounting_key(t, jk, k)) {
            fs_fatal_error!(fs, "error replaying accounting; {}", e.msg());
            return Err(e);
        }

        jk.mark_overwritten(idx);
        fs.recovery.progress.update_count(fs);
    }

    fs.flags.set(Flag::accounting_replay_done, true);

    /*
     * First, attempt to replay keys in sorted order. This is more
     * efficient - better locality of btree access -  but some might fail if
     * that would cause a journal deadlock.
     */
    fs.recovery.progress.init_count(Some(c"journal replay: sorted pass"), Units::keys,
                                    keys.len() as u64);

    let mut keys_sorted = KVVec::new();
    for (idx, k) in keys.iter().enumerate() {
        cond_resched();
        fs.recovery.progress.update_count(fs);

        /*
         * k->allocated means the key wasn't read in from the journal,
         * rather it was from early repair code
         */
        if k.allocated() {
            immediate_flush = true;
        }

        /*
         * Skip the fastpath if we're low on space in the journal, or
         * if btree node allocation has hit the memory-pressure self
         * reclaim path: the fastpath holds every journal pin until
         * replay finishes, but self reclaim needs pins to be released
         * so that journal reclaim can write back and free btree nodes
         * - with enough devices the journal can be bigger than RAM,
         * and the btree node cache will eat all of memory before the
         * journal-space watermark ever trips.
         */
        let self_reclaim = fs.btree.cache.nr_self_reclaim.load(Relaxed);
        let replayed = fs.journal.watermark() == c::Watermark::stripe &&
            self_reclaim == 0 &&
            commit_do(&trans, None,
                      replay_flags | CommitFlags::JOURNAL_RECLAIM |
                      if !k.allocated() { CommitFlags::NO_JOURNAL_RES } else { CommitFlags::empty() },
                      |t| journal_replay_key(t, jk, idx)).is_ok();
        if !replayed {
            keys_sorted.push(idx)?;
        }
    }

    trans.unlock_long();
    /*
     * Now, replay any remaining keys in the order in which they appear in
     * the journal, unpinning those journal entries as we go:
     */
    sort_nonatomic(&mut keys_sorted, |&l, &r| journal_sort_seq_cmp(&keys[l], &keys[r]));

    fs.recovery.progress.init_count(Some(c"journal replay: journal-order pass"), Units::keys,
                                    keys_sorted.len() as u64);

    for &idx in keys_sorted.iter() {
        cond_resched();
        fs.recovery.progress.update_count(fs);

        let k = &keys[idx];

        fs.journal.replay_pins_put(fs.journal_key_seq(k).unwrap_or(fs.journal.lock.replay_journal_seq_end.get()));

        let mut flags = CommitOpts::new().flags(replay_flags);
        if !k.allocated() {
            flags = flags.flags(CommitFlags::NO_JOURNAL_RES).watermark(Watermark::reclaim);
        }
        if let Err(e) = commit_do(&trans, None, flags, |t| journal_replay_key(t, jk, idx)) {
            return bch_err_msg!(fs, Err(e), "while replaying key at btree={} level={}:", k.btree_id(), k.level());
        }

        assert!(k.btree_id() == Btree::accounting || k.is_overwritten(),
                "journal key {idx} replayed, but not overwritten");
    }

    trans.unlock_long();

    if fs.opts.retain_recovery_info.get() == 0 &&
       fs.recovery.runner.pass_done.get() >= Pass::journal_replay {
        journal_keys_put_initial(fs);
    }

    fs.journal.replay_pins_put(fs.journal.lock.replay_journal_seq_end.get());
    fs.journal.replay_journal_seq.set(0);

    fs.journal.set_replay_done();

    /* if we did any repair, flush it immediately */
    if immediate_flush {
        fs.journal_flush_outstanding_pins();
        fs.journal_meta()?;
    }

    if !keys.is_empty() {
        let _ = fs.journal_log_msg(message!("journal replay finished"));
    }
    Ok(())
}

crate::recovery_pass!(journal_replay => journal_replay);

/* journal replay early: */

fn journal_replay_entry_early(fs: &Fs, entry: &c::jset_entry) -> Result<(), BchError> {
    match entry.typed() {
        Ok(c::JsetEntryRef::btree_root(e)) => {
            let Some(k) = e.entries().next() else { return Ok(()) };
            let id = u32::from(e.entry.btree_id);

            if fsck_err_on!(fs, id >= c::BTREE_ID_NR_MAX, id::invalid_btree_id,
                            "invalid btree id {} (max {})", id, c::BTREE_ID_NR_MAX)? {
                return Ok(());
            }

            let mut roots = fs.btree.cache.roots.lock();

            roots.grow_to(id)?;

            let r = roots.get_mut(id).expect("grown to it");
            r.level = e.entry.level;
            r.key.set_key(k.key());
            r.alive = 1;
            roots.published().roots_error[id as usize].set(0);

            /* every journal entry carries every root, this version's btrees or not: */
            let nr_roots = Btree::NR + roots.roots_extra.len() as u32;
            drop(roots);
            if id >= Btree::NR {
                fs.journal_entry_res_resize(&fs.btree.root_journal_res,
                                            crate::btree::interior::roots_journal_u64s(nr_roots));
            }
        }
        Ok(c::JsetEntryRef::usage(u)) => {
            if u.entry.btree_id == c::FsUsageType::key_version.0 as u8 {
                fs.key_version.store(u.v.get() as i64, Relaxed);
            }
        }
        Ok(c::JsetEntryRef::blacklist(bl)) => {
            fs.journal_seq_blacklist_add(bl.seq.get(), bl.seq.get().wrapping_add(1))?;
        }
        Ok(c::JsetEntryRef::blacklist_v2(bl)) => {
            fs.journal_seq_blacklist_add(bl.start.get(), bl.end.get().wrapping_add(1))?;
        }
        Ok(c::JsetEntryRef::clock(clock)) => {
            fs.io_clock[usize::from(clock.rw)].now.store(clock.time.get() as i64, Relaxed);
        }
        _ => {}
    }
    Ok(())
}

fn journal_replay_early(fs: &Fs, clean: Option<&c::bch_sb_field_clean>) -> Result<(), BchError> {
    if let Some(clean) = clean {
        for entry in clean.entries() {
            journal_replay_entry_early(fs, entry)?;
        }
    } else {
        let jk = fs.journal_keys_get().expect("early replay after recovery was done with the journal keys");
        for (_, j) in jk.value().replays() {
            for entry in j.entries() {
                journal_replay_entry_early(fs, entry)?;
            }
        }
    }

    Ok(())
}

/* sb clean section: */

fn read_btree_roots(fs: &Fs) -> Result<(), BchError> {
    let mut buf = Printbuf::new();

    for i in 0..fs.btree_id_nr_alive() {
        let Some((key, level)) = fs.btree.cache.roots.lock().get(i)
            .filter(|r| r.alive != 0)
            .map(|r| (BkeyBuf::from_key(r.key.key().into()), r.level))
        else {
            continue;
        };

        buf.reset();
        prt!(&mut buf, "btree={} level={}", Btree(i), level);

        let fixed = if fs.btree.cache.roots.error(i) != 0 {
            mustfix_fsck_err!(fs, id::btree_root_bkey_invalid, "invalid btree root {}", buf)?
        } else {
            let read = fs.btree_root_read(Btree(i), key.as_ref(), level.into());
            fs.btree_id_root_set_error(i, ret_to_c(read));
            mustfix_fsck_err_on!(fs, read.is_err(), id::btree_root_read_error,
                                 "error reading btree root {}: {}", buf, read.err().map_or("", |e| e.msg()))?
        };
        if fixed && btree_id_can_reconstruct(i) {
            fs.btree_id_root_set_error(i, 0);
        }
    }

    for i in 0..Btree::NR {
        if fs.btree_id_root(i).is_none() && fs.btree.cache.roots.error(i) == 0 {
            {
                let mut roots = fs.btree.cache.roots.lock();
                let r = roots.get_mut(i).expect("a known btree's root");
                r.alive = 0;
                r.level = 0;
            }
            fs.btree_root_alloc_fake(Btree(i), 0);
        }
    }
    Ok(())
}

fn set_may_go_rw(fs: &Fs) -> Result<(), BchError> {
    /*
     * After we go RW, the journal keys buffer can't be modified (except for
     * setting journal_key->overwritten: it will be accessed by multiple
     * threads
     */
    if let Some(jk) = fs.journal_keys_get() {
        fs.not_rw().get_mut_of(&jk.value().keys).make_contiguous();
    }

    fs.flags.set(Flag::may_go_rw, true);

    /* Before going rw, and anything that could start a logged op: */
    logged_ops::note_unfinished(fs)?;

    if passes::go_rw_in_recovery(fs) {
        if fs.feature(SbFeature::no_alloc_info) {
            bch_info!(fs, "mounting a filesystem with no alloc info read-write; will recreate");
            reconstruct_alloc(fs);
        }

        return fs.state_lock_write().start_rw();
    }
    Ok(())
}

crate::recovery_pass!(set_may_go_rw => set_may_go_rw);

fn recovery(fs: &Fs) -> Result<(), BchError> {
    let mut journal_start = c::journal_start_info::default();

    let mut clean = if fs.sb.clean.get() != 0 {
        let c = fs.read_superblock_clean()?;

        bch_info!(fs, "recovering from clean shutdown, journal seq {}", c.journal_seq.get());

        journal_sb::sb_journal_sort(fs)?;
        Some(c)
    } else {
        bch_info!(fs, "recovering from unclean shutdown");
        None
    };

    journal_pos_from_member_info_resume(fs);

    let read_journal = fs.sb.clean.get() == 0 ||
        fs.opts.retain_recovery_info.get() != 0 ||
        fs.opts.journal_rewind.get() != 0 ||
        u32::from(fs.opts.scrub_recent_journal_entries.get()) == c::ScrubJournalOpt::always.0;
    let mut use_clean = !read_journal;

    if read_journal {
        bch_verbose!(fs, "starting journal read");
        journal_start = journal_read(fs)?;

        /*
         * If we found persisted rewind ranges from a previous
         * crashed rewind, re-read any journal entries that were
         * dropped because they were older than last_seq:
         */
        journal_reread_for_rewind(fs)?;

        /*
         * note: cmd_list_journal needs the blacklist table fully up to date so
         * it can asterisk ignored journal entries:
         */
        if fs.opts.read_journal_only.get() != 0 {
            return Ok(());
        }

        /*
         * bch2_journal_read() leaves clean false if it found no flush
         * entry at all - an empty journal, not a dirty one:
         */
        if mustfix_fsck_err_on!(fs, fs.sb.clean.get() != 0 &&
                                    journal_start.replay_end != 0 &&
                                    !journal_start.clean,
                                id::clean_but_journal_not_empty,
                                "filesystem marked clean but journal not empty")? {
            let mut sb = fs.sb_lock();
            let compat = fs.sb.compat.get() & !(1 << c::SbCompat::alloc_info.0);
            sb.published().compat.set(compat);
            sb.sb_mut().set_clean(false);
            sb.published().clean.set(0);
        }

        let jk = fs.journal_keys_get().expect("recovery after it was done with the journal keys");
        let jk = jk.value();
        let mut last_journal_entry = jk.all_replays_rev().find(|(i, _)| !i.ignored()).map(|(_, j)| j);

        if last_journal_entry.is_none() {
            fsck_err_on!(fs, fs.sb.clean.get() == 0, id::dirty_but_no_journal_entries,
                         "no journal entries found")?;
            if clean.is_some() {
                use_clean = true;
            } else if let Some(i) = fs.not_rw().get_mut_of(&jk.entries).slots_rev_mut().find_map(Option::as_mut) {
                i.ignore_blacklisted = false;
                i.ignore_not_dirty = false;
                /*
                 * This was probably a NO_FLUSH entry,
                 * so last_seq was garbage - but we know
                 * we're only using a single journal
                 * entry, set it here:
                 */
                i.j.last_seq = i.j.seq;
                last_journal_entry = jk.all_replays_rev().next().map(|(_, j)| j);
            }
        }

        if !use_clean && fs.sb.clean.get() != 0 {
            if let (Some(c), Some(j)) = (clean.as_deref(), last_journal_entry) {
                if !fs.verify_superblock_clean(c, j)? {
                    clean = None;
                }
            }
        }
    }

    if use_clean {
        let Some(c) = clean.as_deref() else {
            bch_err!(fs, "no superblock clean section found");
            return fs.throw(Errcode::BCH_ERR_fsck_repair_impossible);
        };

        journal_start.cur_seq = c.journal_seq.get().wrapping_add(1);
    }

    fs.journal_replay_seq_start.set(journal_start.last_seq);
    fs.journal_replay_seq_end.set(journal_start.replay_end);

    journal_replay_early(fs, clean.as_deref())?;

    {
        let state = fs.state_lock_write();
        fs.resize_on_mount(&state)?;
    }

    if fs.feature(SbFeature::small_image) {
        bch_info!(fs, "filesystem is an unresized image file, mounting ro");
        fs.opts_writer(&mut fs.sb_lock()).read_only.set(1);
    }

    if fs.feature(SbFeature::no_default_sb) {
        bch_info!(fs, "filesystem does not have default superblock layout, mounting ro");
        fs.opts_writer(&mut fs.sb_lock()).read_only.set(1);
    }

    if fs.opts.read_only.get() == 0 && fs.feature(SbFeature::no_alloc_info) {
        bch_info!(fs, "mounting a filesystem with no alloc info read-write; will recreate");

        reconstruct_alloc(fs);
    } else if fs.opts.dangerously_reconstruct_alloc.get() != 0 {
        let _ = fs.journal_log_msg(message!("dropping alloc info"));
        bch_info!(fs, "dropping and reconstructing all alloc info");

        reconstruct_alloc(fs);
    }

    if fs.opts.journal_rewind.get() != 0 {
        fs.not_rw().journal_add_rewind_range(journal_start.replay_end, fs.opts.journal_rewind.get())?;
        ignore_journal_rewind_errors(fs);
        journal_reread_for_rewind(fs)?;
    }

    if fs.feature(SbFeature::no_alloc_info) {
        /* We can't go RW to fix errors without alloc info */
        let mut sb = fs.sb_lock();
        let w = fs.opts_writer(&mut sb);
        let fix = w.fix_errors.get();
        if fix == c::FsckErrOpt::yes as u8 || fix == c::FsckErrOpt::ask as u8 {
            w.fix_errors.set(c::FsckErrOpt::no as u8);
        }
        if u32::from(w.errors.get()) == c::ErrorAction::fix_safe.0 {
            w.errors.set(c::ErrorAction::BCH_ON_ERROR_continue.0 as u8);
        }
    }

    /*
     * After an unclean shutdown, skip the next several journal sequence
     * numbers: btree nodes may have been written referencing journal
     * sequences whose journal writes never became durable (btree writes
     * lead the journal). Those sequences are blacklisted below, so btree
     * writes referencing them are ignored on replay.
     *
     * The skip must exceed how far a btree write can lead the durable
     * journal, which is bounded by the journal pipeline depth
     * (j->in_flight.size, currently 256). Use a large margin so this
     * can't silently break if pipelining is increased.
     */
    if fs.sb.clean.get() == 0 {
        journal_start.cur_seq = journal_start.cur_seq.wrapping_add(4096);
    }

    let blacklist_seq = journal_start.replay_end.wrapping_add(1);
    if journal_start.replay_end != 0 && blacklist_seq != journal_start.cur_seq {
        fs.journal_log_msg(message!("blacklisting entries {}-{}", blacklist_seq, journal_start.cur_seq))?;
        fs.journal_seq_blacklist_add(blacklist_seq, journal_start.cur_seq)?;
    }

    /*
     * Skip past versions that might have possibly been used (as nonces),
     * but hadn't had their pointers written:
     */
    if fs.sb.encryption_type.get() != 0 && fs.sb.clean.get() == 0 {
        fs.key_version.fetch_add(1 << 16, Relaxed);
    }

    keys_sort(fs)?;

    read_btree_roots(fs)?;

    fs.flags.set(Flag::btree_running, true);

    /* some mount options can only be checked after the btree is running */
    opts_hooks_pre_set(fs)?;

    fs.sb_set_upgrade_extra();

    if fs.journal_scrub_will_run() {
        fs.flags.set(Flag::scrub_journal, true);
        let rewind_seq = crate::data::scrub_journal::scrub(fs)?;
        fs.flags.set(Flag::scrub_journal, false);

        /*
         * Done with the keys from before the replay start: re-sort
         * without them, before accounting read and replay - the
         * rewind below re-sorts anyway.
         */
        let resort = fs.journal_scrub_seq.get() != 0;
        fs.journal_scrub_seq.set(0);
        if resort && rewind_seq == 0 {
            keys_sort(fs)?;
        }
        if rewind_seq != 0 {
            let mut msg = LogMsg::new(fs);
            prt!(&mut msg, "journal scrub: device not honoring flush/FUA, rewinding from seq {} (",
                 journal_start.replay_end);
            fs.journal_seq_datetime_to_text(&mut msg, journal_start.replay_end);
            prt!(&mut msg, ") back to seq {} (", rewind_seq);
            fs.journal_seq_datetime_to_text(&mut msg, rewind_seq);
            prt!(&mut msg, ")");

            let _ = fs.journal_log_msg(&*msg);
            {
                let mut sb = fs.sb_lock();
                let w = fs.opts_writer(&mut sb);
                w.journal_rewind.set(rewind_seq);
                w.fsck.set(1);
            }

            fs.not_rw().journal_add_rewind_range(journal_start.replay_end, rewind_seq)?;
            ignore_journal_rewind_errors(fs);

            /*
             * Re-read journal buckets to pick up entries that
             * were dropped during the first read because they
             * were older than last_seq:
             */
            journal_reread_for_rewind(fs)?;
            keys_sort(fs)?;
        }
    }

    /*
     * Not until the journal scrub has decided whether to rewind: a rewind
     * re-reads entries from before last_seq (back to the rewind target's
     * last_seq) and replays them, so the pin fifo has to start there -
     * journal_replay_seq_start, which only a re-read lowers. Nothing above
     * needs the journal started; the blacklist, which btree node reads do
     * need, is set up before the roots are read.
     */
    journal_start.last_seq = fs.journal_replay_seq_start.get();

    fs.journal_log_msg(message!("starting journal at entry {}, replaying {}-{}",
                                journal_start.cur_seq, journal_start.last_seq, journal_start.replay_end))?;
    fs.journal.start(journal_start)?;

    passes::run_passes_startup(fs, Pass::from_bits(0))?;

    /*
     * Normally set by the appropriate recovery pass: when cleared, this
     * indicates we're in early recovery and btree updates should be done by
     * being applied to the journal replay keys. _Must_ be cleared before
     * multithreaded use:
     */
    fs.flags.set(Flag::may_go_rw, true);
    fs.flags.set(Flag::in_fsck, false);

    /* in case we don't run journal replay, i.e. norecovery mode */
    fs.flags.set(Flag::accounting_replay_done, true);

    fs.btree.async_ops.flush();

    /* fsync if we fixed errors */
    let errors_fixed = fs.flags.get(Flag::errors_fixed) ||
        fs.flags.get(Flag::errors_fixed_silent);

    if errors_fixed {
        fs.journal_flush_outstanding_pins();
        let _ = fs.journal_meta();
    }

    if error::fsck_errs_seen(fs) {
        let mut msg = LogMsg::new(fs);
        prt!(&mut msg, "errors this recovery:\n");
        error::fsck_err_counts_to_text(&mut msg, fs);
        error::fsck_damaged_paths_to_text(&mut msg, fs);
    }

    /* If we fixed errors, verify that fs is actually clean now: */
    if cfg!(CONFIG_BCACHEFS_DEBUG) &&
       errors_fixed &&
       !fs.flags.get(Flag::errors_not_fixed) &&
       !fs.flags.get(Flag::error) {
        error::flush_fsck_errs(fs, true);

        bch_info!(fs, "Fixed errors, running fsck a second time to verify fs is clean");

        let saved_fixed        = fs.flags.swap(Flag::errors_fixed, false);
        let saved_fixed_silent = fs.flags.swap(Flag::errors_fixed_silent, false);

        passes::run_passes_startup(fs, Pass::check_alloc_info)?;

        if fs.flags.get(Flag::errors_fixed) ||
           fs.flags.get(Flag::errors_fixed_silent) ||
           fs.flags.get(Flag::errors_not_fixed) {
            bch_err!(fs, "Second fsck run was not clean");
            fs.flags.set(Flag::errors_not_fixed, true);
        }

        fs.flags.set(Flag::errors_fixed, saved_fixed);
        fs.flags.set(Flag::errors_fixed_silent, saved_fixed_silent);
    }

    if quota::enabled_qtypes(fs) != 0 {
        bch_verbose!(fs, "reading quotas");
        quota::fs_quota_read(fs)?;
        bch_verbose!(fs, "quotas done");
    }

    {
        let mut w = fs.sb_write();
        let error = fs.flags.get(Flag::error);
        let passes_complete = fs.recovery.runner.passes_complete.get();
        let complete = |p: Pass| passes_complete & passes::bit(p) != 0;

        let version = w.sb().version.get();
        if w.sb().version_upgrade_complete() != version {
            w.sb_mut().set_version_upgrade_complete(version);
            w.dirty();
        }

        if !error {
            w.set_compat(c::SbCompat::alloc_info);
        }

        if fs.opts.fsck.get() != 0 &&
           !error &&
           fs.recovery.runner.pass_done.get() == passes::LAST &&
           w.ext_mut().btrees_lost_data.get() != 0 {
            w.ext_mut().btrees_lost_data.set(0);
            w.dirty();
        }

        if fs.opts.fsck.get() != 0 &&
           !error &&
           !fs.flags.get(Flag::errors_not_fixed) {
            w.sb_mut().set_has_errors(false);
            w.dirty();
        }

        /*
         * BCH_SB_HAS_TOPOLOGY_ERRORS forces check_topology on every mount,
         * not just fsck mounts (see the "superblock requires" path in
         * bch2_fs_recovery() startup) - so clear it whenever that pass has
         * run clean, not only under fsck. Otherwise a plain mount re-runs
         * the full (potentially very slow) pass every time and never clears
         * the flag.
         */
        if complete(Pass::check_topology) &&
           !error &&
           !fs.flags.get(Flag::errors_not_fixed) {
            w.sb_mut().set_has_topology_errors(false);
            w.dirty();
        }

        if blacklist_entries_gc(fs, &mut w) {
            w.dirty();
        }

        if complete(Pass::check_extents) &&
           complete(Pass::check_indirect_extents) {
            w.set_compat(c::SbCompat::no_stale_ptrs);
        }

        /*
         * check_allocations recomputes accounting, so it is what makes
         * the stripe fragmentation counters true.
         */
        if complete(Pass::check_allocations) {
            w.set_compat(c::SbCompat::stripe_frag_accounting);
        }

        /*
         * check_inodes runs inode_opts::check_propagated(), so
         * after it every inode's options have reached its ancestor
         * snapshots - and a later violation is a bug, not legacy state.
         */
        if complete(Pass::check_inodes) {
            w.set_compat(c::SbCompat::inode_opts_propagated);
        }
    }

    /*
     * (Hopefully unnecessary) cleanup, once per mount - we should be
     * killing replicas entries when accounting entries go to 0, but - old
     * filesystems, etc.:
     */
    let _ = fs.replicas_gc_accounted();
    Ok(())
}

pub fn fs_recovery(fs: &Fs) -> Result<(), BchError> {
    let ret = recovery(fs);

    error::flush_fsck_errs(fs, true);

    if let Err(e) = ret {
        let mut msg = LogMsg::new(fs);
        prt!(&mut msg, "unexpected error in bch2_fs_recovery(): {}\n", e.msg());
        fs.emergency_read_only(&mut msg);
    }
    ret
}

pub fn fs_initialize(fs: &Fs) -> Result<(), BchError> {
    bch_notice!(fs, "initializing new filesystem");
    fs.flags.set(Flag::new_fs, true);

    {
        let mut sb = fs.sb_lock();
        sb.sb_mut().compat[0] |= 1 << c::SbCompat::extents_above_btree_updates_done.0 |
                                 1 << c::SbCompat::bformat_overflow_done.0 |
                                 1 << c::SbCompat::no_stale_ptrs.0 |
                                 1 << c::SbCompat::stripe_frag_accounting.0 |
                                 1 << c::SbCompat::inode_opts_propagated.0;

        check_version_downgrade(fs, &mut sb);

        if u32::from(fs.opts.version_upgrade.get()) != c::VersionUpgradeOpt::none.0 {
            let current = c::bcachefs_metadata_version_current;
            sb_upgrade(fs, &mut sb, current, false);
            sb.sb_mut().set_version_upgrade_complete(current);
            fs.write_super(&mut sb);
        }

        for ca in fs.members() {
            let m = sb.fs_member_mut(ca.dev_idx.into());
            m.set_freespace_initialized(false);
            m.set_initialized(MemberInitialized::pre_dev_usage);
        }

        fs.write_super(&mut sb);
    }

    for i in 0..Btree::NR {
        fs.btree_root_alloc_fake(Btree(i), 0);
    }

    fs.flags.set(Flag::btree_running, true);

    /*
     * Write out the superblock and journal buckets, now that we can do
     * btree updates
     */
    bch_verbose!(fs, "marking superblocks");
    bch_err_msg!(fs, fs.trans_mark_dev_sbs(UpdateTriggerFlags::TRANSACTIONAL), "marking superblocks")?;

    /*
     * journal_res_get() will crash if called before this has
     * set up the journal.pin FIFO and journal.cur pointer:
     */
    fs.journal.start(c::journal_start_info { cur_seq: 1, ..Default::default() })?;

    set_may_go_rw(fs)?;
    journal_replay(fs)?;
    subvolume::initialize(fs)?;
    snapshots_read(fs)?;
    fs.presplit_shard_boundaries();

    let mut root_inode = inode::init(fs, 0, 0, (c::S_IFDIR | 0o755) as c::umode_t, 0, None);
    root_inode.bi_inum = c::BCACHEFS_ROOT_INO.into();
    root_inode.bi_subvol = c::BCACHEFS_ROOT_SUBVOL;
    let mut packed_inode = inode::pack(&root_inode);
    packed_inode.inode.k.p.snapshot = u32::MAX;

    bch_err_msg!(fs, fs.btree_insert(Btree::inodes, packed_inode.as_mut(), None,
                                     CommitFlags::empty(), BtreeIterFlags::empty()),
                 "creating root directory")?;

    let mut lostfound_inode = c::bch_inode_unpacked::default();
    inode::init_early(fs, &mut lostfound_inode);

    let mut new_subvol = c::Subvolume::default();
    bch_err_msg!(fs, trans_commit_do(fs, None, CommitFlags::empty(), |t| {
        namei::create_trans(t, c::subvol_inum::ROOT, &mut root_inode, &mut lostfound_inode, &mut new_subvol,
                            Some(OsStr::from_bytes(b"lost+found")), 0, 0, (c::S_IFDIR | 0o700) as c::umode_t, 0,
                            None, None, c::subvol_inum::default(), 0)
    }), "creating lost+found")?;

    passes::set_pass_done(fs, passes::LAST);

    fs.copygc_wakeup();
    fs.reconcile_wakeup();

    if quota::enabled_qtypes(fs) != 0 {
        quota::fs_quota_read(fs)?;
    }

    bch_err_msg!(fs, fs.journal_flush(), "writing first journal entry")?;

    /* Don't allow rewind into initialization entries */
    let init_seq = fs.journal_cur_seq() + 1;
    fs.journal_advance_rewind_seq(init_seq);
    bch_info!(fs, "fs initialized, journal seq {} rewind_seq {}", init_seq - 1, init_seq);

    {
        let mut w = fs.sb_write();
        w.sb_mut().set_initialized(true);
        w.sb_mut().set_clean(false);

        let ext = w.ext_mut();
        ext.errors_silent.fill(Le64::new(0));
        ext.recovery_passes_required.fill(Le64::new(0));
        w.dirty();
    }

    Ok(())
}
