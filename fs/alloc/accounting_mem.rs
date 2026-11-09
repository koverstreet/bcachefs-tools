// SPDX-License-Identifier: GPL-2.0

use crate::init::passes::RecoveryPass;
use crate::accounting::{self, accounting_bkey, is_mem, key_to_text,
                        replicas_entry, Accounting, CounterSet, Counters, DiskAccountingPos, MAX_COUNTERS};
use crate::alloc::buckets::bch_fs_usage_base;
use crate::alloc::replicas::{can_read_with_devs, try_replicas_acct};
use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{BkeyI, BkeySC, POS_MIN, SPOS_MAX};
use crate::btree::iter::{commit_do, lockrestart_do, BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags, LoopFlow,
                         TransAttempt};
use crate::c;
use crate::errcode::{bch_errcode, ret_to_c, BchError};
use crate::format::{self, MetadataVersion, Bpos, DataType};
use crate::fs::Fs;
use crate::init::error::{count_fsck_err, id, FsckErrCtx};
use crate::init::passes;
use crate::locking::{percpu_rwsem_noio, ExclusiveGuard, SharedGuard};
use crate::debug::trace::events;
use crate::util::alloc::{AllocError, KVVec};
use crate::data_structures::eytzinger::Eytzinger0;
use crate::util::log::{print_str, LogLevel, LogMsg};
use crate::util::percpu::{Counters as _, CpuGuard, PerCpuRef, PerCpuU64s};
use crate::util::printbuf::{Join, Msg};
use crate::util::rcu;
use crate::util::vstructs::{bytes_of, plain_bytes, ArmError, Unvalidated, Validated};
use crate::util::Printbuf;
use crate::{btree_trans, fsck_err, fsck_err_on, percpu_field};
use core::ffi::c_int;
use cstruct_macros::c_export;
use zerocopy::IntoBytes;

#[derive(Clone, Copy)]
pub(crate) struct Delta<'a> {
    pub p: Bpos,
    pub d: &'a [u64],
}

impl<'a> From<BkeySC<'a, Accounting<Validated>>> for Delta<'a> {
    fn from(k: BkeySC<'a, Accounting<Validated>>) -> Self {
        Delta { p: k.k.p, d: &k.v().d }
    }
}

pub(crate) type MarkRead<'a> = SharedGuard<'a, percpu_rwsem_noio>;
pub(crate) type MarkWrite<'a> = ExclusiveGuard<'a, percpu_rwsem_noio>;

pub(crate) trait MarkHeld {
    fn insert(&mut self, fs: &Fs, a: Delta<'_>) -> Result<(), BchError>;
}

impl MarkHeld for MarkRead<'_> {
    fn insert(&mut self, fs: &Fs, a: Delta<'_>) -> Result<(), BchError> {
        self.with_write(|w| insert_locked(fs, a, w))
    }
}

impl MarkHeld for MarkWrite<'_> {
    fn insert(&mut self, fs: &Fs, a: Delta<'_>) -> Result<(), BchError> {
        insert_locked(fs, a, self)
    }
}

#[derive(Default)]
pub(crate) struct Table {
    entries:    KVVec<Entry>,
    gc_running: bool,
}

pub(crate) struct Entry {
    pos:  Bpos,
    live: PerCpuU64s,
    gc:   Option<PerCpuU64s>,
}

impl Fs {
    fn accounting_mem<'g>(&'g self, _mark: &'g impl MarkHeld) -> &'g Table {
        unsafe { self.accounting.protected() }
    }

    fn accounting_mem_mut<'g>(&'g self, _mark: &'g mut MarkWrite<'_>) -> &'g mut Table {
        unsafe { self.accounting.protected_mut() }
    }
}

impl Table {
    fn find(&self, p: Bpos) -> Option<usize> {
        Eytzinger0::find_by(&self.entries, |e| e.pos.cmp(&p))
    }

    fn find_ge(&self, p: Bpos) -> Option<usize> {
        Eytzinger0::find_ge_by(&self.entries, |e| e.pos.cmp(&p))
    }

    fn insert(&mut self, e: Entry) -> Result<(), AllocError> {
        self.entries.push(e)?;
        Eytzinger0::insert_last_by(&mut self.entries, |l, r| l.pos.cmp(&r.pos));
        Ok(())
    }

    fn remove(&mut self, i: usize) {
        Eytzinger0::remove_to_last(&mut self.entries, i);
        self.entries.pop();
    }

    fn free_gc(&mut self) {
        for e in self.entries.iter_mut() {
            e.gc = None;
        }
    }

    fn with_entries<R>(&mut self, f: impl FnOnce(&mut KVVec<Entry>) -> R) -> R {
        let r = f(&mut self.entries);
        Eytzinger0::sort_by(&mut self.entries, |l, r| l.pos.cmp(&r.pos));
        r
    }
}

impl Entry {
    fn new(pos: Bpos, nr: usize, gc: bool) -> Result<Self, AllocError> {
        let mut e = Entry { pos, live: PerCpuU64s::new(nr)?, gc: None };
        if gc {
            e.alloc_gc()?;
        }
        Ok(e)
    }

    fn nr_counters(&self) -> usize {
        self.live.len()
    }

    fn counters(&self, gc: bool) -> &PerCpuU64s {
        if gc { self.gc.as_ref().expect("gc's counters, while gc runs") } else { &self.live }
    }

    fn sum(&self, gc: bool) -> Counters {
        let mut v = [0; MAX_COUNTERS];
        for (v, c) in v.iter_mut().zip(self.counters(gc).iter()) {
            *v = c.sum();
        }
        v
    }

    fn is_zero(&self) -> bool {
        let zero = |c: &PerCpuU64s| c.iter().all(|c| c.sum() == 0);
        zero(&self.live) && self.gc.as_ref().is_none_or(zero)
    }

    fn alloc_gc(&mut self) -> Result<(), AllocError> {
        self.gc = Some(PerCpuU64s::new(self.nr_counters())?);
        Ok(())
    }
}

pub(crate) fn mem_read_locked<S>(fs: &Fs, mark: &impl MarkHeld, pos: &DiskAccountingPos<S>) -> Counters {
    /*
     * Types not in the mem table read back as zeros - not an error, a
     * silently wrong answer: bch2_snapshot_node_check_no_data()'s empty
     * check read zeros from the day it was added, licensing no_keys
     * transitions that stranded live keys. Non-mem types must be read
     * from the accounting btree.
     */
    debug_assert!(is_mem(pos));

    let acc = fs.accounting_mem(mark);
    acc.find(pos.to_bpos()).map_or([0; MAX_COUNTERS], |i| acc.entries[i].sum(false))
}

pub fn mem_read<S>(fs: &Fs, pos: &DiskAccountingPos<S>) -> Counters {
    mem_read_locked(fs, &fs.mark_lock().read(), pos)
}

/*
 * Read out accounting keys for replicas entries, as an array of
 * bch_replicas_usage entries.
 *
 * Note: this may be deprecated/removed at smoe point in the future and replaced
 * with something more general, it exists to support the ioctl used by the
 * 'bcachefs fs usage' command.
 */
pub fn replicas_usage_read(fs: &Fs) -> Result<KVVec<u8>, BchError> {
    let mark = fs.mark_lock().read();
    let mut out = KVVec::new();
    for e in fs.accounting_mem(&mark).entries.iter() {
        let pos = DiskAccountingPos::from_bpos(e.pos);
        let Some(r) = replicas_entry(&pos) else { continue };

        let sectors = accounting::ReplicasCounters::from_slice(&e.sum(false)).sectors;
        out.extend_from_slice(&sectors.to_ne_bytes())?;
        out.extend_from_slice(bytes_of(r))?;
    }
    Ok(out)
}

pub fn to_keys(fs: &Fs, types_mask: u32) -> Result<KVVec<u8>, BchError> {
    let mark = fs.mark_lock().read();
    let mut out = KVVec::new();
    for e in fs.accounting_mem(&mark).entries.iter() {
        let ty = DiskAccountingPos::from_bpos(e.pos).type_().0;
        if types_mask.checked_shr(ty.into()).unwrap_or(0) & 1 == 0 {
            continue;
        }

        let v = e.sum(false);
        let v = &v[..e.nr_counters()];
        if v.iter().all(|&v| v == 0) {
            continue;
        }

        out.extend_from_slice(plain_bytes(&accounting_bkey(e.pos, v.len())))?;
        out.extend_from_slice(v.as_bytes())?;
    }
    Ok(out)
}

#[cold]
#[inline(never)]
fn insert_locked(fs: &Fs, a: Delta<'_>, mark: &mut MarkWrite<'_>) -> Result<(), BchError> {
    let pos = DiskAccountingPos::from_bpos(a.p);

    /*
     * The replicas entry is checked for under the write lock, not before:
     * between up_read and down_write replicas gc can drop it, as nothing in
     * memory references it until this insert.
     */
    if replicas_entry(&pos).is_some_and(|e| !fs.replicas_marked(e)) {
        return Err(fs.err(bch_errcode::BCH_ERR_btree_insert_need_mark_replicas));
    }

    let acc = fs.accounting_mem_mut(mark);

    /* raced with another insert, already present: */
    if acc.find(a.p).is_some() {
        return Ok(());
    }

    Entry::new(a.p, pos.nr_counters(), acc.gc_running)
        .and_then(|e| acc.insert(e))
        .map_err(|_| fs.err(bch_errcode::BCH_ERR_ENOMEM_disk_accounting))?;
    let nr = acc.entries.len();

    fs.event_trace(events::accounting_mem_insert, |out| {
        write!(out, "entries {nr} added ");
        key_to_text(out, Some(fs), &pos);
        write!(out, " {}", Join(" ", a.d.iter().map(|&d| d as i64)));
    });
    Ok(())
}

pub fn maybe_kill(fs: &Fs, p: Bpos) {
    let pos = DiskAccountingPos::from_bpos(p);
    let Some(e) = replicas_entry(&pos) else { return };

    if u32::from(e.data_type) > DataType::btree.0 &&
       fs.request_incompat_feature(MetadataVersion::no_sb_user_data_replicas).is_ok() {
        return;
    }

    let mut sb = fs.sb_lock();
    {
        let mut mark = fs.mark_lock().write();
        let acc = fs.accounting_mem_mut(&mut mark);
        let Some(i) = acc.find(p) else { return };
        if !acc.entries[i].is_zero() {
            return;
        }

        acc.remove(i);

        fs.replicas_entry_kill(&mut sb, &mark, e);
    }
    fs.write_super(&mut sb);
}

pub(crate) fn mem_gc(fs: &Fs) {
    let mut mark = fs.mark_lock().write();
    fs.accounting_mem_mut(&mut mark).with_entries(|entries| entries.retain(|e| !e.is_zero()));
}

pub(crate) fn gc_accounting_start(fs: &Fs) -> Result<(), BchError> {
    let mut mark = fs.mark_lock().write();
    let acc = fs.accounting_mem_mut(&mut mark);

    let ret = acc.entries.iter_mut().try_for_each(Entry::alloc_gc);
    if ret.is_err() {
        acc.free_gc();
    }
    acc.gc_running = ret.is_ok();
    ret.map_err(|_| fs.err(bch_errcode::BCH_ERR_ENOMEM_disk_accounting))
}

pub(crate) fn gc_accounting_done(fs: &Fs) -> Result<(), BchError> {
    let trans = btree_trans!(fs);
    let mut mark = fs.mark_lock().write();
    let mut pos = POS_MIN;

    loop {
        let acc = fs.accounting_mem(&mark);
        let Some(i) = acc.find_ge(pos) else { return Ok(()) };
        let e = &acc.entries[i];
        pos = e.pos.successor();

        let acc_k = DiskAccountingPos::from_bpos(e.pos);
        if matches!(acc_k.get(), Err(ArmError::Unknown(_))) {
            continue;
        }

        let nr = e.nr_counters();
        let (dst, src) = (e.sum(false), e.sum(true));
        if dst[..nr] == src[..nr] {
            continue;
        }

        let mut buf = Printbuf::new();
        write!(buf, "accounting mismatch for ");
        key_to_text(&mut buf, Some(fs), &acc_k);
        write!(buf, ":\n      got {}\nshould be {}", Join(" ", dst[..nr].iter()), Join(" ", src[..nr].iter()));

        let d: Counters = core::array::from_fn(|i| src[i].wrapping_sub(dst[i]));
        let d = &d[..nr];

        trans.unlock_long();

        if !fsck_err!(fs, id::accounting_mismatch, "{buf}")? {
            continue;
        }

        let acc_k = acc_k.validate().expect("the table's positions are its keys', validated");
        mark.with_unlocked(|| commit_do(&trans, None, CommitFlags::SKIP_ACCOUNTING_APPLY, |t| {
            accounting::mod_normal(t, &acc_k, d)
        }))?;

        if !fs.flags.get(c::bch_fs_flags::may_go_rw) {
            let a = Delta { p: acc_k.to_bpos(), d };
            let usage = mem_add(fs, a, false, &mut mark)
                .expect("an existing entry: entries are only removed by gc_free and exit");

            let cpu = CpuGuard::new();
            percpu_field!(fs.capacity.pcpu.this_cpu(&cpu), counters.usage).add(usage);
        }
    }
}

/// # Safety
/// mark_lock is held for write.
pub(crate) unsafe fn accounting_gc_free(fs: &Fs) {
    let mut mark = unsafe { fs.mark_lock().held() };
    let acc = fs.accounting_mem_mut(&mut mark);
    acc.free_gc();
    acc.gc_running = false;
}

/// # Safety
/// Nothing uses the accounting any more.
pub unsafe fn accounting_exit(fs: &Fs) {
    let acc = unsafe { fs.accounting.protected_mut() };
    acc.entries = KVVec::new();
}

fn read_key(fs: &Fs, entries: &mut KVVec<Entry>, k: BkeySC<'_>) -> Result<(), BchError> {
    let Some(a) = k.arm::<Accounting>() else { return Ok(()) };
    let d = &a.v().d;
    let pos = DiskAccountingPos::from_bpos(k.k.p);

    if let Some(r) = replicas_entry(&pos).filter(|_| d.iter().any(|&v| v != 0)) {
        let flags = match fs.opts().degraded() {
            c::DegradedAction::very => c::BCH_FORCE_IF_DEGRADED | c::BCH_FORCE_IF_LOST,
            c::DegradedAction::yes  => c::BCH_FORCE_IF_DEGRADED,
            _ => 0,
        };

        let mut buf = Printbuf::new();
        if !can_read_with_devs(r, &c::bch_devs_mask { d: fs.devs_online.load() }, flags, Some(&mut buf)) {
            fs.missing_devs_to_text(&mut buf);
            print_str(fs, LogLevel::ERR, &mut buf);
            return Err(fs.err(bch_errcode::BCH_ERR_insufficient_devices_to_start));
        }
    }

    if !is_mem(&pos) {
        return Ok(());
    }

    /*
     * Accumulate into last entry if same bpos - this happens when both
     * btree and journal keys exist for the same accounting entry
     * (journal key = delta on top of btree base value).
     *
     * The btree/journal walk in bch2_accounting_read() processes keys
     * in bpos order, so duplicates are always adjacent.
     */
    if let Some(e) = entries.last_mut().filter(|e| e.pos == k.k.p) {
        let mut v = e.sum(false);
        for (v, &d) in v.iter_mut().zip(d) {
            *v = v.wrapping_add(d);
        }
        let nr = e.nr_counters();
        e.live.set(&v[..nr]);
        return Ok(());
    }

    let mut e = Entry::new(k.k.p, pos.nr_counters(), false)
        .map_err(|_| fs.err(bch_errcode::BCH_ERR_ENOMEM_disk_accounting))?;
    e.live.set(d);
    entries.push(e).map_err(|_| fs.err(bch_errcode::BCH_ERR_ENOMEM_disk_accounting))
}

/// Journal key @i's newer deltas, the keys after it at its position, added
/// into it and marked overwritten: the index past them.
fn accumulate_newer(ctx: &impl FsckErrCtx, fs: &Fs, jk: &c::journal_keys, i: usize) -> Result<usize, BchError> {
    let keys = jk.keys();
    let key = |j: usize| jk.key(&keys[j]);
    let same_pos = |j: usize| j < keys.len() && jk.key_cmp(&keys[i], &keys[j]).is_eq();

    /* Not a key to add deltas into: the accounting keys after it are a run of their own */
    if BkeySC::from(key(i)).arm::<Accounting>().is_none() {
        return Ok(i + 1);
    }

    /*
     * Every version checked before any delta is added: a restart from the
     * fsck error retries the key.
     */
    let mut end = i + 1;
    let mut version = key(i).k.bversion;
    while same_pos(end) {
        let n = key(end);
        if BkeySC::from(n).arm::<Accounting>().is_some() {
            if { n.k.bversion } <= version {
                let mut buf = Printbuf::new();
                write!(buf, "accounting keys with out of order versions:");
                for j in [i, end] {
                    buf.newline();
                    match keys[j].journal_pos() {
                        Some(p) => write!(buf, "{}.{} ", p.journal_seq_offset, p.journal_offset),
                        None    => write!(buf, "(inserted) "),
                    };
                    write!(buf, "{}", BkeySC::from(key(j)).to_text(fs));
                }
                fsck_err!(ctx, id::accounting_key_version_out_of_order, "{buf}")?;
            } else {
                version = n.k.bversion;
            }
        }
        end += 1;
    }

    let k = unsafe { &mut *BkeyI::from_raw_mut(jk.key_k(&keys[i])) };
    for j in i + 1..end {
        let n = key(j);
        if BkeySC::from(n).arm::<Accounting>().is_some() {
            accounting::accumulate(k, BkeySC::from(n));
            jk.mark_overwritten(j);
        }
    }
    Ok(end)
}

fn accumulate_and_read(ctx: &impl FsckErrCtx, fs: &Fs, journal_keys: &c::journal_keys,
                       entries: &mut KVVec<Entry>, i: usize) -> Result<usize, BchError>
{
    let next = accumulate_newer(ctx, fs, journal_keys, i)?;
    read_key(fs, entries, journal_keys.key(&journal_keys.keys()[i]).into())?;
    Ok(next)
}

fn read_walk(trans: &BtreeTrans<'_>, journal_keys: &c::journal_keys, entries: &mut KVVec<Entry>)
    -> Result<(), BchError>
{
    let fs = trans.fs();
    let keys = journal_keys.keys();
    let cmp = |p: Bpos, j: usize| journal_keys.key_cmp_pos(format::Btree::accounting, 0, p, &keys[j]);

    /* Find the range of accounting keys from the journal: */
    let mut jk = journal_keys.search(format::Btree::accounting, 0, POS_MIN);
    let end = journal_keys.search(format::Btree::accounting, 0, SPOS_MAX);

    /*
     * Iterate over btree and journal accounting simultaneously:
     *
     * We want to drop unneeded unneeded accounting deltas early - deltas
     * that are older than the btree accounting key version, and once we've
     * dropped old accounting deltas we can accumulate and compact deltas
     * for the same key:
     */
    let mut iter = BtreeIter::new(trans, format::Btree::accounting, POS_MIN,
                                  BtreeIterFlags::PREFETCH | BtreeIterFlags::ALL_SNAPSHOTS);
    iter.clear_flags(BtreeIterFlags::WITH_JOURNAL);
    iter.for_each(trans, |t, _, k| {
        if k.arm::<Accounting>().is_none() {
            return Ok(LoopFlow::Continue);
        }

        while jk < end && cmp(k.k.p, jk).is_gt() {
            jk = accumulate_and_read(t, fs, journal_keys, entries, jk)?;
        }

        while jk < end && cmp(k.k.p, jk).is_eq() &&
              { journal_keys.key(&keys[jk]).k.bversion } <= { k.k.bversion } {
            journal_keys.mark_overwritten(jk);
            jk += 1;
        }

        if jk < end && cmp(k.k.p, jk).is_eq() {
            jk = accumulate_and_read(t, fs, journal_keys, entries, jk)?;
        }

        let acc_k = DiskAccountingPos::from_bpos(k.k.p);
        if matches!(acc_k.get(), Err(ArmError::Unknown(_))) {
            return Ok(LoopFlow::Break(()));
        }

        if !is_mem(&acc_k) {
            let next = acc_k.next_type_start();
            let next = if jk < end { next.min(journal_keys.key_pos(&keys[jk])) } else { next };
            return Ok(LoopFlow::ContinueFrom(next));
        }

        read_key(fs, entries, k)?;
        Ok(LoopFlow::Continue)
    })?;

    lockrestart_do(trans, |t| {
        while jk < end {
            jk = accumulate_and_read(t, fs, journal_keys, entries, jk)?;
        }
        Ok(())
    })
}

fn invalid_dev(t: &TransAttempt<'_, '_>, pos: &DiskAccountingPos<Unvalidated>, v: &[u64], dev: u32)
    -> Result<(), BchError>
{
    let fs = t.fs();
    let key = Msg(|out: &mut Printbuf| key_to_text(out, Some(fs), pos));

    if !fsck_err!(t, id::accounting_to_invalid_device, "accounting entry points to invalid device {dev}\n{key}")? {
        return Err(fs.err(bch_errcode::BCH_ERR_remove_disk_accounting_entry));
    }

    let neg: [i64; MAX_COUNTERS] = core::array::from_fn(|i| v.get(i).map_or(0, |&v| (v as i64).wrapping_neg()));
    let pos = pos.validate().expect("the table's positions are its keys', validated");
    accounting::add_at(t, &pos, &neg[..v.len()], false)?;
    t.commit(None, CommitFlags::NO_ENOSPC)?;
    Err(bch_errcode::BCH_ERR_remove_disk_accounting_entry.into())
}

fn validate_late(t: &TransAttempt<'_, '_>, pos: &DiskAccountingPos<Unvalidated>, v: &[u64])
    -> Result<(), BchError>
{
    let fs = t.fs();

    if let Some(r) = replicas_entry(pos) {
        let missing = r.devs.iter().find(|&&d| u32::from(d) != c::BCH_SB_MEMBER_INVALID && !fs.dev_exists(d.into()));
        if let Some(&dev) = missing {
            return invalid_dev(t, pos, v, dev.into());
        }

        /*
         * All replicas entry checks except for invalid device are done
         * in bch2_accounting_validate
         */
        assert!(r.validate(|d| fs.dev_exists(d.into()), &mut Printbuf::new()).is_ok());

        let key = Msg(|out: &mut Printbuf| key_to_text(out, Some(fs), pos));
        if fsck_err_on!(t, !fs.replicas_marked(r), id::accounting_replicas_not_marked,
                        "accounting not marked in superblock replicas\n{key}")? {
            fs.mark_replicas(r)?;
            /*
             * bch2_mark_replicas() updates the superblock, not the
             * transaction, so commit here to flush the fsck_err log
             * entry - the caller loops back into a lockrestart_do
             * whose trans_begin would otherwise discard it.
             */
            t.commit(None, CommitFlags::NO_ENOSPC)?;
        }
    } else if let Ok(accounting::DiskAccountingPosRef::dev_data_type(d)) = pos.get() {
        if !fs.dev_exists(d.dev.into()) {
            return invalid_dev(t, pos, v, d.dev.into());
        }
    }
    Ok(())
}

fn check_sanity(out: &mut Printbuf, fs: &Fs, pos: &DiskAccountingPos<Unvalidated>, v: &[u64]) {
    /* Ratelimit... */
    if out.as_bytes().len() > 2048 {
        return;
    }

    /*
     * Check for underflow, schedule check_allocations necessary:
     *
     * XXX - see if we can factor this out to run on a bkey
     * so we can check everything lazily, right now we don't
     * check the non in-mem counters at all
     */
    if v.iter().any(|&v| (v as i64) < 0) {
        write!(out, "Accounting underflow for\n");
        key_to_text(out, Some(fs), pos);
        write!(out, " {}", Join(" ", v.iter().map(|&v| v as i64)));
        out.newline();
        out.set_suppressed(false);
    }
}

fn read_mem_fixups(trans: &BtreeTrans<'_>) -> Result<(), BchError> {
    let fs = trans.fs();

    /*
     * Validated before anything is dropped: validate_late() commits, and a
     * commit finds its entry in the table - sorted, and whole.
     */
    let mut pos = POS_MIN;
    loop {
        let (p, v, nr) = {
            let mark = fs.mark_lock().read();
            let acc = fs.accounting_mem(&mark);
            let Some(i) = acc.find_ge(pos) else { break };
            let e = &acc.entries[i];
            (e.pos, e.sum(false), e.nr_counters())
        };
        pos = p.successor();

        /*
         * If the entry counters are zeroed, it should be treated as
         * nonexistent - it might point to an invalid device.
         *
         * Drop it, so that if it's re-added it gets re-marked in the
         * superblock:
         */
        if v[..nr].iter().all(|&v| v == 0) {
            continue;
        }

        match lockrestart_do(trans, |t| validate_late(t, &DiskAccountingPos::from_bpos(p), &v[..nr])) {
            Err(e) if e.matches(bch_errcode::BCH_ERR_remove_disk_accounting_entry) => {
                let mut mark = fs.mark_lock().write();
                let acc = fs.accounting_mem_mut(&mut mark);
                let i = acc.find(p).expect("an entry, found above: none are dropped till below");
                acc.entries[i].live.set(&[]);
            }
            r => r?,
        }
    }

    /*
     * Filter zeroed/invalid entries with an in-place forward pass.
     *
     * We previously did this reverse-iterating + darray_remove_item per
     * dropped entry, which is O(N*R) memmove (each remove shifts the tail
     * back). On large arrays the replicas table grows combinatorially with
     * device count, and many entries come back zero on read, so the cost
     * was dominating mount time on big multi-device filesystems.
     */
    mem_gc(fs);

    let mut underflow_err = LogMsg::new(fs);
    underflow_err.set_suppressed(true);

    let mut usage = bch_fs_usage_base::default();
    {
        let mark = fs.mark_lock().read();
        for e in fs.accounting_mem(&mark).entries.iter() {
            let k = DiskAccountingPos::from_bpos(e.pos);
            let v = e.sum(false);

            check_sanity(&mut underflow_err, fs, &k, &v[..e.nr_counters()]);

            let (u, _) = usage_of(fs, &k, &v, |c| {
                for (c, v) in c.into_iter().zip(v) {
                    unsafe { c.set(v) };
                }
            });
            usage = usage.wrapping_add(u);
        }
    }
    /*
     * Set, as the devices' usage is, not added to: the fixups' commits added
     * the deltas of the entries they went on to drop.
     */
    let base = percpu_field!(fs.capacity.pcpu.get(), counters.usage);
    unsafe { base.zero() };
    base.this_cpu(&CpuGuard::new()).add(usage);

    if !underflow_err.is_suppressed() {
        let log = count_fsck_err(fs, id::accounting_key_underflow, &mut underflow_err);
        underflow_err.set_suppressed(!log);
        passes::run_explicit(fs, &mut underflow_err, RecoveryPass::check_allocations,
                             c::bch_run_recovery_pass_flags::empty())?;
    }
    Ok(())
}

/*
 * At startup time, initialize the in memory accounting from the btree (and
 * journal)
 */
fn read(fs: &Fs) -> Result<(), BchError> {
    let trans = btree_trans!(fs);

    /*
     * We might run more than once if we rewind to start topology repair or
     * btree node scan - and those might cause us to get different results,
     * so we can't just skip if we've already run.
     *
     * Free and re-initialize accounting - entries are appended unsorted
     * during the read and sorted once at the end, so we need a clean slate.
     */
    fs.accounting_mem_mut(&mut fs.mark_lock().write()).entries.clear();
    for ca in fs.members() {
        unsafe { ca.usage.get().zero() };
    }
    let usage = percpu_field!(fs.capacity.pcpu.get(), counters.usage);
    unsafe { usage.zero() };

    let journal_keys = fs.journal_keys_get().expect("accounting read after recovery was done with the journal keys");
    let journal_keys = journal_keys.value();
    let mut entries = KVVec::new();
    let walked = read_walk(&trans, journal_keys, &mut entries);

    /*
     * Entries were inserted unsorted during the btree/journal walk above.
     * Sort now, before fixups which may need eytzinger lookups.
     */
    {
        let mut mark = fs.mark_lock().write();
        fs.accounting_mem_mut(&mut mark).with_entries(|e| *e = entries);
    }
    walked?;

    trans.unlock();
    unsafe { journal_keys.drop_overwritten() };

    /* Assert no duplicates - the btree/journal walk must produce unique keys */
    {
        let mark = fs.mark_lock().read();
        let e = &fs.accounting_mem(&mark).entries;
        let ez = Eytzinger0::<usize>::new(e.len());
        let mut inorder = ez.iter().map(|i| e[i].pos);
        if let Some(mut prev) = inorder.next() {
            for p in inorder {
                assert!(prev < p, "duplicate or out of order accounting entries: {prev} then {p}");
                prev = p;
            }
        }
    }

    /*
     * read_mem_fixups() does transaction commits to
     * fix invalid accounting entries, so we can't hold mark_lock
     * across it - the commit path takes mark_lock itself.
     */
    read_mem_fixups(&trans)
}
crate::recovery_pass!(accounting_read => read);

/// Whether accounting is kept for @e - or it's an entry no accounting
/// position here holds, a newer version's, and not ours to drop.
pub(crate) fn has_replicas(fs: &Fs, mark: &impl MarkHeld, e: &c::bch_replicas_entry_v1) -> bool {
    let Some(pos) = try_replicas_acct(DataType(e.data_type.into()), e.nr_required, &e.devs) else { return true };
    fs.accounting_mem(mark).find(pos.to_bpos()).is_some()
}

/// @dev gets a dev_data_type key's device counters - buckets, sectors,
/// fragmented - under RCU.
pub(crate) fn usage_of<R>(fs: &Fs, pos: &DiskAccountingPos<Unvalidated>, d: &[u64],
                          dev: impl FnOnce([PerCpuRef<'_, u64>; 3]) -> R) -> (bch_fs_usage_base, Option<R>)
{
    use accounting::DiskAccountingPosRef as A;

    let mut usage = bch_fs_usage_base::default();
    let mut ret = None;
    match pos.get() {
        Ok(A::persistent_reserved(r)) => usage.reserved =
            u64::from(r.nr_replicas).wrapping_mul(accounting::PersistentReservedCounters::from_slice(d).sectors),
        Ok(A::replicas(r)) =>
            usage.add_data_type(DataType(r.data_type.into()), accounting::ReplicasCounters::from_slice(d).sectors),
        Ok(A::dev_data_type(r)) => {
            let rcu = rcu::read_lock();
            if let (Some(ca), Some(t)) = (fs.dev_rcu(r.dev.into(), &rcu), r.known_data_type()) {
                if t.is_hidden() {
                    let buckets = accounting::DevDataTypeCounters::from_slice(d).buckets;
                    usage.hidden = buckets.wrapping_mul(ca.mi.get().bucket_size.into());
                }
                let i = t.0 as usize;
                ret = Some(dev([percpu_field!(ca.usage.get(), d[i].buckets),
                                percpu_field!(ca.usage.get(), d[i].sectors),
                                percpu_field!(ca.usage.get(), d[i].fragmented)]));
            }
        }
        _ => {}
    }
    (usage, ret)
}

/*
 * Update in memory counters so they match the btree update we're doing; called
 * from transaction commit path
 */
#[inline]
pub(crate) fn mem_add<M: MarkHeld>(fs: &Fs, a: Delta<'_>, gc: bool, mark: &mut M)
    -> Result<bch_fs_usage_base, BchError>
{
    if gc && !fs.accounting_mem(mark).gc_running {
        return Ok(Default::default());
    }

    let pos = DiskAccountingPos::from_bpos(a.p);
    if !is_mem(&pos) {
        return Ok(Default::default());
    }

    let idx = loop {
        if let Some(i) = fs.accounting_mem(mark).find(a.p) {
            break i;
        }
        mark.insert(fs, a)?;
    };

    for (c, &d) in fs.accounting_mem(mark).entries[idx].counters(gc).iter().zip(a.d) {
        c.add(d);
    }

    if gc {
        return Ok(Default::default());
    }
    Ok(usage_of(fs, &pos, a.d, |c| {
        for (c, &d) in c.into_iter().zip(a.d) {
            c.add(d);
        }
    }).0)
}

#[c_export]
pub unsafe fn bch2_accounting_mem_add(trans: &BtreeTrans<'_>, k: BkeySC<'_>) -> c_int {
    let fs = trans.fs();
    let a = Delta::from(k.arm::<Accounting>().expect("an accounting key"));
    let usage = mem_add(fs, a, false, &mut unsafe { fs.mark_lock().read_held() });
    ret_to_c(usage.map(|u| trans.fs_usage_delta_add(u)))
}
