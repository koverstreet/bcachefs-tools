// SPDX-License-Identifier: GPL-2.0

//! A device's journal buckets, in the order they're written - the
//! superblock's - each with the newest seq written to it, and the cursors
//! where each stage of a bucket's life starts:
//!
//!   [discard, dirty_ondisk)   reclaimed, not yet discarded
//!   [dirty_ondisk, dirty)     clean, but needed until last_seq_ondisk passes it
//!   [dirty, cur)              dirty
//!   cur                       being written
//!   (cur, discard)            discarded: free
//!
//! The journal lock's; the list changes with sb_lock held too, as the
//! superblock does.

use crate::c;
use crate::c::journal_bucket_stage::{self as Stage, JOURNAL_BUCKET_cur as CUR, JOURNAL_BUCKET_dirty as DIRTY,
                                     JOURNAL_BUCKET_dirty_ondisk as DIRTY_ONDISK,
                                     JOURNAL_BUCKET_discard as DISCARD};
use pin_init::stack_pin_init;

use crate::alloc::buckets::{trans_mark_metadata_bucket, DiskReservation};
use crate::btree::iter::{lockrestart_do, UpdateTriggerFlags};
use crate::errcode::{bch_errcode, BchError};
use crate::format::{DataType, SbFeature};
use crate::fs::{Dev, Fs, SbLockGuard};
use crate::journal::sb::buckets_to_sb;
use crate::c::JournalGuard;
use crate::locking::{Guard as _, LockedBy};
use crate::util::alloc::KVVec;
use crate::util::closure::StackClosure;
use crate::util::kernel::totalram_bytes;
use crate::util::projections::Volatile;
use crate::util::Printbuf;
use crate::{bch_err, bch_err_dev, bch_err_fn, bch_err_fn_dev, bch_err_msg, btree_trans, prt};

const STAGES: [Stage; 4] = [DISCARD, DIRTY_ONDISK, DIRTY, CUR];

pub struct JournalRing {
    buckets: KVVec<u64>,
    seq:     KVVec<u64>,
    cursors: [u32; STAGES.len()],
    /*
     * Buckets fsck found another user's data in, still needed: never written
     * again, and dropped once reclaim has them awaiting discard
     */
    retired: KVVec<u64>,
    /* the journal read's: the newest seq it's found, in cur */
    highest_seq_found: u64,
}

impl JournalRing {
    pub fn nr(&self) -> u32 {
        self.buckets.len() as u32
    }

    pub fn buckets(&self) -> &[u64] {
        &self.buckets
    }

    pub fn seqs(&self) -> &[u64] {
        &self.seq
    }

    pub fn idx(&self, s: Stage) -> u32 {
        self.cursors[s as usize]
    }

    pub fn bucket(&self, s: Stage) -> u64 {
        self.buckets()[self.idx(s) as usize]
    }

    pub fn seq(&self, s: Stage) -> u64 {
        self.seqs()[self.idx(s) as usize]
    }

    /// The seq of the bucket @k on from stage @s's first.
    pub fn seq_ahead(&self, s: Stage, k: u32) -> u64 {
        let i = self.idx(s) + k;
        self.seqs()[if i >= self.nr() { i - self.nr() } else { i } as usize]
    }

    fn next(&self, i: u32) -> u32 {
        if i + 1 == self.nr() { 0 } else { i + 1 }
    }

    fn ahead(&self, from: u32, to: u32) -> u32 {
        if to >= from { to - from } else { to + self.nr() - from }
    }

    fn dist(&self, from: Stage, to: Stage) -> u32 {
        self.ahead(self.idx(from), self.idx(to))
    }

    /* Bucket @i in [dirty_ondisk, cur]: what's in it needed, or being written */
    fn needed(&self, i: u32) -> bool {
        self.ahead(self.idx(DIRTY_ONDISK), i) <= self.dist(DIRTY_ONDISK, CUR)
    }

    fn retired(&self, b: u64) -> bool {
        self.retired.contains(&b)
    }

    /// @b is the journal's: in the ring, and not retired.
    pub fn owns(&self, b: u64) -> bool {
        self.buckets.contains(&b) && !self.retired(b)
    }

    /* The first awaiting discard, if it's retired: dropped, not discarded */
    fn retired_awaiting_discard(&self) -> Option<usize> {
        (self.can_advance(DISCARD) && self.retired(self.bucket(DISCARD))).then(|| self.idx(DISCARD) as usize)
    }

    /* Any retired bucket the journal's done with */
    fn retired_unneeded(&self) -> Option<usize> {
        (0..self.nr()).find(|&i| self.retired(self.buckets()[i as usize]) && !self.needed(i)).map(|i| i as usize)
    }

    /* Not onto the next stage's start - nor cur, a bucket, onto discard */
    fn can_advance(&self, s: Stage) -> bool {
        let to = self.idx(STAGES[(s as usize + 1) % STAGES.len()]);
        self.nr() != 0 && match s {
            CUR => self.next(self.idx(s)) != to,
            _   => self.idx(s) != to,
        }
    }

    fn advance(&mut self, s: Stage) {
        assert!(self.can_advance(s), "journal bucket stage {} advancing past the next", s as u32);
        self.cursors[s as usize] = self.next(self.idx(s));
    }

    fn advance_while(&mut self, s: Stage, f: impl Fn(u64) -> bool) {
        while self.can_advance(s) && f(self.seq(s)) {
            self.advance(s);
        }
    }

    /// Buckets wholly before @last_seq stop being dirty, and those before
    /// @last_seq_ondisk being needed.
    pub fn last_seq(&mut self, last_seq: u64, last_seq_ondisk: u64) {
        self.advance_while(DIRTY,        |seq| seq < last_seq);
        self.advance_while(DIRTY_ONDISK, |seq| seq < last_seq_ondisk);
    }

    /// Whether last_seq_ondisk reaching @seq would make a bucket reclaimable.
    pub fn would_free(&self, seq: u64) -> bool {
        self.can_advance(DIRTY_ONDISK) && self.seq(DIRTY_ONDISK) < seq
    }

    /// The free buckets, if those before @upto had been discarded.
    pub fn free(&self, upto: Stage) -> u32 {
        match (self.nr(), self.dist(CUR, upto)) {
            (0, _)  => 0,
            (nr, 0) => nr - 1,
            (_, d)  => d - 1,
        }
    }

    /// The buckets a journal entry may use, counting @from's as free.
    pub fn available(&self, from: c::journal_space_from) -> u32 {
        use c::journal_space_from::*;

        let available = self.free(match from {
            journal_space_discarded     => DISCARD,
            journal_space_clean_ondisk  => DIRTY_ONDISK,
            journal_space_clean         => DIRTY,
            _                           => panic!("journal space from {}", from as u32),
        });

        /*
         * Don't use the last bucket unless writing the new last_seq
         * will make another bucket available:
         */
        available - (available != 0 && !self.can_advance(DIRTY_ONDISK)) as u32
    }

    /// cur, on to the next bucket - a free one - which @seq is written to.
    pub fn next_bucket(&mut self, seq: u64) {
        self.advance(CUR);
        self.set_cur_seq(seq);
    }

    pub fn set_cur_seq(&mut self, seq: u64) {
        let i = self.idx(CUR) as usize;
        self.seq[i] = seq;
    }

    /// @bucket, first awaiting discard when it was issued, discarded - if
    /// it's still first: a splice may have moved it.
    pub fn discarded(&mut self, bucket: u64) {
        if self.can_advance(DISCARD) && self.bucket(DISCARD) == bucket {
            self.advance(DISCARD);
        }
    }

    /* The journal read's */

    pub fn seqs_mut(&mut self) -> &mut [u64] {
        &mut self.seq
    }

    pub fn set_cur(&mut self, i: u32) {
        assert!(i < self.nr(), "journal cur bucket {i} past the end ({})", self.nr());
        self.cursors[CUR as usize] = i;
    }

    /// Bucket @i cur, if @seq, read from it, is the newest found yet.
    pub fn set_cur_if_newest(&mut self, i: u32, seq: u64) -> bool {
        let newest = seq > self.highest_seq_found;
        if newest {
            self.highest_seq_found = seq;
            self.set_cur(i);
        }
        newest
    }

    /// Every bucket but cur dirty: reclaim works out which aren't.
    pub fn start(&mut self) {
        let first = self.next(self.idx(CUR));
        for s in [DISCARD, DIRTY_ONDISK, DIRTY] {
            self.cursors[s as usize] = first;
        }
    }

    /*
     * Changing the list: into arrays the caller allocated outside the lock,
     * the old ones given back to free outside it too
     */

    fn replace(&mut self, buckets: KVVec<u64>, seq: KVVec<u64>) -> [KVVec<u64>; 2] {
        [core::mem::replace(&mut self.buckets, buckets), core::mem::replace(&mut self.seq, seq)]
    }

    fn splice(&mut self, s: Splice, buckets: KVVec<u64>, mut seq: KVVec<u64>) -> [KVVec<u64>; 2] {
        s.apply(&mut seq, self.seqs());
        for c in &mut self.cursors {
            *c = s.cursor(*c, seq.len());
        }
        let old = self.replace(buckets, seq);
        let buckets = &self.buckets;
        self.retired.retain(|b| buckets.contains(b));
        old
    }

    pub fn to_text(&self, out: &mut Printbuf) {
        prt!(out, "discard_idx\t{}\n", self.idx(DISCARD));
        for (name, s) in [("dirty_ondisk", DIRTY_ONDISK), ("dirty_idx", DIRTY), ("cur_idx", CUR)] {
            prt!(out, "{}\t{} (seq {})\n", name, self.idx(s), self.seq(s));
        }
        if !self.retired.is_empty() {
            prt!(out, "retired\t{:?}\n", &self.retired[..]);
        }
    }
}

/* @removed buckets at @pos taken out, and @inserted put in */
#[derive(Clone, Copy)]
struct Splice {
    pos:      usize,
    removed:  usize,
    inserted: usize,
}

impl Splice {
    /// @old into @dst, around the inserted - left as they are in @dst.
    fn apply<T: Copy>(self, dst: &mut [T], old: &[T]) {
        dst[..self.pos].copy_from_slice(&old[..self.pos]);
        dst[self.pos + self.inserted..].copy_from_slice(&old[self.pos + self.removed..]);
    }

    /*
     * Cursor @i, past those taken out or put in before it - or, taken out
     * itself, onto the first after them: the start, if that's the end
     */
    fn cursor(self, i: u32, nr: usize) -> u32 {
        let i = i as usize;
        let i = if i >= self.pos + self.removed {
            i - self.removed + self.inserted
        } else if i >= self.pos {
            self.pos
        } else {
            i
        };
        if i == nr { 0 } else { i as u32 }
    }
}

/* The ring, and its nr for those without the lock */
pub type JournalBuckets = LockedBy<JournalRing, Volatile<u32>, c::JournalLockAt>;

pub fn new() -> JournalBuckets {
    let ring = JournalRing {
        buckets: KVVec::new(),
        seq:     KVVec::new(),
        cursors: [0; STAGES.len()],
        retired: KVVec::new(),
        highest_seq_found: 0,
    };
    LockedBy::new(ring, pin_init::Zeroable::zeroed())
}

impl Dev {
    pub fn journal_ring<'g>(&'g self, j: &'g JournalGuard<'_>) -> &'g JournalRing {
        j.get_of(&self.journal.buckets)
    }

    pub fn journal_ring_mut<'g>(&'g self, j: &'g mut JournalGuard<'_>) -> &'g mut JournalRing {
        j.get_mut_of(&self.journal.buckets)
    }

    pub fn with_journal_ring<R>(&self, f: impl FnOnce(&mut JournalRing) -> R) -> R {
        f(self.journal_ring_mut(&mut self.fs().journal.lock.lock()))
    }
}

impl c::journal {
    /// Whether @ring's discard cursor should move: not past a retired bucket,
    /// whose removal is queued instead.
    pub(crate) fn should_discard_bucket(&self, ring: &JournalRing) -> bool {
        if ring.retired_awaiting_discard().is_some() {
            self.drop_retired_work.queue(self.fs().write_ref_wq());
            return false;
        }

        ring.can_advance(DISCARD) &&
            ring.available(c::journal_space_from::journal_space_discarded) < (ring.nr() / 2).max(4)
    }
}

impl c::journal_device {
    pub fn nr(&self) -> u32 {
        self.buckets.get()
    }
}

fn enomem(fs: &Fs) -> impl Fn(crate::util::alloc::AllocError) -> BchError + '_ {
    |_| fs.err(bch_errcode::BCH_ERR_ENOMEM_set_nr_journal_buckets)
}

/* journal_v2's runs, or v1's buckets if there's no v2 */
fn sb_buckets(sb: &c::bch_sb) -> impl Iterator<Item = u64> + '_ {
    let v2 = sb.field::<c::bch_sb_field_journal_v2>();
    let v1 = sb.field::<c::bch_sb_field_journal>().filter(|_| v2.is_none());

    v2.into_iter().flat_map(|j| &j.d).flat_map(|d| d.start.get()..d.start.get() + d.nr.get())
        .chain(v1.into_iter().flat_map(|j| &j.buckets).map(|b| b.get()))
}

/// @ca's buckets, from its superblock @sb: at device online.
pub fn load(ca: &Dev, sb: &c::bch_sb) -> Result<(), BchError> {
    let fs = ca.fs();
    let enomem = |_| fs.err(bch_errcode::BCH_ERR_ENOMEM_dev_journal_init);
    let mut buckets = KVVec::from_elem(0, sb_buckets(sb).count()).map_err(enomem)?;
    for (dst, b) in buckets.iter_mut().zip(sb_buckets(sb)) {
        *dst = b;
    }
    let seq = KVVec::from_elem(0, buckets.len()).map_err(enomem)?;

    let old = ca.with_journal_ring(|ring| {
        ca.journal.buckets.set(buckets.len() as u32);
        ring.cursors = [0; STAGES.len()];
        ring.retired.clear();
        ring.replace(buckets, seq)
    });
    drop(old);
    Ok(())
}

/// Sorted, in the superblock too: at a clean mount, before the journal read
/// has given them seqs.
pub(crate) fn sort(fs: &Fs, sb: &mut SbLockGuard<'_>, ca: &Dev) -> Result<(), BchError> {
    let mut buckets = KVVec::from_elem(0, ca.journal.nr() as usize).map_err(enomem(fs))?;
    ca.with_journal_ring(|ring| {
        ring.buckets.sort_unstable();
        buckets.copy_from_slice(ring.buckets());
    });
    buckets_to_sb(fs, sb, ca, &buckets)
}

/*
 * @removed buckets out of the list at @pos's position, and @inserted put in
 * there - or nothing, if @pos says None: whether the list changed.
 *
 * The superblock lists every bucket the ring might write: one going in goes
 * into the superblock first, then the ring; one going out, out of the ring
 * first - in the look @pos decided it in, its stage as @pos saw it.
 */
fn splice(fs: &Fs, sb: &mut SbLockGuard<'_>, ca: &Dev, removed: usize, inserted: &[u64],
          pos: impl FnOnce(&mut JournalRing) -> Result<Option<usize>, BchError>) -> Result<bool, BchError> {
    /* sb_lock held, the list doesn't change between our looks at it */
    let alloc = || KVVec::from_elem(0, ca.journal.nr() as usize + inserted.len()).map_err(enomem(fs));
    let (mut list, mut buckets, mut seq) = (alloc()?, alloc()?, alloc()?);

    let commit = |ring: &mut JournalRing, s, buckets: KVVec<u64>, seq| {
        ca.journal.buckets.set(buckets.len() as u32);
        ring.splice(s, buckets, seq)
    };

    let Some((old, pending)) = ca.with_journal_ring(|ring| {
        let Some(pos) = pos(ring)? else { return Ok(None) };
        let nr = ring.nr() as usize - removed + inserted.len();
        for v in [&mut list, &mut buckets, &mut seq] {
            v.truncate(nr);
        }
        let s = Splice { pos, removed, inserted: inserted.len() };
        s.apply(&mut list, ring.buckets());
        list[s.pos..s.pos + s.inserted].copy_from_slice(inserted);
        buckets.copy_from_slice(&list);

        Ok::<_, BchError>(Some(if removed != 0 {
            (Some(commit(ring, s, buckets, seq)), None)
        } else {
            (None, Some((s, buckets, seq)))
        }))
    })? else {
        return Ok(false);
    };
    drop(old);

    buckets_to_sb(fs, sb, ca, &list)?;
    fs.write_super_ret(sb)?;

    if let Some((s, buckets, seq)) = pending {
        drop(ca.with_journal_ring(|ring| commit(ring, s, buckets, seq)));
    }
    Ok(true)
}

fn space_available(fs: &Fs) {
    fs.journal.space_available(&mut fs.journal.lock.lock());
}

/// @new, into the free stage: before discard's bucket, the oldest, so the
/// order stays the order they're written in. The journal is the caller's
/// to block.
pub fn add(fs: &Fs, ca: &Dev, new: &[u64]) -> Result<(), BchError> {
    splice(fs, &mut fs.sb_lock(), ca, 0, new, |ring| Ok(Some(match ring.idx(DISCARD) {
        0 => ring.nr() as usize,
        d => d as usize,
    }))).map(|_| ())
}

/* allocate journal on a device: */

fn set_nr_iter(fs: &Fs, ca: &Dev, nr: u32, watermark: c::bch_watermark, cl: &StackClosure)
    -> Result<(), BchError>
{
    let have = ca.journal.nr();
    assert!(nr > have, "growing {ca}'s journal to {nr} buckets, from {have}");
    let want = (nr - have) as usize;

    let mut obs = KVVec::with_capacity(want).map_err(enomem(fs))?;
    let mut bu = KVVec::with_capacity(want).map_err(enomem(fs))?;

    let ret = (|| -> Result<(), BchError> {
        while bu.len() < want {
            let trans = btree_trans!(fs);
            let first = bu.is_empty();
            let ob = lockrestart_do(&trans, |t| {
                let mut req = t.alloc_request(0, false, None, 1, 0, watermark, c::bch_write_flags::empty(),
                                              first.then(|| cl.as_opaque()))?;
                req.data_type = DataType::journal;
                match t.bucket_alloc(&mut req, ca) {
                    Err(e) if e.matches(bch_errcode::BCH_ERR_operation_blocked) => {
                        t.wait_on_allocator(&mut req, e, cl.as_opaque())?;
                        Err(t.restart(bch_errcode::BCH_ERR_transaction_restart_nested))
                    }
                    ob => ob,
                }
            })?;

            let b = ob.id().bucket();
            let marked = trans_mark_metadata_bucket(&trans, ca, b, DataType::journal,
                                                    ca.mi.get().bucket_size.into(), UpdateTriggerFlags::TRANSACTIONAL);
            bch_err_msg!(fs, marked, "marking new journal buckets")?;

            let Ok(()) = obs.push_within_capacity(ob) else { unreachable!() };
            let Ok(()) = bu.push_within_capacity(b) else { unreachable!() };
        }
        Ok(())
    })();

    if bu.is_empty() {
        return ret;
    }

    /* Don't return an error if we successfully allocated some buckets: */
    let ret = {
        let _blocked = fs.journal.block();
        add(fs, ca, &bu)
    };

    if ret.is_err() {
        let trans = btree_trans!(fs);
        for &b in bu.iter() {
            let _ = trans_mark_metadata_bucket(&trans, ca, b, DataType::free, 0, UpdateTriggerFlags::TRANSACTIONAL);
        }
    }

    ret
}

fn set_nr_loop(fs: &Fs, ca: &Dev, nr: u32, new_fs: bool) -> Result<(), BchError> {
    let watermark = if new_fs {
        c::bch_watermark::btree
    } else {
        c::bch_watermark::normal
    };

    stack_pin_init!(let cl = StackClosure::new());

    /* don't handle reducing nr of buckets yet: */
    if nr < ca.journal.nr() {
        return Ok(());
    }

    while ca.journal.nr() < nr {
        /*
         * note: journal buckets aren't really counted as _sectors_ used yet, so
         * we don't need the disk reservation to avoid the BUG_ON() in buckets.c
         * when space used goes up without a reservation - but we do need the
         * reservation to ensure we'll actually be able to allocate:
         *
         * XXX: that's not right, disk reservations only ensure a
         * filesystem-wide allocation will succeed, this is a device
         * specific allocation - we can hang here:
         *
         * A device outside the capacity (durability 0) takes nothing
         * anyone reserved, so there's nothing to reserve against - and
         * when initializing a new filesystem, the devices after it
         * don't have their free space counted yet.
         */
        let res = DiskReservation::new(fs);
        if !new_fs && ca.has_capacity() {
            res.add(ca.bucket_to_sector((nr - ca.journal.nr()).into()), 1, c::bch_reservation_flags::empty())?;
        }

        set_nr_iter(fs, ca, nr, watermark, &cl)?;
    }
    Ok(())
}

/*
 * Allocate more journal space at runtime - not currently making use if it, but
 * the code works:
 */
pub fn set_nr_journal_buckets(fs: &Fs, ca: &Dev, nr: u32) -> Result<(), BchError> {
    let _state = fs.state_lock_write();

    if ca.removing.get() {
        return fs.throw(bch_errcode::BCH_ERR_device_has_been_removed);
    }

    bch_err_fn!(fs, set_nr_loop(fs, ca, nr, false))
}

pub fn dev_journal_alloc(ca: &Dev, new_fs: bool) -> Result<(), BchError> {
    let fs = ca.fs();

    if !ca.mi.get().data_allowed.contains(DataType::journal.into()) {
        return Ok(());
    }

    if fs.feature(SbFeature::small_image) {
        bch_err!(fs, "cannot allocate journal, filesystem is an unresized image file");
        return fs.throw(bch_errcode::BCH_ERR_erofs_filesystem_full);
    }

    /* 1/128th of the device by default: */
    let nr = (ca.mi.get().nbuckets >> 7) as u32;

    /* but no more than a quarter of RAM: */
    let ram = (totalram_bytes() as u64 / 4 / ca.bucket_bytes()) as u32;
    let nr = nr.max(c::BCH_JOURNAL_BUCKETS_MIN).min(ram);

    bch_err_fn_dev!(ca, set_nr_loop(fs, ca, nr, new_fs))
}

crate::recovery_pass!(fs_journal_alloc => |fs: &Fs| fs.online_members()
    .filter(|ca| ca.journal.nr() == 0)
    .try_for_each(|ca| dev_journal_alloc(&ca, true)));

/// @bucket, which fsck found another user's data in, no longer the journal's:
/// out of the ring now, if the journal's done with it - or retired, never
/// written again, and dropped once reclaim has it awaiting discard.
///
/// Before we're rw: with nothing writing or discarding, its stage doesn't
/// move under us, and sectors_free - the write path's - is ours to set.
pub fn retire(fs: &Fs, ca: &Dev, bucket: u64) -> Result<(), BchError> {
    if fs.flags.get(c::bch_fs_flags::may_go_rw) {
        return Err(fs.err(bch_errcode::BCH_ERR_EBUSY_journal_bucket_retire_rw));
    }

    splice(fs, &mut fs.sb_lock(), ca, 1, &[], |ring| {
        let i = ring.buckets().iter().position(|&b| b == bucket)
            .ok_or_else(|| fs.err(bch_errcode::BCH_ERR_EINVAL_journal_bucket_not_found))? as u32;
        if !ring.needed(i) {
            return Ok(Some(i as usize));
        }

        if i == ring.idx(CUR) {
            /* Being written: closed, so the first write moves on - if there's a bucket to move to */
            if ring.free(DIRTY_ONDISK) == 0 {
                return Err(fs.err(bch_errcode::BCH_ERR_EBUSY_journal_bucket_retire_no_space));
            }
            ca.journal.sectors_free.set(0);
        }
        ring.retired.push(bucket).map_err(enomem(fs))?;
        Ok(None)
    }).inspect_err(|e| bch_err_dev!(ca, "error retiring journal bucket {}: {}", bucket, e))?;

    space_available(fs);
    Ok(())
}

/*
 * The retired buckets reclaim has awaiting discard - done with, but not
 * ours to discard - out of the ring and the superblock: a worker's, as
 * neither reclaim nor the write path's discards may take sb_lock
 */
pub(crate) fn drop_retired(j: &c::journal) {
    let fs = j.fs();
    for ca in fs.members() {
        while splice(fs, &mut fs.sb_lock(), &ca, 1, &[], |ring| Ok(ring.retired_awaiting_discard()))
            .inspect_err(|e| bch_err_dev!(&ca, "error dropping retired journal bucket: {}", e))
            .unwrap_or(false) {}
    }

    space_available(fs);
    j.do_discards();
}

/*
 * Going read-only, the journal flushed: the retired buckets reclaim hasn't
 * dropped - discard may never have reached them. Retired is only in memory:
 * left in the superblock, a bucket is the journal's again at the next mount,
 * over the data fsck found in it.
 */
pub(crate) fn drop_retired_going_ro(fs: &Fs) {
    for ca in fs.members() {
        while splice(fs, &mut fs.sb_lock(), &ca, 1, &[], |ring| Ok(ring.retired_unneeded()))
            .inspect_err(|e| bch_err_dev!(&ca, "error dropping retired journal bucket: {}", e))
            .unwrap_or(false) {}
    }
}
