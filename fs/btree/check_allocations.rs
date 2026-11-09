// SPDX-License-Identifier: GPL-2.0

//! check_allocations: every reference to a bucket walked, and the alloc info
//! recomputed from them.

use core::ptr::{self, NonNull};
use core::sync::atomic::Ordering::Relaxed;
use crate::accounting::dev_usage_init;
use crate::alloc::accounting_mem::{accounting_gc_free, gc_accounting_done, gc_accounting_start, MarkWrite};
use crate::alloc::background::alloc_to_v4;
use crate::alloc::capacity::DiskReservation;
use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{pos, BkeyI, BkeySC, POS_MIN, SPOS_MAX};
use crate::btree::interior::check_topology;
use crate::btree::iter::{lockrestart_do, BtreeIter, BtreeIterFlags, BtreeNodeIter, CommitFlags, TransAttempt,
                         UpdateTriggerFlags};
use crate::btree::trans::BtreeTrans;
use crate::btree::trigger::{key_trigger, TriggerOp};
use crate::c;
use crate::data::reflink::{gc_reflink_add, gc_reflink_done, gc_reflink_free, gc_reflink_start};
use crate::errcode::{bch_errcode, BchError};
use crate::format::{Btree, BtreeFlags, DataType};
use crate::fs::{Dev, Fs};
use crate::init::error::id;
use crate::init::progress::Progress;
use crate::types::Le16;
use crate::util::Printbuf;
use crate::util::kernel::READ;
use crate::{bch_err_fn, bch_err_msg, btree_trans, fsck_err_on, mustfix_fsck_err_on, prt};

/* marking of btree keys/nodes: */

fn gc_mark_key(t: &TransAttempt<'_, '_>, btree: Btree, level: u32, iter: &BtreeIter<'_>,
               k: BkeySC<'_>) -> Result<(), BchError> {
    let fs = t.fs();

    let version = k.k.bversion.lo;
    assert!(!crate::debug::params::journal_seq_verify() || version <= fs.journal_cur_seq(),
            "key version {version} past the journal's seq {}", fs.journal_cur_seq());

    let key_version = fs.key_version.load(Relaxed) as u64;
    if fsck_err_on!(t, btree != Btree::accounting && version > key_version, id::bkey_version_in_future,
                    "key version number higher than recorded {key_version}\n{}", k.to_text(fs))? {
        fs.key_version.store(version as i64, Relaxed);
    }

    if mustfix_fsck_err_on!(t, level != 0 && !fs.btree_bitmap_marked(k, false), id::btree_bitmap_not_marked,
                            "btree ptr not marked in member info btree allocated bitmap\n{}", k.to_text(fs))? {
        fs.btree_bitmap_mark(k);
    }

    (k.k.key_type().ops().check_repair)(t, iter, btree, level, k)?;

    if t.has_updates() {
        let res = DiskReservation::new(fs);
        t.commit(Some(&res), CommitFlags::NO_ENOSPC)?;
        return Err(t.restart(bch_errcode::BCH_ERR_transaction_restart_commit));
    }

    if btree == Btree::reflink && level == 0 {
        bch_err_fn!(fs, gc_reflink_add(fs, k))?;
    }

    let deleted = BkeyI::<c::Deleted>::at(k.k.p);
    let old = BkeySC::from(deleted.as_ref());
    let mut new = t.bkey_reassemble_room(k, 0)?;
    let buf_u64s = new.buf_u64s;
    let mut op = TriggerOp::new(btree, level, old, new.as_s(), UpdateTriggerFlags::GC | UpdateTriggerFlags::INSERT,
                                buf_u64s);
    key_trigger(t, &mut op)
}

fn gc_btree_root(t: &TransAttempt<'_, '_>, btree: Btree) -> Result<(), BchError> {
    let fs = t.fs();
    let level = fs.btree_id_root(btree.0).expect("a btree gc walks, with a root").c.level;
    let mut iter = BtreeNodeIter::new(t, btree, POS_MIN, 0, level.into(), BtreeIterFlags::empty());

    if !iter.peek()?.is_some_and(|b| fs.btree_id_root(btree.0).is_some_and(|r| ptr::eq(r, &*b))) {
        return Err(t.restart(bch_errcode::BCH_ERR_transaction_restart_lock_root_race));
    }
    let b = t.trans().paths().held_btree_node(iter.iter().path, level.into()).expect("the node it peeked");

    let level = u32::from(level) + 1;
    fs.gc.set_pos(c::gc_pos::in_btree(btree, level as u16, SPOS_MAX));
    gc_mark_key(t, btree, level, iter.iter(), BkeySC::from(b.contents().key_i()))
}

fn gc_btree(trans: &BtreeTrans<'_>, progress: &Progress<'_>, btree: Btree, target_depth: u32)
    -> Result<(), BchError>
{
    let fs = trans.fs();

    for level in target_depth..c::BTREE_MAX_DEPTH as u32 {
        let mut prev = None;
        BtreeIter::new_level(trans, btree, POS_MIN, level, BtreeIterFlags::PREFETCH).for_each(trans, |t, iter, k| {
            fs.gc.set_pos(c::gc_pos::in_btree(btree, level as u16, k.k.p));
            progress.update(t, iter)?;

            let b = t.trans().paths().held_btree_node(iter.path, level as usize).expect("the node its key is in");
            if prev != Some(NonNull::from(&*b)) {
                check_topology(t.trans(), b)?;
            }
            prev = Some(NonNull::from(&*b));

            gc_mark_key(t, btree, level, iter, k)
        })?;
    }

    lockrestart_do(trans, |t| gc_btree_root(t, btree))
}

fn gc_btrees(fs: &Fs) -> Result<(), BchError> {
    let trans = btree_trans!(fs);
    let progress = Progress::recovery_masks(fs, c"check_allocations", !0, !0);

    let mut ids: [Btree; Btree::NR as usize] = core::array::from_fn(|i| Btree(i as u32));
    ids.sort_unstable_by_key(|&b| c::gc_btree_order(b));

    let btrees = ids.into_iter().chain((Btree::NR..fs.btree_id_nr_alive()).map(Btree));
    let ret = btrees.filter(|b| fs.btree_id_root(b.0).is_some()).try_for_each(|btree| {
        /*
         * Btrees that have triggers for their leaves: every btree with
         * snapshots runs transactional triggers, for bch2_trigger_snapshot_nr_keys()
         */
        let leaf_has_triggers = btree.flags().intersects(BtreeFlags::trans_triggers |
                                                         BtreeFlags::atomic_triggers |
                                                         BtreeFlags::snapshots);
        let mut target_depth = if leaf_has_triggers { 0 } else { 1 };

        /*
         * In fsck, we need to make sure every leaf node is readable
         * before going RW, otherwise we can no longer rewind inside
         * btree_lost_data to repair during the current fsck run.
         *
         * Otherwise, we can delay the repair to the next
         * mount or offline fsck.
         */
        if fs.flags.get(c::bch_fs_flags::in_fsck) {
            target_depth = 0;
        }

        gc_btree(&trans, &progress, btree, target_depth)
    });

    bch_err_fn!(fs, ret)
}

fn mark_superblocks(fs: &Fs) -> Result<(), BchError> {
    fs.gc.set_pos(c::gc_pos::of_phase(c::gc_phase::sb));

    fs.trans_mark_dev_sbs(UpdateTriggerFlags::GC)
}

/// # Safety
/// gc.pos is back to not_running.
unsafe fn gc_free(fs: &Fs, mark: &mut MarkWrite<'_>) {
    accounting_gc_free(fs, mark);

    unsafe {
        gc_reflink_free(fs);
        fs.ec.gc_stripes.free();

        for ca in fs.members() {
            ca.buckets_gc.free();
        }
    }
}

fn gc_start(fs: &Fs) -> Result<(), BchError> {
    fs.members().try_for_each(|ca| dev_usage_init(&ca, true))
}

/* returns true if not equal */
fn alloc_v4_cmp(l: &c::AllocV4, r: &c::AllocV4) -> bool {
    l.generation      != r.generation     ||
    l.oldest_gen      != r.oldest_gen     ||
    l.data_type       != r.data_type      ||
    l.dirty_sectors   != r.dirty_sectors  ||
    l.stripe_sectors  != r.stripe_sectors ||
    l.cached_sectors  != r.cached_sectors ||
    l.stripe_refcount != r.stripe_refcount
}

fn alloc_write_key(t: &TransAttempt<'_, '_>, iter: &BtreeIter<'_>, ca: &Dev, k: BkeySC<'_>) -> Result<(), BchError> {
    let fs = t.fs();
    let (inode, offset) = (iter.pos().inode, iter.pos().offset);

    if !ca.bucket_valid(k.k.p.offset) {
        return Ok(());
    }

    let old = alloc_to_v4(k);
    let mut new = old;
    let mut gc = old;

    let g = ca.gc_bucket(offset).expect("a valid bucket's gc state");
    let m = *g.lock.lock();
    m.to_alloc(&mut gc);

    let old_gc = gc;

    if (old.data_type() == DataType::sb || old.data_type() == DataType::journal) && !ca.is_online() {
        gc.data_type = old.data_type;
        gc.dirty_sectors = old.dirty_sectors;
    }

    /*
     * gc tracks sector counts via extent triggers but does not follow
     * empty-state transitions (need_discard, need_gc_gens) - those are
     * decided by bch2_trigger_alloc() using old vs new state and stick
     * in data_type until the discard / gc_gens path clears them.
     *
     * For nonempty buckets gc.data_type is authoritative from the
     * extent walk. For empty buckets gc.data_type is BCH_DATA_free
     * (gc never sets need_*); use the on-disk hint instead so a bucket
     * correctly in need_discard or need_gc_gens compares equal.
     *
     * Using new.data_type unconditionally would break reconstruct_alloc:
     * post-kill_btree the on-disk key is zeros, hint=free, and
     * alloc_data_type() falls into bucket_data_type(free) → free even
     * with dirty_sectors > 0, contradicting the rebuilt counters.
     */
    gc.set_data_type(if !gc.data_type().is_empty() { gc.data_type() } else { new.data_type() });
    if gc.data_type != old_gc.data_type || gc.dirty_sectors != old_gc.dirty_sectors {
        t.alloc_key_to_dev_counters(ca, &old_gc, &gc, UpdateTriggerFlags::GC)?;

        /*
         * Ugly: alloc_key_to_dev_counters(..., BTREE_TRIGGER_gc) is not
         * safe w.r.t. transaction restarts, so fixup the gc_bucket so
         * we don't run it twice:
         */
        let mut m = g.lock.lock();
        m.set_data_type(gc.data_type);
        m.dirty_sectors = gc.dirty_sectors;
    }

    if fsck_err_on!(t, new.data_type != gc.data_type, id::alloc_key_data_type_wrong,
                    "bucket {}:{} gen {} has wrong data_type: got {}, should be {}",
                    inode, offset, gc.generation, new.data_type(), gc.data_type())? {
        new.data_type = gc.data_type;

        /*
         * Reconstruct runs under BTREE_TRIGGER_gc which skips the
         * transactional path that maintains NEED_DISCARD. Set it
         * here on non-empty buckets for downgrade compat (older
         * kernels read the bit to decide need_discard state).
         */
        if !new.data_type().is_empty() &&
           new.data_type() != DataType::sb &&
           new.data_type() != DataType::journal {
            new.set_need_discard(true);
        }
    }

    macro_rules! copy_bucket_field {
        ($err:ident, $f:ident) => {
            if fsck_err_on!(t, new.$f != gc.$f, id::$err,
                            "bucket {}:{} gen {} data type {} has wrong {}: got {}, should be {}",
                            inode, offset, gc.generation, gc.data_type(), stringify!($f),
                            u64::from(new.$f), u64::from(gc.$f))? {
                new.$f = gc.$f;
            }
        };
    }

    copy_bucket_field!(alloc_key_gen_wrong,             generation);
    copy_bucket_field!(alloc_key_dirty_sectors_wrong,   dirty_sectors);
    copy_bucket_field!(alloc_key_stripe_sectors_wrong,  stripe_sectors);
    copy_bucket_field!(alloc_key_cached_sectors_wrong,  cached_sectors);
    copy_bucket_field!(alloc_key_stripe_wrong,          stripe_refcount);

    if !alloc_v4_cmp(&old, &new) {
        return Ok(());
    }

    let mut a = t.alloc_to_v4_mut(k)?;

    *a.v_mut() = new;

    /*
     * The trigger normally makes sure these are set, but we're not running
     * triggers:
     */
    if a.v().data_type() == DataType::cached && a.v().io_time[READ] == 0 {
        a.v_mut().io_time[READ] = (fs.io_clock[READ].now.load(Relaxed) as u64).max(1);
    }

    t.update(iter, &a, UpdateTriggerFlags::NORUN)
}

fn gc_alloc_done(fs: &Fs) -> Result<(), BchError> {
    let trans = btree_trans!(fs);

    for ca in fs.members() {
        let mi = ca.mi.get();
        let dev = u64::from(ca.dev_idx);
        let flags = BtreeIterFlags::SLOTS | BtreeIterFlags::PREFETCH;

        BtreeIter::new(&trans, Btree::alloc, pos(dev, mi.first_bucket.into()), flags)
            .for_each_max_commit(&trans, pos(dev, mi.nbuckets - 1), flags, None, CommitFlags::NO_ENOSPC,
                                 |t, iter, k| alloc_write_key(t, iter, &ca, k))?;
    }

    Ok(())
}

fn gc_alloc_start(fs: &Fs) -> Result<(), BchError> {
    for ca in fs.members() {
        if ca.buckets_gc.prealloc_n(ca.mi.get().nbuckets as usize).is_err() {
            return fs.throw(bch_errcode::BCH_ERR_ENOMEM_gc_alloc_start);
        }
    }

    Ok(())
}

fn gc_write_stripes_key(t: &TransAttempt<'_, '_>, iter: &BtreeIter<'_>, k: BkeySC<'_>) -> Result<(), BchError> {
    let Some(s) = k.arm::<c::Stripe>() else { return Ok(()) };

    let fs = t.fs();
    let m = fs.ec.gc_stripe(k.k.p.offset).map(|m| *m.lock.lock());
    let Some(counts) = s.v().blockcounts() else { return Ok(()) };

    let nr_blocks = usize::from(s.v().nr_blocks);
    let nr_data = nr_blocks - usize::from(s.v().nr_redundant);
    let block_sectors = |i: usize| m.map_or(0, |m| m.block_sectors[i]);

    let mut buf = Printbuf::new();
    let mut bad = false;
    let mut parity_bad = false;
    for (i, count) in counts.iter().enumerate().take(nr_blocks) {
        let old = count.get();
        let new = block_sectors(i);

        if old != new {
            prt!(&mut buf, "stripe block {i} has wrong sector count: got {old}, should be {new}\n");
            bad = true;
        }

        parity_bad |= i >= nr_data && old != 0;
    }

    if bad || parity_bad {
        prt!(&mut buf, "{}", k.to_text(fs));
    }

    if fsck_err_on!(t, parity_bad, id::stripe_parity_block_sector_count_wrong,
                    "stripe parity block with nonzero sector count\n{}", buf.as_str())? ||
       fsck_err_on!(t, bad, id::stripe_sector_count_wrong, "{}", buf.as_str())? {
        let mut new = t.bkey_make_mut_noupdate(s)?;

        let counts = new.v_mut().blockcounts_mut().expect("the block counts it had");
        for (i, count) in counts.iter_mut().enumerate().take(nr_blocks) {
            *count = Le16::new(if i < nr_data { block_sectors(i) } else { 0 });
        }

        t.update(iter, &new, UpdateTriggerFlags::empty())?;
    }

    Ok(())
}

fn gc_stripes_done(fs: &Fs) -> Result<(), BchError> {
    let trans = btree_trans!(fs);
    BtreeIter::new(&trans, Btree::stripes, POS_MIN, BtreeIterFlags::PREFETCH)
        .for_each_commit(&trans, None, CommitFlags::NO_ENOSPC, gc_write_stripes_key)
}

fn gc(fs: &Fs) -> Result<(), BchError> {
    gc_accounting_start(fs)?;
    gc_start(fs)?;
    gc_alloc_start(fs)?;
    gc_reflink_start(fs);

    fs.gc.set_pos(c::gc_pos::of_phase(c::gc_phase::start));

    bch_err_msg!(fs, mark_superblocks(fs), "marking superblocks")?;

    gc_btrees(fs)?;

    gc_alloc_done(fs)?;
    bch_err_fn!(fs, gc_accounting_done(fs))?;
    gc_stripes_done(fs)?;
    gc_reflink_done(fs)
}

/**
 * bch2_check_allocations - walk all references to buckets, and recompute them:
 *
 * @c:			filesystem object
 *
 * Returns: 0 on success, or standard errcode on failure
 *
 * Order matters here:
 *  - Concurrent GC relies on the fact that we have a total ordering for
 *    everything that GC walks - see  gc_will_visit_node(),
 *    gc_will_visit_root()
 *
 *  - also, references move around in the course of index updates and
 *    various other crap: everything needs to agree on the ordering
 *    references are allowed to move around in - e.g., we're allowed to
 *    start with a reference owned by an open_bucket (the allocator) and
 *    move it to the btree, but not the reverse.
 *
 *    This is necessary to ensure that gc doesn't miss references that
 *    move around - if references move backwards in the ordering GC
 *    uses, GC could skip past them
 */
fn check_allocations(fs: &Fs) -> Result<(), BchError> {
    let _state = fs.state_lock_read();
    let _gc = fs.gc.lock.write();

    fs.btree_interior_updates_flush();

    let ret = gc(fs);

    {
        let mut mark = fs.mark_lock().write();
        /* Indicates that gc is no longer in progress: */
        fs.gc.set_not_running();
        unsafe { gc_free(fs, &mut mark) };
    }

    /*
     * At startup, allocations can happen directly instead of via the
     * allocator thread - issue wakeup in case they blocked on gc_lock:
     */
    fs.alloc_wake_all();

    if ret.is_ok() && !fs.flags.get(c::bch_fs_flags::errors_not_fixed) {
        fs.sb_members_clean_deleted();
    }

    ret
}

crate::recovery_pass!(check_allocations => check_allocations);
