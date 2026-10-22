// SPDX-License-Identifier: GPL-2.0

//! The superblock's journal buckets, per device: BCH_SB_FIELD_journal, a
//! bucket per entry, and journal_v2, which replaced it, a run of contiguous
//! buckets per entry. Both are validated as runs - v1's of one bucket.

use core::ffi::{c_int, c_uint};
use core::ops::Range;

use cstruct_macros::c_export;

use crate::c;
use crate::util::vstructs::Unvalidated;
use crate::errcode::{bch_errcode, ret_to_c, BchError};
use crate::fs::{Fs, SbLockGuard};
use crate::prt;
use crate::sb::io::{sb_field_u64s, SbFieldOps, SbState};
use crate::sb::members::sb_member;
use crate::types::Le64;
use crate::util::alloc::{flags::GFP_KERNEL, KVVec};
use crate::util::ffi::Opaque;
use crate::util::Printbuf;

fn invalid() -> BchError {
    bch_errcode::BCH_ERR_invalid_sb_journal.into()
}

/// @runs are on the device and don't overlap - so they total no more than
/// its nbuckets, which members validation, first, has capped below u32.
fn runs_validate<S: SbState>(sb: &c::bch_sb<S>, mut runs: KVVec<Range<u64>>, err: &mut Printbuf)
    -> Result<(), BchError>
{
    runs.sort_unstable_by_key(|r| r.start);

    let (Some(first), Some(last)) = (runs.first(), runs.last()) else {
        return Ok(());
    };
    let m = sb_member(sb, S::head(sb).dev_idx.into()).unwrap_or_default();

    if first.start == 0 {
        prt!(err, "journal bucket at sector 0");
        return Err(invalid());
    }

    if first.start < m.first_bucket.get().into() {
        prt!(err, "journal bucket {} before first bucket {}", first.start, m.first_bucket.get());
        return Err(invalid());
    }

    if last.end > m.nbuckets.get() {
        prt!(err, "journal bucket {} past end of device (nbuckets {})", last.end - 1, m.nbuckets.get());
        return Err(invalid());
    }

    if let Some(w) = runs.windows(2).find(|w| w[0].end > w[1].start) {
        prt!(err, "duplicate journal buckets in ranges {}-{}, {}-{}",
             w[0].start, w[0].end, w[1].start, w[1].end);
        return Err(invalid());
    }

    Ok(())
}

impl SbFieldOps for c::bch_sb_field_journal {
    fn validate(sb: &c::bch_sb<Unvalidated>, f: &c::bch_sb_field_journal<Unvalidated>,
                _flags: c::bch_validate_flags, err: &mut Printbuf) -> Result<(), BchError> {
        let mut runs = KVVec::with_capacity(f.buckets.len(), GFP_KERNEL)?;
        for b in &f.buckets {
            runs.push(b.get()..b.get().saturating_add(1), GFP_KERNEL)?;
        }
        runs_validate(sb, runs, err)
    }

    fn to_text(out: &mut Printbuf, _fs: Option<&Fs>, _sb: &c::bch_sb<Unvalidated>,
                f: &c::bch_sb_field_journal<Unvalidated>) {
        prt!(out, "Buckets: ");
        for b in &f.buckets {
            prt!(out, " {}", b.get());
        }
        prt!(out, "\n");
    }
}

impl SbFieldOps for c::bch_sb_field_journal_v2 {
    fn validate(sb: &c::bch_sb<Unvalidated>, f: &c::bch_sb_field_journal_v2<Unvalidated>,
                _flags: c::bch_validate_flags, err: &mut Printbuf) -> Result<(), BchError> {
        let mut runs = KVVec::with_capacity(f.d.len(), GFP_KERNEL)?;
        for d in &f.d {
            let (start, nr) = (d.start.get(), d.nr.get());
            let Some(end) = start.checked_add(nr).filter(|&end| end > start) else {
                prt!(err, "journal buckets entry with bad nr: {start}+{nr}");
                return Err(invalid());
            };
            runs.push(start..end, GFP_KERNEL)?;
        }
        runs_validate(sb, runs, err)
    }

    fn to_text(out: &mut Printbuf, _fs: Option<&Fs>, _sb: &c::bch_sb<Unvalidated>,
                f: &c::bch_sb_field_journal_v2<Unvalidated>) {
        prt!(out, "Buckets: ");
        for d in &f.d {
            prt!(out, " {}-{}", d.start.get(), d.start.get().wrapping_add(d.nr.get()));
        }
        prt!(out, "\n");
    }
}

/// @ca's journal buckets, in ring order, to its superblock: journal_v2, a
/// run of contiguous buckets per entry, replacing v1.
///
/// v1 goes once v2 is written: deleting a field moves those after it down -
/// v2, if it was added after v1 - so v2 can't be written through a pointer
/// from before (C did: its entries landed v1's size past v2). And on failure,
/// the superblock is as it was.
fn buckets_to_sb(fs: &Fs, sb: &mut SbLockGuard<'_>, ca: &c::bch_dev, buckets: &[u64])
    -> Result<(), BchError>
{
    let mut dev_sb = fs.dev_sb_mut(sb, ca).expect("journal buckets: a device with no superblock");

    if buckets.is_empty() {
        dev_sb.field_delete::<c::bch_sb_field_journal>();
        dev_sb.field_delete::<c::bch_sb_field_journal_v2>();
        return Ok(());
    }

    let runs = || buckets.chunk_by(|a, b| a + 1 == *b);
    let u64s = sb_field_u64s::<c::bch_sb_field_journal_v2>(runs().count());
    let Some(j) = dev_sb.field_resize::<c::bch_sb_field_journal_v2>(u64s) else {
        return fs.throw(bch_errcode::BCH_ERR_ENOSPC_sb_journal);
    };

    for (d, run) in j.d.iter_mut().zip(runs()) {
        d.start = Le64::new(run[0]);
        d.nr    = Le64::new(run.len() as u64);
    }

    dev_sb.field_delete::<c::bch_sb_field_journal>();
    Ok(())
}

/// # Safety
/// @buckets is @nr u64s; sb_lock is held.
#[c_export]
pub unsafe fn bch2_journal_buckets_to_sb(c: &Opaque<c::bch_fs>, ca: &c::bch_dev,
                                         buckets: *const u64, nr: c_uint) -> c_int {
    let fs = Fs::from_c(c);
    let buckets = match nr {
        0  => &[],
        // SAFETY: the caller's
        nr => unsafe { core::slice::from_raw_parts(buckets, nr as usize) },
    };
    // SAFETY: the caller holds it
    let mut sb = unsafe { fs.sb_lock_held() };
    ret_to_c(buckets_to_sb(fs, &mut sb, ca, buckets))
}

/// At a clean mount, before the journal starts: rewrite journal_v2 sorted,
/// for devices where it isn't or a v1 field is left.
#[c_export]
pub fn bch2_sb_journal_sort(c: &Opaque<c::bch_fs>) -> c_int {
    let fs = Fs::from_c(c);
    assert!(fs.sb.clean.get() != 0);
    assert!(!fs.flag(c::bch_fs_flags::BCH_FS_rw));

    let mut sb = fs.sb_lock();
    let mut dirty = false;

    for ca in fs.online_members(c::bch_dev_read_ref::BCH_DEV_READ_REF_sb_journal_sort) {
        let Some(dev_sb) = fs.dev_sb(&sb, &ca) else { continue };
        let Some(j) = dev_sb.field::<c::bch_sb_field_journal_v2>() else { continue };

        if !j.d.is_sorted_by_key(|d| d.start.get()) ||
           dev_sb.field::<c::bch_sb_field_journal>().is_some() {
            let ja = &ca.journal;
            let buckets: &mut [u64] = match ja.nr {
                0  => &mut [],
                // SAFETY: the journal isn't started - nothing else has its buckets
                nr => unsafe { core::slice::from_raw_parts_mut(ja.buckets, nr as usize) },
            };
            buckets.sort_unstable();

            // As C: if it fails, the superblock's as it was, and still valid
            let _ = buckets_to_sb(fs, &mut sb, &ca, buckets);
            dirty = true;
        }
    }

    ret_to_c(if dirty { fs.write_super_ret(&mut sb) } else { Ok(()) })
}
