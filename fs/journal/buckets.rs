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

use core::ffi::{c_int, c_uint};

use cstruct_macros::c_export;

use crate::c;
use crate::c::journal_bucket_stage::{self as Stage, JOURNAL_BUCKET_cur as CUR, JOURNAL_BUCKET_dirty as DIRTY,
                                     JOURNAL_BUCKET_dirty_ondisk as DIRTY_ONDISK,
                                     JOURNAL_BUCKET_discard as DISCARD};
use crate::errcode::{bch_errcode, ret_to_c, BchError};
use crate::fs::{Dev, Fs, SbLockGuard};
use crate::journal::sb::buckets_to_sb;
use crate::locking::{ExclusiveGuard, LockedBy};
use crate::util::alloc::KVVec;
use crate::util::projections::Volatile;
use crate::util::Printbuf;
use crate::{bch_err_dev, prt};

const STAGES: [Stage; 4] = [DISCARD, DIRTY_ONDISK, DIRTY, CUR];

pub struct JournalRing {
    buckets: KVVec<u64>,
    seq:     KVVec<u64>,
    cursors: [u32; STAGES.len()],
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

    fn dist(&self, from: Stage, to: Stage) -> u32 {
        let (f, t) = (self.idx(from), self.idx(to));
        if t >= f { t - f } else { t + self.nr() - f }
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
    /// it's still first: remove() may have moved it.
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
        self.replace(buckets, seq)
    }

    pub fn to_text(&self, out: &mut Printbuf) {
        prt!(out, "discard_idx\t{}\n", self.idx(DISCARD));
        for (name, s) in [("dirty_ondisk", DIRTY_ONDISK), ("dirty_idx", DIRTY), ("cur_idx", CUR)] {
            prt!(out, "{}\t{} (seq {})\n", name, self.idx(s), self.seq(s));
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
pub type JournalBuckets = LockedBy<JournalRing, Volatile<u32>>;

pub fn new() -> JournalBuckets {
    let ring = JournalRing { buckets: KVVec::new(), seq: KVVec::new(), cursors: [0; STAGES.len()] };
    LockedBy::new(ring, pin_init::Zeroable::zeroed())
}

type JournalGuard<'a> = ExclusiveGuard<'a, c::spinlock_t, c::JournalLocked, c::JournalPublished>;

impl Dev {
    pub fn journal_ring<'g>(&'g self, j: &'g mut JournalGuard<'_>) -> &'g mut JournalRing {
        assert!(core::ptr::eq(j.lock(), &self.fs().journal.lock), "journal_ring(): another filesystem's journal lock");
        unsafe { self.journal.buckets.protected_mut() }
    }

    pub fn with_journal_ring<R>(&self, f: impl FnOnce(&mut JournalRing) -> R) -> R {
        f(self.journal_ring(&mut self.fs().journal.lock.lock()))
    }
}

impl c::journal_device {
    pub fn nr(&self) -> u32 {
        self.buckets.get()
    }

    /// Before the journal starts: through the only reference to it.
    pub fn ring_mut(&mut self) -> &mut JournalRing {
        self.buckets.get_mut()
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
 * there: the superblock first, then the journal
 */
fn splice(fs: &Fs, ca: &Dev, removed: usize, inserted: &[u64],
          pos: impl FnOnce(&JournalRing) -> Result<usize, BchError>) -> Result<(), BchError> {
    let mut sb = fs.sb_lock();

    /* sb_lock held, the list doesn't change between our two looks at it */
    let nr = ca.journal.nr() as usize - removed + inserted.len();
    let mut buckets = KVVec::from_elem(0, nr).map_err(enomem(fs))?;
    let seq = KVVec::from_elem(0, nr).map_err(enomem(fs))?;

    let s = ca.with_journal_ring(|ring| {
        let s = Splice { pos: pos(ring)?, removed, inserted: inserted.len() };
        s.apply(&mut buckets, ring.buckets());
        Ok::<_, BchError>(s)
    })?;
    buckets[s.pos..s.pos + s.inserted].copy_from_slice(inserted);

    buckets_to_sb(fs, &mut sb, ca, &buckets)?;
    fs.write_super_ret(&mut sb)?;

    let old = ca.with_journal_ring(|ring| {
        ca.journal.buckets.set(nr as u32);
        ring.splice(s, buckets, seq)
    });
    drop(old);
    Ok(())
}

/// @new, into the free stage: before discard's bucket, the oldest, so the
/// order stays the order they're written in. The journal is the caller's
/// to block.
pub fn add(fs: &Fs, ca: &Dev, new: &[u64]) -> Result<(), BchError> {
    splice(fs, ca, 0, new, |ring| Ok(match ring.idx(DISCARD) {
        0 => ring.nr() as usize,
        d => d as usize,
    }))
}

pub fn remove(fs: &Fs, ca: &Dev, bucket: u64) -> Result<(), BchError> {
    splice(fs, ca, 1, &[], |ring| ring.buckets().iter().position(|&b| b == bucket)
           .ok_or_else(|| fs.err(bch_errcode::BCH_ERR_EINVAL_journal_bucket_not_found)))
        .inspect_err(|e| bch_err_dev!(ca, "error deleting journal bucket {}: {}", bucket, e))?;

    let _j = fs.journal.lock.lock();
    unsafe { c::bch2_journal_space_available(core::ptr::from_ref(&fs.journal).cast_mut()) };
    Ok(())
}

/* C's: with the journal lock held */

fn locked<'a>(j: &c::journal, ja: &'a c::journal_device) -> &'a mut JournalRing {
    j.lock.assert_held();
    unsafe { ja.buckets.protected_mut() }
}

#[c_export]
pub fn bch2_journal_buckets_nr(ja: &c::journal_device) -> c_uint {
    ja.nr()
}

#[c_export]
pub fn bch2_journal_dev_buckets_available(j: &c::journal, ja: &c::journal_device,
                                          from: c::journal_space_from) -> c_uint {
    use c::journal_space_from::*;

    let ring = locked(j, ja);
    let available = ring.free(match from {
        journal_space_discarded     => DISCARD,
        journal_space_clean_ondisk  => DIRTY_ONDISK,
        journal_space_clean         => DIRTY,
        _                           => panic!("journal space from {}", from as u32),
    });

    /*
     * Don't use the last bucket unless writing the new last_seq
     * will make another bucket available:
     */
    available - (available != 0 && !ring.can_advance(DIRTY_ONDISK)) as u32
}

#[c_export]
pub fn bch2_journal_should_discard_bucket(j: &c::journal, ja: &c::journal_device) -> bool {
    bch2_journal_dev_buckets_available(j, ja, c::journal_space_from::journal_space_discarded) <
        (ja.nr() / 2).max(4) &&
        locked(j, ja).can_advance(DISCARD)
}

/* Each a ring method, as bch2_journal_buckets_NAME(j, ja, args) */
macro_rules! ring_exports {
    ($($name:ident($($arg:ident: $ty:ty),*) $(-> $ret:ty)?;)*) => { ::paste::paste! { $(
        #[c_export]
        pub fn [<bch2_journal_buckets_ $name>](j: &c::journal, ja: &c::journal_device, $($arg: $ty),*) $(-> $ret)? {
            locked(j, ja).$name($($arg),*)
        }
    )* } };
}

ring_exports! {
    bucket(s: c::journal_bucket_stage) -> u64;
    seq_ahead(s: c::journal_bucket_stage, k: c_uint) -> u64;
    last_seq(last_seq: u64, last_seq_ondisk: u64);
    would_free(seq: u64) -> bool;
    next_bucket(seq: u64);
    set_cur_seq(seq: u64);
    discarded(bucket: u64);
    to_text(out: &mut Printbuf);
}

/// # Safety
/// @new is @nr u64s.
#[c_export]
pub unsafe fn bch2_journal_buckets_add(ca: &Dev, new: *const u64, nr: c_uint) -> c_int {
    let new = unsafe { core::slice::from_raw_parts(new, nr as usize) };
    ret_to_c(add(ca.fs(), ca, new))
}
