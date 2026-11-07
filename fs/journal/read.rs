#![allow(non_camel_case_types)]

use crate::btree::bkey::ValidateFlags;
use crate::format::{Btree, MemberState, Nonce};

#[path = "buckets.rs"] pub mod buckets;
#[path = "entries.rs"] pub mod entries;
#[path = "init.rs"] pub mod init;
#[path = "pins.rs"] pub mod pins;
#[path = "reclaim.rs"] pub mod reclaim;
#[path = "sb.rs"] pub mod sb;
#[path = "seq_blacklist.rs"] pub mod seq_blacklist;
#[path = "validate.rs"] pub mod validate;
#[path = "write.rs"] pub mod write;

use crate::c;
use crate::util::Printbuf;
use crate::data::checksum::{checksum_type_valid, csum_vstruct, BCH_NONCE_JOURNAL};
use crate::errcode::{bch_errcode, BchError};
use crate::fs::{Dev, Fs};
use crate::init::error::id;
use crate::init::recovery::NotRw;
use crate::locking::Locked;
use crate::sb::members::DevIoRef;
use crate::types::{GenRadix, Le32};
use crate::util::alloc::{init_at, kvzalloc, ABox, AVec, AllocError, ConstAlign, DArrayLoan,
                         KVVec};
use crate::util::bio::{Bio, BioBox, BioBuffersMapped, BioOwner};
use crate::util::kernel::{local_clock, PAGE_SIZE};
use crate::util::log::LogMsg;
use crate::util::par::{self, Backend};
use crate::util::vstructs::{cast_plain, cast_plain_mut, Head, SelfSized, Unvalidated, Validated, VStruct};
use crate::util::workqueue::Queue;
use crate::util::ToText;
use crate::{bch_err, bch_err_dev, bch_info, bch_info_dev, fs_fatal_error, fsck_err, fsck_err_on, new_lock, prt};
use core::mem::offset_of;
use core::ops::Range;
use core::ptr::NonNull;
use pin_init::stack_pin_init;
use validate::Jset;

#[allow(non_camel_case_types)]
pub type journal_entry_type = c::bch_jset_entry_type;

// ---- jset helpers ----

impl c::jset__head {
    /// The nonce of this journal entry: its seq, in the journal domain.
    pub fn nonce(&self) -> Nonce {
        let seq = self.seq.get();
        Nonce {
            d: [
                Le32::new(0),
                Le32::new(seq as u32),
                Le32::new((seq >> 32) as u32),
                Le32::new(BCH_NONCE_JOURNAL),
            ],
        }
    }

    fn checksum_type(&self) -> c::bch_csum_type {
        c::bch_csum_type(self.csum_type() as u32)
    }
}

/// A journal entry's checksum covers all of it past its csum, and its
/// encryption all of it from encrypted_start.
impl c::jset<Unvalidated> {
    pub fn checksum(&self, fs: &Fs) -> c::bch_csum {
        csum_vstruct(Some(fs), self.checksum_type(), self.nonce(), self)
    }

    /// Or decrypted: ChaCha20 is its own inverse.
    pub fn encrypt(&mut self, fs: &Fs) -> Result<(), BchError> {
        let (ty, nonce) = (self.checksum_type(), self.nonce());
        let start = core::mem::offset_of!(Head<c::jset>, encrypted_start);
        fs.encrypt(ty, nonce, &mut self.as_bytes_mut()[start..])
    }

    pub fn datetime(&self) -> u64 {
        self.entries().flatten()
            .find_map(|e| match e.typed() {
                Ok(c::JsetEntryRef::datetime(d)) => Some(d.seconds.get()),
                _ => None,
            })
            .unwrap_or(0)
    }

    pub(crate) fn empty(&self) -> bool {
        self.seq == self.last_seq &&
            !self.entries().flatten().any(|e| {
                entry_type(e) == c::bch_jset_entry_type::btree_keys && e.u64s() != 0
            })
    }
}

impl c::journal_replay {
    /// @p's jset, as long as its header says: a replay is allocated to hold
    /// it, past the struct's end - which a &journal_replay doesn't reach.
    ///
    /// # Safety
    /// @p is a replay from the journal read, which validated its jset, live
    /// for 'a.
    pub unsafe fn jset<'a>(p: *const Self) -> &'a c::jset {
        // SAFETY: the caller's
        unsafe { c::jset::<Validated>::from_ffi(&raw const (*p).j) }
    }

    pub fn ignored(&self) -> bool {
        self.ignore_blacklisted || self.ignore_not_dirty
    }

    pub fn ptrs(&self) -> &[c::journal_ptr] {
        &self.ptrs
    }
}

/// The devices replay @i's copies are on, each once - or, said, none, for more
/// than BCH_REPLICAS_MAX: no one write puts an entry on more, so they aren't
/// its replicas, and a crafted image can claim any number.
pub fn replay_devs(fs: &Fs, i: &c::journal_replay) -> c::bch_devs_list {
    let mut devs = c::bch_devs_list::default();
    for p in i.ptrs() {
        if !devs.has(p.dev) && u32::from(devs.nr) >= c::BCH_REPLICAS_MAX {
            let mut buf = Printbuf::new();
            journal_ptrs_to_text(&mut buf, fs, i);
            bch_err!(fs, "journal seq {} found on more than {} devices, not tracking its replicas:\n{}",
                     i.j.seq.get(), c::BCH_REPLICAS_MAX, buf);
            return c::bch_devs_list::default();
        }
        devs.add(p.dev);
    }
    devs
}

/// kvmalloc()'d with its jset past the struct's end: C's journal_replay *, null
/// for an empty slot.
#[repr(transparent)]
pub struct Replay(NonNull<c::journal_replay>);

unsafe impl Send for Replay {}
unsafe impl Sync for Replay {}

unsafe impl pin_init::ZeroableOption for Replay {}

impl Replay {
    fn new(fs: &Fs, j: &[u8], csum_good: bool) -> Result<Self, BchError> {
        let bytes = offset_of!(c::journal_replay, j) + j.len();
        let p = kvzalloc(bytes).map_err(|_| fs.err(bch_errcode::BCH_ERR_ENOMEM_journal_entry_add))?
            .cast::<c::journal_replay>();
        unsafe {
            let Ok(()) = init_at(crate::types::RustOnly::new(KVVec::new()), &raw mut (*p.as_ptr()).ptrs);
            (*p.as_ptr()).csum_good = csum_good;
            core::ptr::copy_nonoverlapping(j.as_ptr(), (&raw mut (*p.as_ptr()).j).cast::<u8>(), j.len());
        }
        Ok(Replay(p))
    }

    /// Not validated until the read's final pass: its readers come after.
    pub fn jset(&self) -> &c::jset {
        unsafe { c::journal_replay::jset(self.0.as_ptr()) }
    }

    pub(crate) fn jset_unvalidated(&self) -> &c::jset<Unvalidated> {
        c::jset::<Unvalidated>::from_bytes(self.jset_bytes()).expect("a replay holds its jset")
    }

    /// Its jset's datetime entry's, in seconds: 0 if it has none.
    pub fn datetime(&self) -> u64 {
        self.jset_unvalidated().datetime()
    }

    fn jset_bytes(&self) -> &[u8] {
        let bytes = self.j.bytes();
        unsafe { core::slice::from_raw_parts((&raw const (*self.0.as_ptr()).j).cast(), bytes) }
    }

    fn jset_bytes_mut(&mut self) -> &mut [u8] {
        let bytes = self.j.bytes();
        unsafe { core::slice::from_raw_parts_mut((&raw mut (*self.0.as_ptr()).j).cast(), bytes) }
    }

    pub(crate) fn seq(&self) -> u64 {
        self.j.seq.get()
    }
}

impl core::ops::Deref for Replay {
    type Target = c::journal_replay;

    fn deref(&self) -> &c::journal_replay {
        unsafe { self.0.as_ref() }
    }
}

impl core::ops::DerefMut for Replay {
    fn deref_mut(&mut self) -> &mut c::journal_replay {
        unsafe { self.0.as_mut() }
    }
}

impl Drop for Replay {
    fn drop(&mut self) {
        unsafe {
            core::ptr::drop_in_place(&raw mut (*self.0.as_ptr()).ptrs);
            c::rust_kvfree(self.0.as_ptr().cast());
        }
    }
}

/// By seq, from journal_entries_base_seq - the fs's, not ours: C reads it too.
#[repr(transparent)]
pub struct JournalEntries(GenRadix<Option<Replay>>);

unsafe impl pin_init::Zeroable for JournalEntries {}

impl JournalEntries {
    fn idx(fs: &Fs, seq: u64) -> usize {
        seq.wrapping_sub(fs.journal_entries_base_seq.get()) as u32 as usize
    }

    /* seq may be 0, below the base: not a valid index */
    fn idx_from(fs: &Fs, seq: u64) -> usize {
        Self::idx(fs, seq.max(fs.journal_entries_base_seq.get()))
    }

    pub fn get(&self, fs: &Fs, seq: u64) -> Option<&Replay> {
        self.0.get(Self::idx(fs, seq)).and_then(Option::as_ref)
    }

    pub(crate) fn at_offset(&self, offset: u32) -> Option<&Replay> {
        self.0.get(offset as usize).and_then(Option::as_ref)
    }

    fn slot(&mut self, fs: &Fs, seq: u64) -> Result<&mut Option<Replay>, BchError> {
        self.0.get_or_alloc(Self::idx(fs, seq))
            .map_err(|_| fs.err(bch_errcode::BCH_ERR_ENOMEM_journal_entry_add))
    }

    pub fn iter(&self) -> impl Iterator<Item = &Replay> {
        self.0.iter_from(0).filter_map(|(_, i)| i.as_ref())
    }

    pub fn iter_rev(&self) -> impl Iterator<Item = &Replay> {
        self.0.iter_rev().filter_map(|(_, i)| i.as_ref())
    }

    fn kept_from(&self, fs: &Fs, seq: u64) -> impl Iterator<Item = &Replay> {
        self.0.iter_from(Self::idx_from(fs, seq)).filter_map(|(_, i)| i.as_ref().filter(|i| !i.ignored()))
    }

    fn slots_mut(&mut self) -> impl Iterator<Item = &mut Option<Replay>> {
        self.0.iter_mut_from(0).map(|(_, i)| i)
    }

    fn slots_mut_from(&mut self, fs: &Fs, seq: u64) -> impl Iterator<Item = &mut Option<Replay>> {
        self.0.iter_mut_from(Self::idx_from(fs, seq)).map(|(_, i)| i)
    }

    pub(crate) fn slots_rev_mut(&mut self) -> impl Iterator<Item = &mut Option<Replay>> {
        self.0.iter_rev_mut().map(|(_, i)| i)
    }
}

impl Drop for JournalEntries {
    fn drop(&mut self) {
        self.slots_mut().for_each(|i| drop(i.take()));
        self.0.free();
    }
}

impl NotRw<'_> {
    /// # Safety
    /// No other reference to it lives meanwhile.
    unsafe fn journal_device<'d>(&self, ca: &'d Dev) -> &'d mut c::journal_device {
        unsafe { &mut *(&raw const ca.journal).cast_mut() }
    }

    /// # Safety
    /// As journal_device()'s.
    unsafe fn journal_rewind_ranges(&self) -> DArrayLoan<'_, c::journal_rewind_range> {
        unsafe { DArrayLoan::new(&mut *(&raw const self.fs.journal.rewind_ranges).cast_mut()) }
    }
}

pub fn journal_pos_from_member_info_set(fs: &Fs, sb: &mut crate::fs::SbLockGuard<'_>) {
    for ca in fs.members() {
        let ja = &ca.journal;
        let m = sb.fs_member_mut(ca.dev_idx.into());

        let cur = ca.with_journal_ring(|r| r.idx(c::journal_bucket_stage::JOURNAL_BUCKET_cur));
        m.last_journal_bucket = Le32::new(cur);
        m.last_journal_bucket_offset = Le32::new(u32::from(ca.mi.get().bucket_size) - ja.sectors_free.get());
    }
}

pub fn journal_pos_from_member_info_resume(fs: &Fs) {
    let sb = fs.sb_lock();
    let not_rw = unsafe { fs.not_rw() };

    for ca in fs.members() {
        let m = crate::sb::members::sb_member(sb.sb(), ca.dev_idx.into()).unwrap_or_default();
        let ja = unsafe { not_rw.journal_device(&ca) };

        let idx = m.last_journal_bucket.get();
        if idx < ja.nr() {
            ja.ring_mut().set_cur(idx);
        }
        let offset = m.last_journal_bucket_offset.get();
        let bucket_size = u32::from(ca.mi.get().bucket_size);
        if offset <= bucket_size {
            ja.sectors_free.set(bucket_size - offset);
        }
    }
}

fn journal_ptr_to_text(out: &mut Printbuf, fs: &Fs, p: &c::journal_ptr) {
    match fs.dev_get(p.dev.into()) {
        Some(ca) => ca.to_text(out),
        None     => prt!(out, "(invalid dev)"),
    }
    prt!(out, " {}:{}:{} (sector {})", p.dev, p.bucket, p.bucket_offset, p.sector);
}

pub fn journal_ptrs_to_text(out: &mut Printbuf, fs: &Fs, j: &c::journal_replay) {
    for (n, p) in j.ptrs().iter().enumerate() {
        if n != 0 {
            prt!(out, " ");
        }
        journal_ptr_to_text(out, fs, p);
    }
}

fn journal_datetime_to_text(out: &mut Printbuf, j: &c::jset<Unvalidated>) {
    let t = j.datetime();
    if t != 0 {
        crate::util::printbuf::DateTime(t as i64).to_text(out);
    }
}

impl Fs {
    pub fn journal_seq_datetime_to_text(&self, out: &mut Printbuf, seq: u64) {
        let Some(jk) = self.journal_keys_get() else { return };
        if let Some(i) = jk.value().entries().get(self, seq) {
            journal_datetime_to_text(out, i.jset_unvalidated());
        }
    }
}

fn journal_replay_to_text(out: &mut Printbuf, fs: &Fs, j: &Replay) {
    prt!(out, "seq {} ", j.seq());
    journal_datetime_to_text(out, j.jset_unvalidated());
    prt!(out, " ");
    journal_ptrs_to_text(out, fs, j);
}

fn jset_csum_good(fs: &Fs, j: &c::jset<Unvalidated>) -> (bool, c::bch_csum) {
    if !checksum_type_valid(fs, j.csum_type()) {
        return (false, c::bch_csum::default());
    }

    let csum = j.checksum(fs);
    (csum == j.csum, csum)
}

struct ReadState<'a> {
    entries:   &'a mut JournalEntries,
    last_seq:  u64,
    /*
     * If the journal scrub will run: the newest flush entry before
     * last_seq. It can only rewind to a flush, so its first range starts
     * there, and we keep entries from there instead of from last_seq.
     * Neither ever goes backwards, so dropping below it is always safe.
     */
    scrub:     bool,
    scrub_seq: u64,
}

type ReadLock<'a> = Locked<c::mutex, ReadState<'a>>;

fn journal_replay_free(fs: &Fs, slot: &mut Option<Replay>, blacklisted: bool) {
    let Some(i) = slot.as_mut() else { return };

    if blacklisted {
        i.ignore_blacklisted = true;
    } else {
        i.ignore_not_dirty = true;
    }

    if fs.opts.read_entire_journal.get() == 0 {
        *slot = None;
    }
}

fn journal_replay_maybe_drop_overwrites<'j>(fs: &Fs, j: &'j mut c::jset<Unvalidated>) -> &'j c::jset<Unvalidated> {
    /* Drop overwrites, log entries if we don't need them: */
    let drop = fs.opts.retain_recovery_info.get() == 0 &&
        fs.opts.journal_rewind.get() == 0 &&
        fs.opts.scrub_recent_journal_entries.get() == 0 &&
        j.entries().all(|e| e.is_ok());

    let j = j.as_bytes_mut();
    if drop {
        const JSET_HEAD: usize = size_of::<Head<c::jset>>();
        let (mut src, mut dst) = (JSET_HEAD, JSET_HEAD);
        while src < j.len() {
            let e = c::jset_entry::<Unvalidated>::from_bytes(&j[src..]).expect("an entry that fits");
            let (len, ty) = (size_of_val(e), entry_type(e));
            if ty != c::bch_jset_entry_type::log &&
               ty != c::bch_jset_entry_type::overwrite {
                j.copy_within(src..src + len, dst);
                dst += len;
            }
            src += len;
        }

        cast_plain_mut::<Head<c::jset>>(j).expect("a jset's header").set_val_u64s((dst - JSET_HEAD) / 8);
    }
    c::jset::<Unvalidated>::from_bytes(j).expect("a jset, as long as its u64s says")
}

impl ReadState<'_> {
    /* Entries before this aren't needed (0: none found yet, keep everything) */
    fn keep_from(&self) -> u64 {
        if self.scrub { self.scrub_seq } else { self.last_seq }
    }

    /*
     * last_seq went up: the newest flush entry before it may be one we already
     * have. Everything from the old scrub_seq on is still here.
     */
    fn scrub_seq_update(&mut self, fs: &Fs) {
        let last_seq = self.last_seq;
        self.scrub_seq = self.entries.kept_from(fs, self.scrub_seq)
            .take_while(|i| i.seq() < last_seq)
            .filter(|i| !i.j.no_flush())
            .fold(self.scrub_seq, |s, i| s.max(i.seq()));
    }

    /*
     * Given a journal entry we just read, add it to the list of journal entries to
     * be replayed:
     */
    fn entry_add(&mut self, fs: &Fs, ca: &Dev, entry_ptr: c::journal_ptr, j: &mut c::jset<Unvalidated>)
        -> Result<(), BchError>
    {
        let mut last_seq = if !j.no_flush() { j.last_seq.get() } else { 0 };
        let seq = j.seq.get();
        let read_entire_journal = fs.opts.read_entire_journal.get() != 0;

        if last_seq != 0 && fs.opts.journal_rewind.get() != 0 {
            last_seq = last_seq.min(fs.opts.journal_rewind.get());
        }

        let oldest = fs.journal.oldest_seq_found_ondisk.get();
        if oldest == 0 || seq < oldest {
            fs.journal.oldest_seq_found_ondisk.set(seq);
        }

        /* Is this entry older than the range we need? */
        if !read_entire_journal && seq < self.keep_from() {
            return Ok(());
        }

        /*
         * genradixes are indexed by a ulong, not a u64, so we can't index them
         * by sequence number directly: Assume instead that they will all fall
         * within the range of +-2billion of the filrst one we find.
         */
        if fs.journal_entries_base_seq.get() == 0 {
            fs.journal_entries_base_seq.set(seq.saturating_sub(i32::MAX as u64).max(1));
        }

        if seq.wrapping_sub(fs.journal_entries_base_seq.get()) > u64::from(u32::MAX) {
            bch_err!(fs, "journal entry sequence numbers span too large a range: cannot replay, contact developers\n\
                          base {} last_seq currently {}, but have seq {}",
                     fs.journal_entries_base_seq.get(), self.last_seq, seq);
            return fs.throw(bch_errcode::BCH_ERR_ENOMEM_journal_entry_add);
        }

        let keep_from = self.keep_from();

        if last_seq > self.last_seq {
            self.last_seq = last_seq;
            if self.scrub {
                self.scrub_seq_update(fs);
            }
        }

        /* A flush entry before last_seq, newer than the one we had */
        if self.scrub && !j.no_flush() && seq < self.last_seq {
            self.scrub_seq = self.scrub_seq.max(seq);
        }

        /* Drop entries we don't need anymore */
        if self.keep_from() > keep_from && !read_entire_journal {
            let new_keep_from = self.keep_from();
            for slot in self.entries.slots_mut_from(fs, keep_from) {
                let Some(seq) = slot.as_ref().filter(|i| !i.ignored()).map(|i| i.seq()) else { continue };
                if seq >= new_keep_from {
                    break;
                }
                journal_replay_free(fs, slot, false);
            }
        }

        let j = journal_replay_maybe_drop_overwrites(fs, j);
        let bytes = j.as_bytes();

        let slot = self.entries.slot(fs, seq)?;

        /*
         * Duplicate journal entries? If so we want the one that didn't have a
         * checksum error:
         */
        if let Some(dup) = slot.as_mut() {
            if seq != dup.seq() {
                bch_err!(fs, "seq {} != dup {} base {}", seq, dup.seq(), fs.journal_entries_base_seq.get());
                crate::warn_on!(true);
            }

            let identical = dup.jset_bytes() == bytes;
            let not_identical = !identical && entry_ptr.csum_good && dup.csum_good;

            let mut same_device = false;
            for ptr in dup.ptrs() {
                if ptr.dev == ca.dev_idx {
                    if ptr.sector == entry_ptr.sector {
                        return Ok(()); /* same physical location, re-read */
                    }
                    same_device = true;
                }
            }

            let mut buf = Printbuf::new();
            journal_replay_to_text(&mut buf, fs, dup);
            buf.newline();
            prt!(&mut buf, "seq {} ", seq);
            journal_datetime_to_text(&mut buf, j);
            prt!(&mut buf, " ");
            journal_ptr_to_text(&mut buf, fs, &entry_ptr);

            dup.ptrs.push(entry_ptr).map_err(|_| fs.err(bch_errcode::BCH_ERR_ENOMEM_journal_entry_add))?;

            fsck_err_on!(fs, same_device, id::journal_entry_dup_same_device,
                         "duplicate journal entry on same device\n{}", buf)?;

            fsck_err_on!(fs, not_identical, id::journal_entry_replicas_data_mismatch,
                         "found duplicate but non identical journal entries\n{}", buf)?;

            if identical || !entry_ptr.csum_good {
                return Ok(());
            }
        }

        let mut i = Replay::new(fs, bytes, entry_ptr.csum_good)?;

        match slot.take() {
            /* The first ptr should represent the jset we kept: */
            Some(dup) => i.ptrs.extend_from_slice(dup.ptrs()),
            None      => i.ptrs.push(entry_ptr),
        }.map_err(|_| fs.err(bch_errcode::BCH_ERR_ENOMEM_journal_entry_add))?;

        *slot = Some(i);
        Ok(())
    }
}

fn ring_back(head: usize, nr: usize) -> impl Iterator<Item = usize> {
    (1..nr).map(move |k| (head + nr - k) % nr)
}

/*
 * Bisect-stride descent to find any non-empty bucket: peek bucket[0],
 * then unpeeked positions at stride = largest power of 2 <= nr-1,
 * halving each level. Each level peeks {step, 3*step, 5*step, ...} —
 * the odd multiples of step not covered by prior larger-step levels.
 * When step reaches 0, every position has been peeked, so an "empty"
 * return guarantees the journal is genuinely empty.
 *
 * Returns the position of a non-empty bucket, or None if every bucket
 * was peeked and none contained a journal entry.
 */
fn anchor_bucket(nr: usize, seq: &mut impl FnMut(usize) -> u64) -> Option<usize> {
    if seq(0) != 0 {
        return Some(0);
    }

    if nr <= 1 {
        return None;
    }

    let mut step = 1 << (nr - 1).ilog2();
    while step != 0 {
        for pos in (step..nr).step_by(step * 2) {
            if seq(pos) != 0 {
                return Some(pos);
            }
        }
        step >>= 1;
    }
    None
}

/*
 * Binary search forward (mod nr) from a non-empty anchor for the
 * write head — the bucket with the maximum seq. In healthy state the
 * in-use buckets form a contiguous range with monotonically increasing
 * seqs from tail to head, with the discarded region as a contiguous run
 * of empties just past the head.
 *
 * Returns the head's bucket index.
 */
fn bsearch_head(nr: usize, anchor: usize, seq: &mut impl FnMut(usize) -> u64) -> usize {
    let (mut lo, mut hi) = (anchor, anchor + nr - 1);

    while lo < hi {
        let mid = (lo + hi).div_ceil(2); /* bias high */
        let s_mid = seq(mid % nr);
        let s_lo = seq(lo % nr);

        if s_mid == 0 {
            /* mid is empty (discarded); head is in [lo, mid) */
            hi = mid - 1;
        } else if s_mid > s_lo {
            /* same in-use range, head is at or after mid */
            lo = mid;
        } else {
            /* wrapped past head; head < mid */
            hi = mid - 1;
        }
    }

    lo % nr
}

/*
 * Walk backwards (mod nr) from the write head, pushing each
 * non-empty bucket onto `order`. Stops at the first empty bucket
 * (discarded boundary). Returns false if the seqs aren't strictly
 * decreasing going backwards from head — that's the fallback signal
 * to redo with the full Pass 1 scan.
 *
 * Strictly decreasing (not just non-increasing): on a single device
 * with replicas=1 the seqs descend by 1 between adjacent buckets; on
 * multi-device with replication, a given device only sees the seqs
 * that landed on it, so gaps are normal — but a given seq still maps
 * to a single bucket per device, so two adjacent buckets with the
 * same seq is impossible and signals corruption.
 */
fn walk_inuse(nr: usize, head: usize, seq: &mut impl FnMut(usize) -> u64, order: &mut KVVec<(usize, u64)>)
    -> Result<bool, AllocError>
{
    let mut prev_seq = seq(head);

    if prev_seq == 0 {
        return Ok(false);
    }

    order.push((head, prev_seq))?;

    for idx in ring_back(head, nr) {
        let s = seq(idx);
        if s == 0 {
            break;          /* hit discarded region */
        }
        if s >= prev_seq {
            return Ok(false);       /* non-decreasing — fallback */
        }

        order.push((idx, s))?;
        prev_seq = s;
    }
    Ok(true)
}

/*
 * Locate the in-use journal bucket range, populating `order` with all
 * live buckets:
 *
 *   - Fast path: binary search forward from an anchor to the head,
 *     then backwards walk while seqs are monotonic and bucket-contiguous.
 *
 *   - Slow fallback: if bsearch / walk / monotonicity / contiguity fails,
 *     ensure every bucket has been peeked, then rebuild order from every
 *     non-empty peeked bucket. Anchor descent already covers the whole
 *     ring when no anchor is found early, so the additional peeks here
 *     are bounded by nr.
 *
 *   - Anchor-not-found: every bucket peeked and empty -> journal is
 *     empty. `order` stays empty; caller falls through to the slow
 *     full-bucket-read path (which handles the empty-journal case too).
 */
fn bsearch_collect(nr: usize, mut seq: impl FnMut(usize) -> u64) -> Result<KVVec<(usize, u64)>, AllocError> {
    let mut order = KVVec::new();

    let Some(anchor) = anchor_bucket(nr, &mut seq) else {
        /* journal is empty — every bucket peeked, none had a jset */
        return Ok(order);
    };

    let head = bsearch_head(nr, anchor, &mut seq);

    /*
     * No post-walk seq-contiguity check: walk_inuse already enforces
     * strictly-decreasing seqs, which is the correct invariant for both
     * single-device (descend by 1) and multi-device with replication
     * (gaps where a seq went to other devices, no two adjacent buckets
     * with the same seq on this device).
     */
    if walk_inuse(nr, head, &mut seq, &mut order)? {
        return Ok(order);
    }

    /*
     * Bsearch / walk / validation failed. Peek any remaining unpeeked
     * buckets so we have a complete view, then rebuild order from every
     * non-empty bucket (same as the old Pass 1 loop did).
     */
    order.clear();
    for i in 0..nr {
        let s = seq(i);
        if s != 0 {
            order.push((i, s))?;
        }
    }
    Ok(order)
}

/*
 * Check monotonicity: walk all journal buckets backwards
 * from the write head (wrapping around). Seq should be
 * non-increasing, and once we hit an empty (discarded)
 * bucket, everything after it should also be empty.
 * Groundwork for a future binary search.
 */
fn bucket_seqs_monotonic(bucket_seq: &[u64], head: usize, out: &mut Printbuf) -> bool {
    let nr = bucket_seq.len();
    let mut prev_seq = bucket_seq[head];
    let mut monotonic = true;
    let mut saw_empty = false;

    for idx in ring_back(head, nr) {
        let seq = bucket_seq[idx];

        if seq == 0 {
            saw_empty = true;
        } else if saw_empty || seq > prev_seq {
            monotonic = false;
        }
        if seq > prev_seq {
            prt!(out, "  [{}]={} > [{}]={}\n", idx, seq, (idx + 1) % nr, prev_seq);
        }
        prev_seq = seq;
    }
    monotonic
}

struct DevReader<'a> {
    fs:  &'a Fs,
    ca:  &'a Dev,
    /* ca.journal, which the reader writes: read it here, never through ca */
    ja:  &'a mut c::journal_device,
    /* its buffer page aligned: userspace reads into it O_DIRECT */
    bio: BioBox<Bio, BioBuffersMapped<ABox<[u8], ConstAlign<PAGE_SIZE>>>>,
}

impl<'a> DevReader<'a> {
    fn new(fs: &'a Fs, ca: &'a Dev, ja: &'a mut c::journal_device) -> Result<Self, BchError> {
        let bytes = (ca.bucket_bytes() as usize).next_power_of_two();
        let mut buf = AVec::<u8, ConstAlign<PAGE_SIZE>>::new(PAGE_SIZE);
        buf.try_reserve_exact(bytes).map_err(|_| fs.err(bch_errcode::BCH_ERR_ENOMEM_journal_read_buf_realloc))?;
        unsafe { buf.set_len(bytes) };
        let mut buf = buf.into_boxed_slice();
        buf.fill(0);
        let bio = BioBox::<Bio, BioBuffersMapped<_>>::new_with::<BchError, _>(
            ca.disk_sb.bdev, c::req_op_REQ_OP_READ, buf, |b| b.init())?;
        Ok(DevReader { fs, ca, ja, bio })
    }

    fn read(&mut self, sector: u64, bytes: usize) -> Result<(), BchError> {
        self.bio.reset(self.ca.disk_sb.bdev, c::req_op_REQ_OP_READ);
        self.bio.bio_mut().set_sector(sector);
        self.bio.bio_mut().shrink(bytes.try_into().expect("a journal read of 4G or more"));
        self.bio.submit_wait()
    }

    fn bucket_to_sector(&mut self, bucket: usize) -> u64 {
        self.ca.bucket_to_sector(self.ja.ring_mut().buckets()[bucket])
    }

    fn read_bucket(&mut self, read: &ReadLock<'_>, bucket: usize) -> Result<(), BchError> {
        let (fs, ca) = (self.fs, self.ca);
        let bucket_size = u64::from(ca.mi.get().bucket_size);
        let start = self.bucket_to_sector(bucket);
        let (mut offset, end) = (start, start + bucket_size);
        let mut saw_bad = false;

        let submit_time = local_clock();
        let ret = self.read(offset, ca.bucket_bytes() as usize);

        ca.account_io_completion(c::bch_member_error_type::BCH_MEMBER_ERROR_read, submit_time, ret.is_ok());

        if ret.is_err() {
            crate::bch_err_dev_ratelimited!(ca, "journal read error: sector {}", offset);
            /*
             * We don't error out of the recovery process
             * here, since the relevant journal entry may be
             * found on a different device, and missing or
             * no journal entries will be handled later
             */
            return Ok(());
        }

        while offset < end {
            let j = &mut self.bio.buf_mut()[((offset - start) << 9) as usize..];
            let h = cast_plain_mut::<Head<c::jset>>(j).expect("a jset's header");

            let sectors = match validate::jset_validate_early(fs, Some(ca), h, offset, (end - offset) as u32)? {
                Jset::Ok => h.sectors(fs.block_bits()) as u64,
                Jset::None | Jset::Bad => {
                    if !saw_bad {
                        return Ok(());
                    }
                    /*
                     * On checksum error we don't really trust the size
                     * field of the journal entry we read, so try reading
                     * again at next block boundary:
                     */
                    offset += 1 << fs.block_bits();
                    continue;
                }
            };

            let seq = h.seq.get();
            if seq > self.ja.highest_seq_found {
                self.ja.highest_seq_found = seq;
                self.ja.ring_mut().set_cur(bucket as u32);
                self.ja.sectors_free.set((end - offset - sectors) as u32);
            }

            /*
             * This happens sometimes if we don't have discards on -
             * when we've partially overwritten a bucket with new
             * journal entries. We don't need the rest of the
             * bucket:
             */
            if seq < self.ja.ring_mut().seqs()[bucket] {
                return Ok(());
            }

            self.ja.ring_mut().seqs_mut()[bucket] = seq;

            let j = c::jset::<Unvalidated>::from_bytes_mut(j).expect("a jset, cut to fit its bucket");
            let (csum_good, csum) = jset_csum_good(fs, j);

            ca.account_io_completion(c::bch_member_error_type::BCH_MEMBER_ERROR_checksum, 0, csum_good);

            if !csum_good {
                /*
                 * Don't print an error here, we'll print the error
                 * later if we need this journal entry
                 */
                saw_bad = true;
            }

            if let Err(e) = j.encrypt(fs) {
                fs_fatal_error!(fs, "decrypting journal entry: {}", e);
            }

            let ptr = c::journal_ptr {
                csum_good,
                csum,
                dev:           ca.dev_idx,
                bucket:        bucket as u32,
                bucket_offset: (offset - start) as u32,
                sector:        offset,
            };
            read.lock().entry_add(fs, ca, ptr, j)?;

            offset += sectors;
        }

        Ok(())
    }

    /*
     * Read just the first block of a journal bucket to extract the sequence
     * number from the jset header: its seq stored in bucket_seq[bucket], or
     * left at 0 if the bucket doesn't contain a valid journal entry.
     */
    fn peek_bucket(&mut self, bucket: usize) {
        let sector = self.bucket_to_sector(bucket);
        if self.read(sector, 512 << self.fs.block_bits()).is_err() {
            return; /* not fatal - bucket may be readable on another device */
        }

        let j = cast_plain::<Head<c::jset>>(self.bio.buf()).expect("a jset's header");
        if j.magic.get() != self.fs.jset_magic() {
            return;
        }

        self.ja.ring_mut().seqs_mut()[bucket] = j.seq.get();
    }

    /*
     * Peek a bucket if we haven't already: the bucket's seq, 0 if empty /
     * unreadable / not a journal entry.
     */
    fn peek_once(&mut self, peeked: &mut [bool], bucket: usize) -> u64 {
        if !peeked[bucket] {
            self.peek_bucket(bucket);
            peeked[bucket] = true;
        }
        self.ja.ring_mut().seqs()[bucket]
    }

    fn read_live(&mut self, read: &ReadLock<'_>, mut order: KVVec<(usize, u64)>) -> Result<(), BchError> {
        let nr = self.ja.nr() as usize;

        /*
         * Sort by seq descending, then read in that order. Once
         * we've read past what we keep (last_seq, or the journal
         * scrub's start), all remaining buckets are dead — stop.
         */
        order.sort_unstable_by(|a, b| b.1.cmp(&a.1));

        let mut last_seq_idx = 0;
        let mut nr_read = 0;
        for &(bucket, _) in order.iter() {
            self.read_bucket(read, bucket)?;
            nr_read += 1;

            let keep_from = read.lock().keep_from();

            /*
             * Once we've established where to keep from and this
             * bucket's max seq (now in bucket_seq from the full
             * read) is below it, we're done:
             */
            if keep_from != 0 && self.ja.ring_mut().seqs()[bucket] < keep_from {
                break;
            }

            last_seq_idx = bucket;
        }

        let bucket_seq = self.ja.ring_mut().seqs();

        /* Find write head — bucket with max seq among all buckets */
        let max_bucket = (0..nr).fold(0, |m, i| if bucket_seq[i] > bucket_seq[m] { i } else { m });

        let mut violations = Printbuf::new();
        if !bucket_seqs_monotonic(bucket_seq, max_bucket, &mut violations) {
            let mut msg = LogMsg::new(self.fs);
            prt!(&mut msg, "{}: journal bucket seqs not monotonic\n  \
                            last_seq bucket {} write head bucket {} seq {}:\n",
                 self.ca, last_seq_idx, max_bucket, bucket_seq[max_bucket]);
            violations.to_text(&mut msg);
            crate::init::error::sb_error_count(self.fs, id::journal_bucket_seq_not_monotonic);
        }

        if self.fs.opts.verbose.get() != 0 {
            bch_info_dev!(self.ca, "journal read: {}/{} buckets read", nr_read, nr);
        }
        Ok(())
    }

    fn read_device(&mut self, read: &ReadLock<'_>, full_read: bool) -> Result<(), BchError> {
        let nr = self.ja.nr() as usize;

        /*
         * Optimization for large journals on slow devices (e.g. SD cards):
         * first read just the header of each bucket to find sequence numbers,
         * then only fully read buckets that contain live journal entries.
         *
         * Skip when reading the entire journal is requested (fsck, debugging).
         */
        let order = if self.fs.opts.read_entire_journal.get() == 0 && nr > 32 && !full_read {
            /*
             * Fast path: O(log nr + dirty_size) peeks via binary search +
             * monotonic backwards walk. bsearch_collect handles its own
             * fallback internally (rebuilds order from all peeked buckets if
             * bsearch / walk fails). An empty order on return means the
             * journal itself is empty -> slow full-bucket-read.
             */
            let mut peeked = KVVec::from_elem(false, nr)?;
            bsearch_collect(nr, |i| self.peek_once(&mut peeked, i))?
        } else {
            KVVec::new()
        };

        if !order.is_empty() {
            self.read_live(read, order)?;
        } else {
            for i in 0..nr {
                self.read_bucket(read, i)?;
            }
        }

        /*
         * Set dirty_idx to indicate the entire journal is full and needs to be
         * reclaimed - journal reclaim will immediately reclaim whatever isn't
         * pinned when it first runs:
         */
        self.ja.ring_mut().start();
        Ok(())
    }
}

fn read_devices(fs: &Fs, not_rw: &NotRw<'_>, read: &ReadLock<'_>, devs: KVVec<DevIoRef<c::bch_dev_read_ref>>,
                full_read: bool) -> Result<(), BchError> {
    par::scope(Backend::Workqueue(Queue::system_dfl()), |s| {
        for ca in devs.iter() {
            let ca: &Dev = ca;
            let ja = unsafe { not_rw.journal_device(ca) };
            s.spawn(format_args!("journal read {}", ca), move || {
                let ret = if ja.nr() == 0 {
                    Ok(())
                } else {
                    DevReader::new(fs, ca, ja).and_then(|mut r| r.read_device(read, full_read))
                };

                match &ret {
                    Ok(())  => if fs.opts.verbose.get() != 0 { bch_info_dev!(ca, "journal read done") },
                    Err(e)  => bch_err_dev!(ca, "journal read error {}", e),
                }
                ret
            })?;
        }
        Ok(())
    })
}

fn journal_devs(fs: &Fs, want: impl Fn(&Dev) -> bool, degraded: bool)
    -> Result<KVVec<DevIoRef<c::bch_dev_read_ref>>, BchError>
{
    let mut devs = KVVec::new();
    for ca in fs.members() {
        if !want(&ca) {
            continue;
        }

        let state = ca.mi.get().state;
        let ioref = (state == MemberState::rw ||
                     state == MemberState::ro)
            .then(|| fs.dev_get_ioref::<c::bch_dev_read_ref>(ca.dev_idx.into()))
            .flatten();
        match ioref {
            Some(ca) => devs.push(ca)?,
            None if degraded => fs.journal.flags.set(c::journal_flags::JOURNAL_degraded, true),
            None => {}
        }
    }
    Ok(devs)
}

pub fn journal_entry_missing_range(fs: &Fs, start: u64, end: u64) -> Option<Range<u64>> {
    assert!(start <= end);

    if start == end {
        return None;
    }

    let start = fs.journal_seq_blacklist.next_nonblacklisted(start);
    if start >= end {
        return None;
    }

    let missing = start..end.min(fs.journal_seq_blacklist.next_blacklisted(start));
    (!missing.is_empty()).then_some(missing)
}

/*
 * Returns true if any seq in [start_seq, end_seq] is missing from
 * the entries (excluding blacklisted ranges). Used to decide whether
 * to fall back to a full per-device journal read after the bsearch fast
 * path; the bsearch on any single device may legitimately have gaps on
 * multi-device + replication setups, but the union across devices must
 * cover the live range.
 */
fn journal_has_any_missing(fs: &Fs, entries: &JournalEntries, start_seq: u64, end_seq: u64) -> bool {
    let mut seq = start_seq;

    /* From start_seq: entries before it are only kept for the journal scrub */
    for i in entries.kept_from(fs, start_seq) {
        if journal_entry_missing_range(fs, seq, i.seq()).is_some() {
            return true;
        }
        seq = i.seq() + 1;
    }
    journal_entry_missing_range(fs, seq, end_seq + 1).is_some()
}

fn journal_check_for_missing(fs: &Fs, entries: &JournalEntries, start_seq: u64, end_seq: u64)
    -> Result<(), BchError>
{
    let mut prev: Option<&Replay> = None;
    /* Sequence number we expect to find next, to check for missing entries */
    let mut seq = start_seq;

    /* From start_seq: entries before it are only kept for the journal scrub */
    for i in entries.kept_from(fs, start_seq) {
        assert!(seq <= i.seq());

        while let Some(missing) = journal_entry_missing_range(fs, seq, i.seq()) {
            let mut buf = Printbuf::new();
            prt!(&mut buf, "journal entries {}-{} missing! (replaying {}-{})",
                 missing.start, missing.end - 1, start_seq, end_seq);

            if let Some(prev) = prev {
                prt!(&mut buf, "\n{} at ", prev.seq());
                journal_ptrs_to_text(&mut buf, fs, prev);
                prt!(&mut buf, " size {}", prev.j.sectors(fs.block_bits()));
            }

            prt!(&mut buf, "\n{} at ", i.seq());
            journal_ptrs_to_text(&mut buf, fs, i);
            prt!(&mut buf, ", continue?");

            fsck_err!(fs, id::journal_entries_missing, "{}", buf)?;

            seq = missing.end;
        }

        prev = Some(i);
        seq = i.seq() + 1;
    }
    Ok(())
}

fn journal_print_checksum_error(fs: &Fs, j: &Replay) {
    let mut msg = LogMsg::new(fs);

    let csum_type = j.j.checksum_type();
    let mut have_good = false;

    prt!(&mut msg, "invalid journal checksum(s) at seq {} ", j.seq());
    journal_datetime_to_text(&mut msg, j.jset_unvalidated());
    msg.newline();

    for ptr in j.ptrs() {
        if !ptr.csum_good {
            journal_ptr_to_text(&mut msg, fs, ptr);
            prt!(&mut msg, " ");
            ptr.csum.to_text(&mut msg, csum_type);
            msg.newline();
        } else {
            have_good = true;
        }
    }

    prt!(&mut msg, "should be ");
    j.j.csum.to_text(&mut msg, csum_type);

    if have_good {
        prt!(&mut msg, "\n(had good copy on another device)");
    }
}

/*
 * Re-read journal buckets needed for rewind.
 *
 * The first journal read drops entries older than the most recent flush
 * entry's last_seq.  When rewinding, we need entries back to the rewind
 * target's last_seq.  Check all rewind ranges in journal.rewind_ranges
 * and re-read any buckets containing entries we need.
 */
pub fn journal_reread_for_rewind(fs: &Fs) -> Result<(), BchError> {
    let not_rw = unsafe { fs.not_rw() };
    let entries = unsafe { not_rw.journal_entries() };
    let mut need_from = u64::MAX;

    for range in fs.journal.rewind_ranges.as_slice() {
        let Some(p) = entries.get(fs, range.to) else {
            bch_err!(fs, "journal rewind: flush entry at seq {} not found", range.to);
            return fs.throw(bch_errcode::BCH_ERR_EINVAL_journal_rewind_before_discard);
        };

        need_from = need_from.min(p.j.last_seq.get());
    }

    if need_from == u64::MAX {
        return Ok(());
    }

    bch_info!(fs, "journal rewind: re-reading entries {}-{}", need_from, fs.journal_replay_seq_start.get());

    if need_from >= fs.journal_replay_seq_start.get() {
        return Ok(()); /* nothing extra needed */
    }

    {
        stack_pin_init!(let read: ReadLock<'_> = new_lock!(ReadState {
            entries:   &mut *entries,
            last_seq:  need_from,
            scrub:     false,
            scrub_seq: 0,
        }));

        for ca in journal_devs(fs, |ca| ca.journal.nr() != 0, false)?.iter() {
            let mut r = DevReader::new(fs, ca, unsafe { not_rw.journal_device(ca) })?;

            for i in 0..r.ja.nr() as usize {
                /* Only re-read buckets that might have entries we need */
                if r.ja.ring_mut().seqs()[i] < need_from {
                    continue;
                }

                r.read_bucket(&read, i)?;
            }
        }
    }

    /*
     * Un-ignore entries that were marked ignore_not_dirty by the first
     * read's drop_before pass — they're needed for replay now:
     */
    for i in entries.slots_mut() {
        if let Some(i) = i.as_mut().filter(|i| i.seq() >= need_from) {
            i.ignore_not_dirty = false;
        }
    }

    fs.journal_replay_seq_start.set(need_from);
    Ok(())
}

enum Newest {
    Nothing,
    NoFlush { cur_seq: u64 },
    Flush(c::journal_start_info),
}

/*
 * Iterating in reverse, find the most recent flush entry and compute
 * the three sequence number zones:
 *
 * - cur_seq: set from the highest entry of any kind (first iteration);
 *   new journal writes will start here. Must be strictly greater than
 *   every on-disk entry, including noflush entries that will be
 *   blacklisted — we must never reuse an on-disk sequence number.
 *
 * - replay_end: the most recent flush entry's seq — replay up to here.
 *
 * - last_seq: the most recent flush entry's last_seq — replay from here.
 *
 * Everything between replay_end+1 and cur_seq-1 (noflush entries and
 * torn flush writes) will be blacklisted by the caller.
 */
fn newest_flush(fs: &Fs, entries: &mut JournalEntries) -> Result<Newest, BchError> {
    let mut cur_seq = 0;
    let mut last_write_torn = false;

    for i in entries.slots_rev_mut() {
        let Some(i) = i.as_mut().filter(|i| !i.ignored()) else { continue };

        if cur_seq == 0 {
            cur_seq = i.seq() + 1;
        }

        if i.j.no_flush() {
            i.ignore_blacklisted = true;
            continue;
        }

        if !last_write_torn && !i.csum_good {
            last_write_torn = true;
            i.ignore_blacklisted = true;
            continue;
        }

        validate::jset_last_seq_check(fs, &mut i.j, ValidateFlags::empty())?;

        return Ok(Newest::Flush(c::journal_start_info {
            cur_seq,
            last_seq:   i.j.last_seq.get(),
            replay_end: i.seq(),
            clean:      i.jset_unvalidated().empty(),
        }));
    }

    Ok(if cur_seq == 0 { Newest::Nothing } else { Newest::NoFlush { cur_seq } })
}

pub fn journal_read(fs: &Fs) -> Result<c::journal_start_info, BchError> {
    let bsearch_used = fs.opts.read_entire_journal.get() == 0;

    let not_rw = unsafe { fs.not_rw() };
    stack_pin_init!(let read: ReadLock<'_> = new_lock!(ReadState {
        entries:   unsafe { not_rw.journal_entries() },
        last_seq:  0,
        scrub:     fs.journal_scrub_will_run(),
        scrub_seq: 0,
    }));

    let devs = journal_devs(fs, |ca| {
        fs.opts.read_entire_journal.get() != 0 ||
            fs.opts.fsck.get() != 0 ||
            ca.has_data() & (1 << c::bch_data_type::BCH_DATA_journal.0) != 0
    }, true)?;
    read_devices(fs, &not_rw, &read, devs, false)?;

    let mut state = read.lock();

    let mut info = match newest_flush(fs, state.entries)? {
        Newest::Nothing => {
            bch_info!(fs, "journal read done, but no entries found");
            return Ok(c::journal_start_info::default());
        }
        Newest::NoFlush { cur_seq } => {
            fsck_err_on!(fs, fs.sb.clean.get() == 0, id::dirty_but_no_journal_entries_post_drop_nonflushes,
                         "journal read done, but no entries found after dropping non-flushes")?;
            return Ok(c::journal_start_info { cur_seq, ..Default::default() });
        }
        Newest::Flush(info) => info,
    };

    let mut drop_before = info.last_seq;
    {
        let mut buf = Printbuf::new();
        prt!(&mut buf, "journal read done, replaying entries {}-{}", info.last_seq, info.replay_end);

        /*
         * Drop blacklisted entries and entries older than last_seq (or start of
         * journal rewind:
         */
        let rewind = fs.opts.journal_rewind.get();
        if rewind != 0 {
            let rewind_seq = fs.journal.lock.rewind_seq.get();
            if rewind_seq != 0 && rewind < rewind_seq {
                bch_err!(fs, "cannot rewind to {}: discards have invalidated journal entries before {}",
                         rewind, rewind_seq);
                return fs.throw(bch_errcode::BCH_ERR_EINVAL_journal_rewind_before_discard);
            }
            drop_before = drop_before.min(rewind);
            prt!(&mut buf, " (rewinding from {})", rewind);
        }

        info.last_seq = drop_before;
        if info.replay_end + 1 != info.cur_seq {
            prt!(&mut buf, " (unflushed {}-{})", info.replay_end + 1, info.cur_seq - 1);
        }

        /*
         * Not replayed - just for the journal scrub, whose first
         * flush range starts here:
         */
        fs.journal_scrub_seq.set(if state.scrub && state.scrub_seq < drop_before { state.scrub_seq } else { 0 });
        if fs.journal_scrub_seq.get() != 0 {
            prt!(&mut buf, " (scrubbing from {})", fs.journal_scrub_seq.get());
        }
        bch_info!(fs, "{}", buf);
    }

    let keep_from = match fs.journal_scrub_seq.get() { 0 => drop_before, s => s };

    for slot in state.entries.slots_mut() {
        let Some(i) = slot.as_mut().filter(|i| !i.ignored()) else { continue };
        let seq = i.seq();
        if seq < keep_from {
            journal_replay_free(fs, slot, false);
            continue;
        }

        if fs.journal_seq_is_blacklisted(seq, true) {
            fsck_err_on!(fs, !i.j.no_flush(), id::jset_seq_blacklisted,
                         "found blacklisted journal entry {}", seq)?;
            i.ignore_blacklisted = true;
        }
    }

    /*
     * If the per-device bsearch fast path was used, it may legitimately
     * have gaps in any single device's bucket_seq (multi-device with
     * replication: seqs that went to other devices), so the only valid
     * place to verify completeness is here, against the union of all
     * devices' entries. If anything's missing, fall back to a full
     * per-device read before erroring.
     */
    if bsearch_used && journal_has_any_missing(fs, state.entries, drop_before, info.replay_end) {
        /*
         * Re-read every journal bucket on every member device that could
         * have used the bsearch fast path (nr > 32), in parallel, each
         * device's reader skipping bsearch for the peek-every-bucket
         * path. entry_add() dedupes by genradix slot so re-adding seqs
         * the initial round already found is a no-op.
         */
        drop(state);
        read_devices(fs, &not_rw, &read, journal_devs(fs, |ca| ca.journal.nr() > 32, false)?, true)?;
        state = read.lock();
    }

    journal_check_for_missing(fs, state.entries, drop_before, info.replay_end)?;
    validate_kept(fs, &not_rw, state.entries, drop_before)?;
    Ok(info)
}

fn validate_kept(fs: &Fs, not_rw: &NotRw<'_>, entries: &mut JournalEntries, drop_before: u64) -> Result<(), BchError> {
    for i in entries.slots_mut() {
        let Some(i) = i.as_mut().filter(|i| !i.ignored()) else { continue };

        /*
         * Don't print checksum errors until we know we're going to use
         * a given journal entry:
         */
        if i.ptrs().iter().any(|p| !p.csum_good) {
            journal_print_checksum_error(fs, i);
        }

        let p = i.ptrs()[0];
        let ca = fs.dev_get(p.dev.into());
        match validate::jset_validate(fs, ca.as_deref(), i.jset_bytes_mut(), p.sector, ValidateFlags::empty())? {
            Jset::Ok => {}
            /* its last_seq was past its seq, and repaired: its entries still need validating */
            Jset::Bad => validate::jset_validate_entries(fs, i.jset_bytes_mut(), ValidateFlags::empty())?,
            Jset::None => unreachable!("a kept entry's magic was checked when it was read"),
        }

        /*
         * Rewind state only from the entries we replay: one kept for the
         * journal scrub could hold a rewind range that's long done.
         */
        if i.seq() < drop_before {
            continue;
        }

        for entry in i.jset().entries() {
            match entry.typed() {
                Ok(c::JsetEntryRef::rewind_limit(r)) => {
                    let mut g = fs.journal.lock.lock();
                    let p = g.published();
                    p.rewind_seq.set(r.seq.get());
                    p.rewind_seq_ondisk.set(r.seq.get());
                }
                Ok(c::JsetEntryRef::rewind(r)) => {
                    let range = c::journal_rewind_range { from: r.from.get(), to: r.to.get() };
                    let _ = unsafe { not_rw.journal_rewind_ranges() }.push(range);
                }
                _ => {}
            }
        }
    }
    Ok(())
}

impl Fs {
    /* The journal scrub runs after an unclean shutdown, or always if asked to */
    pub fn journal_scrub_will_run(&self) -> bool {
        let opt = u32::from(self.opts.scrub_recent_journal_entries.get());
        opt != 0 &&
            (self.sb.clean.get() == 0 || opt == c::bch_scrub_journal_opts::BCH_SCRUB_JOURNAL_always.0)
    }
}

// ---- entry type conversion ----

/// Convert entry type byte to the raw journal entry type.
pub fn entry_type(entry: &Head<c::jset_entry>) -> c::bch_jset_entry_type {
    c::bch_jset_entry_type(entry.type_ as u32)
}

/// Convert entry btree_id byte to the enum, if it's a known btree.
pub fn entry_btree_id(entry: &Head<c::jset_entry>) -> Option<Btree> {
    Btree::from_raw(entry.btree_id as u32)
}

// ---- jset_entry_log helpers ----

/// A log entry's message, its NUL padding trimmed - empty for any other entry.
pub fn entry_log_msg(entry: &c::jset_entry) -> &[u8] {
    let Ok(c::JsetEntryRef::log(l)) = entry.typed() else { return &[] };
    let data = &l.d;
    // Trim trailing nulls
    let len = data.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    &data[..len]
}

pub fn entry_log_str_eq(entry: &c::jset_entry, s: &str) -> bool {
    let msg = entry_log_msg(entry);
    msg.len() >= s.len() && &msg[..s.len()] == s.as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring(nr: usize, tail: usize, live: usize, first: u64, step: u64) -> Vec<u64> {
        let mut r = vec![0; nr];
        for k in 0..live {
            r[(tail + k) % nr] = first + k as u64 * step;
        }
        r
    }

    fn newest_first(mut v: Vec<(usize, u64)>) -> Vec<(usize, u64)> {
        v.sort_by_key(|&(i, s)| (core::cmp::Reverse(s), i));
        v
    }

    fn in_use(r: &[u64]) -> Vec<(usize, u64)> {
        newest_first(r.iter().copied().enumerate().filter(|&(_, s)| s != 0).collect())
    }

    fn collect(r: &[u64]) -> (Vec<(usize, u64)>, usize) {
        let mut peeked = vec![false; r.len()];
        let order = bsearch_collect(r.len(), |i| { peeked[i] = true; r[i] }).unwrap();
        (newest_first(order.iter().copied().collect()), peeked.iter().filter(|&&p| p).count())
    }

    #[test]
    fn finds_the_live_buckets_wherever_they_wrap() {
        for nr in [1, 2, 3, 33, 64, 100] {
            for live in 1..=nr {
                for tail in 0..nr {
                    let r = ring(nr, tail, live, 1000, 1);
                    assert_eq!(collect(&r).0, in_use(&r), "nr {nr} tail {tail} live {live}");

                    /* found by the bisection, not the fallback's full peek */
                    let mut seq = |i: usize| r[i];
                    let anchor = anchor_bucket(nr, &mut seq).expect("a bucket in use");
                    assert_eq!(bsearch_head(nr, anchor, &mut seq), (tail + live - 1) % nr,
                               "nr {nr} tail {tail} live {live}");
                }
            }
        }
    }

    /// Replication: a device sees only the seqs written to it.
    #[test]
    fn gaps_in_the_seqs_are_still_one_range() {
        let r = ring(64, 50, 20, 1000, 3);
        let (order, peeked) = collect(&r);
        assert_eq!(order, in_use(&r));
        assert!(peeked < r.len(), "{peeked} peeks");
    }

    #[test]
    fn an_empty_journal_is_every_bucket_peeked() {
        let r = vec![0; 64];
        assert_eq!(collect(&r), (vec![], 64));
    }

    #[test]
    fn a_big_journal_is_mostly_not_peeked() {
        let r = ring(1024, 700, 10, 5000, 1);
        let (order, peeked) = collect(&r);
        assert_eq!(order, in_use(&r));
        assert!(peeked < r.len() / 2, "{peeked} peeks");
    }

    #[test]
    fn a_corrupt_ring_falls_back_to_every_bucket() {
        let mut r = ring(64, 10, 30, 1000, 1);
        r[20] = r[19];
        assert_eq!(collect(&r), (in_use(&r), 64));
    }

    #[test]
    fn monotonic_is_decreasing_back_from_the_head_then_empty() {
        let mut r = ring(16, 12, 8, 100, 1);
        let head = 3;
        assert!(bucket_seqs_monotonic(&r, head, &mut Printbuf::new()));

        r[8] = 50;      /* in use past the empty run */
        assert!(!bucket_seqs_monotonic(&r, head, &mut Printbuf::new()));

        let mut r = ring(16, 12, 8, 100, 1);
        r.swap(14, 15);
        assert!(!bucket_seqs_monotonic(&r, head, &mut Printbuf::new()));
    }
}
