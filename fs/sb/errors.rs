// SPDX-License-Identifier: GPL-2.0

//! The superblock's error counts: how often each kind of error -
//! SbErrorId, errors_format.rs - has happened to this filesystem, and when
//! it first and last did. The first thing a field report wants to know.
//!
//! On disk they're one of two sections, sorted strictly ascending by id:
//! BCH_SB_FIELD_errors, an id, a count and the last time; or
//! BCH_SB_FIELD_errors_v2, which adds the first time and saturates the count
//! and times instead of wrapping. A v1 entry's one time is both its first and
//! its last - the error happened at least that recently - which is how
//! ErrorEntry reads it, so the two sections are read as one.
//!
//! An error id is a permanent on-disk number, never reused or renumbered:
//! the ids are checked at build time to be exactly 0..=MAX.

#![allow(non_camel_case_types)]

use core::cmp::{Ordering, Reverse};
use core::ffi::c_int;
use cstruct_macros::{MaybeZeroable, bitfield, TypeInfo, BitfieldMembers};
use crate::btree::bkey::ValidateFlags;
use crate::c;
pub use crate::cstructs::sb_errors_format::{FsckFlags, SbErrorId};
use crate::util::vstructs::Unvalidated;
use crate::errcode::{Errcode, BchError};
use crate::fs::{Fs, SbLockGuard};
use crate::{prt, sb_err_on};
use crate::sb::io::{sb_field_u64s, SbFieldOps};
use crate::util::alloc::{kvvec_insert, KVVec};
use crate::os::kernel::ktime_get_real_seconds;
use crate::util::{DateTime, Printbuf};
use crate::sb::io::SbResize;
use nestify::nest;

nest! {
    #[repr(C)]
    #[derive(Clone, Copy, Default, MaybeZeroable, BitfieldMembers, TypeInfo)]
    pub struct bch_sb_error_entry_cpu {
        #[members]
        #>[bitfield(u64)]
        pub bits: pub struct bch_sb_error_entry_cpu_bits {
            #[bits(16)]
            id: SbErrorId,
            nr: u48,
        },
        pub first_error_time: u64,
        pub last_error_time: u64,
    }
}

// ── Error ids, and their names ───────────────────────────────────────────────

/// @ids by name, comma separated.
pub fn prt_error_ids(out: &mut Printbuf, ids: impl IntoIterator<Item = SbErrorId>) {
    for (i, id) in ids.into_iter().enumerate() {
        if i != 0 {
            write!(out, ",");
        }
        prt!(out, "{}", id);
    }
}

/// A v2 count, which saturates: one that did is a floor, not an exact value.
pub fn prt_error_nr(out: &mut Printbuf, nr: u64) {
    write!(out, "{nr}");
    if nr >= c::BCH_SB_ERROR_ENTRY_V2_NR_MAX {
        write!(out, "+");
    }
}

// ── The sections ─────────────────────────────────────────────────────────────

/// An entry of either section, as v2 has it: see the top of the file.
pub(crate) trait ErrorEntry: Copy {
    /// v2's: a first time, and a count that saturates.
    const V2: bool;

    fn id(&self) -> SbErrorId;
    fn nr(&self) -> u64;
    fn first(&self) -> u64;
    fn last(&self) -> u64;
}

impl ErrorEntry for c::bch_sb_field_error_entry {
    const V2: bool = false;

    fn id(&self) -> SbErrorId {
        self.v.id()
    }

    fn nr(&self) -> u64 {
        self.v.nr()
    }

    fn first(&self) -> u64 {
        self.last()
    }

    fn last(&self) -> u64 {
        self.last_error_time.get()
    }
}

impl ErrorEntry for c::bch_sb_field_error_entry_v2 {
    const V2: bool = true;

    fn id(&self) -> SbErrorId {
        self.v.id()
    }

    fn nr(&self) -> u64 {
        self.v.nr().into()
    }

    fn first(&self) -> u64 {
        self.v.first_error_time()
    }

    fn last(&self) -> u64 {
        self.v.last_error_time()
    }
}

impl c::bch_sb_field_error_entry_v2 {
    /// A count past its field's width is pinned at the most it holds, and so
    /// is a time, as C's setters pin them: a wrapped one would be worse.
    pub(crate) fn new(id: SbErrorId, nr: u64, first: u64, last: u64) -> Self {
        let mut e = Self::default();
        e.v.set_id(id);
        e.v.set_nr(nr.min(c::BCH_SB_ERROR_ENTRY_V2_NR_MAX) as u32);
        e.v.set_first_error_time(first.min(c::BCH_SB_ERROR_ENTRY_V2_TIME_MAX));
        e.v.set_last_error_time(last.min(c::BCH_SB_ERROR_ENTRY_V2_TIME_MAX));
        e
    }
}

/// Memory's count as v2 has it.
impl From<&c::bch_sb_error_entry_cpu> for c::bch_sb_field_error_entry_v2 {
    fn from(s: &c::bch_sb_error_entry_cpu) -> Self {
        Self::new(s.id(), s.nr(), s.first_error_time, s.last_error_time)
    }
}

fn validate<E: ErrorEntry>(entries: &[E], err: &mut Printbuf) -> Result<(), BchError> {
    for (i, e) in entries.iter().enumerate() {
        sb_err_on!(err, e.nr() == 0, BCH_ERR_invalid_sb_errors, "entry with count 0 (id {})", e.id())?;

        // never for v1, whose first is its last
        sb_err_on!(err, e.first() > e.last(), BCH_ERR_invalid_sb_errors,
                   "entry with first occurrence after last (id {})", e.id())?;

        sb_err_on!(err, entries.get(i + 1).is_some_and(|n| e.id() >= n.id()), BCH_ERR_invalid_sb_errors,
                   "entries out of order")?;
    }
    Ok(())
}

/// Newest first. If there's no memory to sort them in, in id order.
fn to_text<E: ErrorEntry>(out: &mut Printbuf, entries: &[E]) {
    if out.nr_tabstops() <= 1 {
        out.tabstop_push(16);
    }

    let mut sorted = KVVec::new();
    let entries = match sorted.extend_from_slice(entries) {
        Ok(()) => {
            sorted.sort_unstable_by_key(|e: &E| Reverse(e.last()));
            &sorted[..]
        }
        Err(_) => entries,
    };

    /* the space: a name as long as the column is still separated from its count */
    for e in entries {
        prt!(out, "{} \t", e.id());
        if E::V2 {
            prt_error_nr(out, e.nr());
            prt!(out, "\t{}", DateTime(e.first() as i64));
        } else {
            write!(out, "{}", e.nr());
        }
        prt!(out, "\t{}\n", DateTime(e.last() as i64));
    }
}

impl SbFieldOps for c::bch_sb_field_errors {
    fn validate(_sb: &c::bch_sb<Unvalidated>, f: &c::bch_sb_field_errors<Unvalidated>,
                _flags: ValidateFlags, err: &mut Printbuf) -> Result<(), BchError> {
        validate(&f.entries, err)
    }

    fn to_text(out: &mut Printbuf, _fs: Option<&Fs>, _sb: &c::bch_sb<Unvalidated>,
                f: &c::bch_sb_field_errors<Unvalidated>) {
        to_text(out, &f.entries)
    }
}

impl SbFieldOps for c::bch_sb_field_errors_v2 {
    fn validate(_sb: &c::bch_sb<Unvalidated>, f: &c::bch_sb_field_errors_v2<Unvalidated>,
                _flags: ValidateFlags, err: &mut Printbuf) -> Result<(), BchError> {
        validate(&f.entries, err)
    }

    fn to_text(out: &mut Printbuf, _fs: Option<&Fs>, _sb: &c::bch_sb<Unvalidated>,
                f: &c::bch_sb_field_errors_v2<Unvalidated>) {
        to_text(out, &f.entries)
    }

    /// The counts, both sections'. Both present means the filesystem was
    /// downgraded: writing v2 always deletes v1 in the same superblock write,
    /// so a v1 alongside v2 was written by an old kernel since, and holds
    /// exactly the counts accrued since. They're merged - nothing lost or
    /// counted twice across a downgrade and upgrade - and the next superblock
    /// write folds everything back into v2.
    fn to_cpu(fs: &Fs, sb: &mut SbLockGuard<'_>) -> Result<(), BchError> {
        let v2: &[c::bch_sb_field_error_entry_v2] = sb.field::<c::bch_sb_field_errors_v2>().map_or(&[], |f| &f.entries);
        let v1: &[c::bch_sb_field_error_entry]    = sb.field::<c::bch_sb_field_errors>().map_or(&[], |f| &f.entries);

        let mut counts = fs.errors.counts.lock();
        counts.clear();
        counts.reserve(v2.len() + v1.len())?;

        // validated strictly ascending by id: a sorted merge, a section
        // that's run out reading as past every id
        let (mut v2, mut v1) = (v2.iter().peekable(), v1.iter().peekable());
        loop {
            let a = v2.peek().map_or(u64::MAX, |e| e.id().0.into());
            let b = v1.peek().map_or(u64::MAX, |e| e.id().0.into());
            let e = match a.cmp(&b) {
                Ordering::Less    => v2.next().unwrap().into(),
                Ordering::Greater => v1.next().unwrap().into(),
                Ordering::Equal if a == u64::MAX => return Ok(()),
                Ordering::Equal   => merge(v2.next().unwrap().into(), v1.next().unwrap().into()),
            };
            counts.push(e)?;
        }
    }

    /// The counts - memory's - and v1 deleted, which to_cpu() merged in. If
    /// v2 can't be grown, the superblock keeps what it had until the next
    /// write.
    fn from_cpu(fs: &Fs, guard: &mut SbLockGuard<'_>) -> Result<(), BchError> {
        let mut sb = fs.sb_resize(guard);
        let counts = fs.errors.counts.lock();

        let u64s = sb_field_u64s::<c::bch_sb_field_errors_v2>(counts.len());
        let Some(f) = sb.field_resize::<c::bch_sb_field_errors_v2>(u64s) else { return Ok(()) };

        // what the resize left there doesn't matter: each entry is overwritten
        for (d, s) in f.entries.iter_mut().zip(counts.iter()) {
            *d = s.into();
        }

        sb.field_delete(c::SbFieldType::errors);
        Ok(())
    }
}

// ── Which error an errcode is counted as ─────────────────────────────────────
//
// Each is passed a failure, negative, and matches on its magnitude.

/*
 * Block statuses we count separately: what the device said is the first
 * thing worth knowing from a filesystem that's logging IO errors - media
 * errors mean the drive is going, timeouts mean it's hanging, transport
 * errors point at the fabric, and INVAL means the request itself was
 * rejected: not an error the device hit, an error in what we handed it.
 *
 * Curated by hand rather than generated from BLK_ERRS(): sb error ids are
 * permanent on-disk numbers, so we don't spend them on statuses that can't
 * reach a bcachefs completion - DM_REQUEUE is consumed inside dm, and the
 * zone codes want zoned device support we don't have. Anything left out
 * counts as blk_sts_unknown; to promote one, add it here and add a
 * numbered entry in errors_format.rs - the build breaks if you forget.
 */
/// @err a BCH_ERR_BLK_STS_* errcode, as blk_status_to_bch_err() throws: its
/// status's error.
pub fn blk_sts_sb_err(err: c_int) -> SbErrorId {
    match Errcode(err.unsigned_abs()) {
        Errcode::BCH_ERR_BLK_STS_NOTSUPP => SbErrorId::blk_sts_notsupp,
        Errcode::BCH_ERR_BLK_STS_TIMEOUT => SbErrorId::blk_sts_timeout,
        Errcode::BCH_ERR_BLK_STS_NOSPC => SbErrorId::blk_sts_nospc,
        Errcode::BCH_ERR_BLK_STS_TRANSPORT => SbErrorId::blk_sts_transport,
        Errcode::BCH_ERR_BLK_STS_TARGET => SbErrorId::blk_sts_target,
        Errcode::BCH_ERR_BLK_STS_RESV_CONFLICT => SbErrorId::blk_sts_resv_conflict,
        Errcode::BCH_ERR_BLK_STS_MEDIUM => SbErrorId::blk_sts_medium,
        Errcode::BCH_ERR_BLK_STS_PROTECTION => SbErrorId::blk_sts_protection,
        Errcode::BCH_ERR_BLK_STS_RESOURCE => SbErrorId::blk_sts_resource,
        Errcode::BCH_ERR_BLK_STS_IOERR => SbErrorId::blk_sts_ioerr,
        Errcode::BCH_ERR_BLK_STS_AGAIN => SbErrorId::blk_sts_again,
        Errcode::BCH_ERR_BLK_STS_DEV_RESOURCE => SbErrorId::blk_sts_dev_resource,
        Errcode::BCH_ERR_BLK_STS_OFFLINE => SbErrorId::blk_sts_offline,
        Errcode::BCH_ERR_BLK_STS_DURATION_LIMIT => SbErrorId::blk_sts_duration_limit,
        Errcode::BCH_ERR_BLK_STS_INVAL => SbErrorId::blk_sts_inval,
        Errcode::BCH_ERR_BLK_STS_REMOVED => SbErrorId::blk_sts_removed,
        _ => SbErrorId::blk_sts_unknown,
    }
}

/*
 * Decompression failures we count separately: an extent that won't
 * decompress is a different animal from one that won't read or won't
 * checksum - as far as the checksum can tell the data came off the device
 * intact, and then didn't decode. Counting per compression type answers
 * the first question such a report raises, which is whether one algorithm
 * is at fault or all of them are.
 *
 * The zstd codes separate that further: corruption_detected and
 * checksum_wrong say the compressed data is bad, dst_size_too_small and
 * memory_allocation say we drove zstd wrong or couldn't allocate.
 *
 * Curated by hand for the same reason as the block statuses above - sb
 * error ids are permanent on-disk numbers. zstd defines 27 error codes,
 * most of which zstd_decompress_dctx() can't return for the single raw
 * frame we hand it; those count as data_decompress_err_zstd_unknown, and
 * anything not a decompress errcode at all as data_decompress_err_unknown.
 * To promote one, add it here and add a numbered entry in
 * errors_format.rs, and an ErrorKeyReason - the build breaks if
 * you forget.
 */
/// @err a BCH_ERR_decompress errcode, as buf_uncompress() throws: what it's
/// counted as.
pub fn bch2_decompress_sb_err(err: c_int) -> SbErrorId {
    match Errcode(err.unsigned_abs()) {
        Errcode::BCH_ERR_decompress_exceeded_max_encoded_extent => SbErrorId::data_decompress_err_exceeded_max_encoded_extent,
        Errcode::BCH_ERR_decompress_lz4_old => SbErrorId::data_decompress_err_lz4_old,
        Errcode::BCH_ERR_decompress_lz4 => SbErrorId::data_decompress_err_lz4,
        Errcode::BCH_ERR_decompress_gzip => SbErrorId::data_decompress_err_gzip,
        Errcode::BCH_ERR_decompress_gzip_size_mismatch => SbErrorId::data_decompress_err_gzip_size_mismatch,
        Errcode::BCH_ERR_decompress_zstd_src_len_bad => SbErrorId::data_decompress_err_zstd_src_len_bad,
        Errcode::BCH_ERR_decompress_zstd_size_mismatch => SbErrorId::data_decompress_err_zstd_size_mismatch,
        Errcode::BCH_ERR_ZSTD_error_corruption_detected => SbErrorId::data_decompress_err_zstd_corruption_detected,
        Errcode::BCH_ERR_ZSTD_error_checksum_wrong => SbErrorId::data_decompress_err_zstd_checksum_wrong,
        Errcode::BCH_ERR_ZSTD_error_prefix_unknown => SbErrorId::data_decompress_err_zstd_prefix_unknown,
        Errcode::BCH_ERR_ZSTD_error_srcSize_wrong => SbErrorId::data_decompress_err_zstd_src_size_wrong,
        Errcode::BCH_ERR_ZSTD_error_dstSize_tooSmall => SbErrorId::data_decompress_err_zstd_dst_size_too_small,
        Errcode::BCH_ERR_ZSTD_error_memory_allocation => SbErrorId::data_decompress_err_zstd_memory_allocation,
        code if BchError::from(code).matches(Errcode::BCH_ERR_zstd_error) =>
            SbErrorId::data_decompress_err_zstd_unknown,
        _ => SbErrorId::data_decompress_err_unknown,
    }
}

// ── The counts, in memory ────────────────────────────────────────────────────
//
// c->errors.counts, sorted by id: read in from the superblock at mount, counted
// in as errors happen, and written out with every superblock write. Lock
// order: sb_lock, then counts - sb_errors_from_cpu() takes counts under
// the superblock write's sb_lock.

/// The largest in-memory count: nr is 48 bits.
const CPU_NR_MAX: u64 = (1 << 48) - 1;

fn cpu_entry(id: SbErrorId, nr: u64, first: u64, last: u64) -> c::bch_sb_error_entry_cpu {
    let mut e = c::bch_sb_error_entry_cpu {
        first_error_time: first,
        last_error_time:  last,
        ..Default::default()
    };
    e.set_id(id);
    e.set_nr(nr.min(CPU_NR_MAX));
    e
}

impl<E: ErrorEntry> From<&E> for c::bch_sb_error_entry_cpu {
    fn from(e: &E) -> Self {
        cpu_entry(e.id(), e.nr(), e.first(), e.last())
    }
}

/// One error's counts from both sections: summed, over the union of their
/// time ranges.
pub(crate) fn merge(a: c::bch_sb_error_entry_cpu, b: c::bch_sb_error_entry_cpu) -> c::bch_sb_error_entry_cpu {
    cpu_entry(a.id(), a.nr().saturating_add(b.nr()),
              a.first_error_time.min(b.first_error_time),
              a.last_error_time.max(b.last_error_time))
}

impl Fs {
    /// Count @err, as having happened now. With no memory for a new entry,
    /// it isn't counted.
    pub fn sb_error_count(&self, err: SbErrorId) {
        let now = ktime_get_real_seconds() as u64;
        let mut counts = self.errors.counts.lock();

        match counts.binary_search_by_key(&err, |e| e.id()) {
            Ok(i) => {
                let e = &mut counts[i];
                e.update_nr(|n| (n + 1).min(CPU_NR_MAX));
                e.last_error_time = now;
            }
            Err(i) => {
                let _ = kvvec_insert(&mut counts, i, cpu_entry(err, 1, now, now));
            }
        }
    }

    /// The counts in memory, by id.
    pub fn errors_to_text(&self, out: &mut Printbuf) {
        for (i, w) in [48, 8, 16, 16].into_iter().enumerate() {
            if out.nr_tabstops() <= i as u32 {
                out.tabstop_push(w);
            }
        }

        for e in self.errors.counts.lock().iter() {
            prt!(out, "{} \t{}\t{}\t{}\n", e.id(), e.nr(),
                 DateTime(e.first_error_time as i64), DateTime(e.last_error_time as i64));
        }
    }
}

