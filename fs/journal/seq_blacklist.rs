// SPDX-License-Identifier: GPL-2.0

//! Blacklisted journal sequence numbers.
//!
//! To guarantee order of btree updates after a crash, we need to detect when a
//! btree node entry (bset) is newer than the newest journal entry that was
//! successfully written, and ignore it - effectively ignoring any btree updates
//! that didn't make it into the journal.
//!
//! If we didn't do this, we might have two btree nodes, a and b, both with
//! updates that weren't written to the journal yet: if b was updated after a,
//! but b was flushed and not a - oops; on recovery we'll find that the updates
//! to b happened, but not the updates to a that happened before it.
//!
//! Ignoring bsets that are newer than the newest journal entry is always safe,
//! because everything they contain will also have been journalled - and must
//! still be present in the journal on disk until a journal entry has been
//! written _after_ that bset was written.
//!
//! To accomplish this, bsets record the newest journal sequence number they
//! contain updates for; then, on startup, the btree code queries the journal
//! code to ask "Is this sequence number newer than the newest journal entry? If
//! so, ignore it."
//!
//! When this happens, we must blacklist that journal sequence number: the
//! journal must not write any entries with that sequence number, and it must
//! record that it was blacklisted so that a) on recovery we don't think we have
//! missing journal entries and b) so that the btree code continues to ignore
//! that bset, until that btree node is rewritten.
//!
//! The blacklist is BCH_SB_FIELD_journal_seq_blacklist: ranges [start, end),
//! sorted and not overlapping.

use core::ffi::c_int;
use core::sync::atomic::{AtomicBool, Ordering::Relaxed};

use cstruct_macros::c_export;

use crate::bch_verbose;
use crate::c;
use crate::util::vstructs::Unvalidated;
use crate::errcode::{bch_errcode, ret_to_c, BchError};
use crate::fs::{Fs, SbLockGuard};
use crate::sb::io::{sb_field_u64s, SbFieldOps};
use crate::types::Le64;
use crate::util::eytzinger::Eytzinger0;
use crate::util::ffi::Opaque;
use crate::util::rcu::{RcuArray, RcuElement};
use crate::util::Printbuf;

type Section = c::bch_sb_field_journal_seq_blacklist;

impl SbFieldOps for Section {
    fn validate(_sb: &c::bch_sb<Unvalidated>, f: &c::bch_sb_field_journal_seq_blacklist<Unvalidated>,
                _flags: c::bch_validate_flags, err: &mut Printbuf) -> Result<(), BchError> {
        for (i, e) in f.start.iter().enumerate() {
            let (start, end) = (e.start.get(), e.end.get());
            if start >= end {
                write!(err, "entry {i} start >= end ({start} >= {end})");
                return Err(bch_errcode::BCH_ERR_invalid_sb_journal_seq_blacklist.into());
            }

            if let Some(next) = f.start.get(i + 1).filter(|n| end > n.start.get()) {
                write!(err, "entry {} out of order with next entry ({end} > {})", i + 1, next.start.get());
                return Err(bch_errcode::BCH_ERR_invalid_sb_journal_seq_blacklist.into());
            }
        }
        Ok(())
    }

    fn to_text(out: &mut Printbuf, _fs: Option<&Fs>, _sb: &c::bch_sb<Unvalidated>,
                f: &c::bch_sb_field_journal_seq_blacklist<Unvalidated>) {
        for (i, e) in f.start.iter().enumerate() {
            if i != 0 {
                write!(out, " ");
            }
            write!(out, "{}-{}", e.start.get(), e.end.get());
        }
        out.newline();
    }
}

// ── The table, in memory ─────────────────────────────────────────────────────
//
// c->journal_seq_blacklist: the section's ranges, in eytzinger order, looked up
// without a lock - from btree node reads, from journal entry open under
// j->lock, and from the journal read - under RCU. It's rebuilt from the
// superblock under sb_lock, the writers' lock, at mount and when a range is
// added; a rebuild starts every range clean.
//
// That loses nothing gc needs: the only marks made before the last rebuild are
// the journal read's, on ranges of entries still on disk, which gc keeps
// anyway - their end is past oldest_seq_found_ondisk.

/// A blacklisted range, [start, end): dirty once a bset or journal entry in it
/// has been found, so gc keeps it.
pub struct BlacklistEntry {
    start: u64,
    end:   u64,
    dirty: AtomicBool,
}

// SAFETY: integers and an atomic: a bytewise copy is one, owning nothing, and
// zeroes are an empty range
unsafe impl RcuElement for BlacklistEntry {}

pub type JournalSeqBlacklist = RcuArray<BlacklistEntry>;

impl JournalSeqBlacklist {
    /// Whether @seq is blacklisted - if @dirty, noting that something in its
    /// range was found, for gc.
    pub fn is_blacklisted(&self, seq: u64, dirty: bool) -> bool {
        self.with(|t| {
            let Some(e) = Eytzinger0::find_le_by(t, |e| e.start.cmp(&seq)).map(|i| &t[i]) else {
                return false;
            };
            if seq >= e.end {
                return false;
            }
            if dirty {
                e.dirty.store(true, Relaxed);
            }
            true
        })
    }

    /// The first blacklisted seq from @seq on: u64::MAX if there's none.
    pub fn next_blacklisted(&self, seq: u64) -> u64 {
        self.with(|t| {
            Eytzinger0::find_gt_by(t, |e| e.end.cmp(&seq)).map_or(u64::MAX, |i| seq.max(t[i].start))
        })
    }

    /// The first seq from @seq on that isn't blacklisted.
    pub fn next_nonblacklisted(&self, mut seq: u64) -> u64 {
        self.with(|t| {
            while let Some(i) = Eytzinger0::find_le_by(t, |e| e.start.cmp(&seq)).filter(|&i| t[i].end > seq) {
                seq = t[i].end;
            }
            seq
        })
    }

    /// The last blacklisted seq: 0 if there's none.
    pub fn last_blacklisted(&self) -> u64 {
        self.with(|t| Eytzinger0::of(t).last().map_or(0, |i| t[i].end - 1))
    }
}

/// [@start, @end) blacklisted in @sb's section, merged with the ranges it
/// overlaps or touches, which go. False if there's no room for it.
fn section_add(sb: &mut c::bch_sb_handle, mut start: u64, mut end: u64) -> bool {
    // @i is where the merged range goes, @nr the ranges kept
    let (mut i, mut nr) = (0, 0);
    if let Some(f) = sb.field_mut::<Section>() {
        let bl = &mut f.start;
        nr = bl.len();
        while i < nr {
            let (s, e) = (bl[i].start.get(), bl[i].end.get());
            if end < s {
                break;
            }
            if start > e {
                i += 1;
                continue;
            }
            start = start.min(s);
            end   = end.max(e);
            bl[i..nr].rotate_left(1);
            nr -= 1;
        }
    }

    let Some(f) = sb.field_resize::<Section>(sb_field_u64s::<Section>(nr + 1)) else { return false };
    f.start[i..].rotate_right(1);
    f.start[i] = c::journal_seq_blacklist_entry { start: Le64::new(start), end: Le64::new(end) };

    sb.sb_mut().features[0] |= 1 << c::bch_sb_feature::BCH_FEATURE_journal_seq_blacklist_v3.0;
    true
}

impl Fs {
    /// JournalSeqBlacklist::is_blacklisted(), for the tools.
    pub fn journal_seq_is_blacklisted(&self, seq: u64, dirty: bool) -> bool {
        self.journal_seq_blacklist.is_blacklisted(seq, dirty)
    }

    /// The table, from the superblock's section. A filesystem that's never
    /// blacklisted anything has none: nothing's published, and lookups find
    /// nothing.
    fn journal_seq_blacklist_rebuild(&self, sb: &SbLockGuard<'_>) -> Result<(), BchError> {
        let Some(bl) = sb.field::<Section>() else { return Ok(()) };

        // SAFETY: under sb_lock, the writers' lock
        unsafe {
            self.journal_seq_blacklist.rebuild(bl.start.len(), |t| {
                for (t, e) in t.iter_mut().zip(&bl.start) {
                    t.start = e.start.get();
                    t.end   = e.end.get();
                }
                Eytzinger0::sort_by(t, |l, r| l.start.cmp(&r.start));
            })
        }.map_err(|_| self.err(bch_errcode::BCH_ERR_ENOMEM_blacklist_table_init))
    }

    /// Blacklist [@start, @end), and write the superblock.
    pub fn journal_seq_blacklist_add(&self, start: u64, end: u64) -> Result<(), BchError> {
        let mut sb = self.sb_lock();
        if !section_add(&mut sb, start, end) {
            return self.throw(bch_errcode::BCH_ERR_ENOSPC_sb_journal_seq_blacklist);
        }
        self.write_super_ret(&mut sb)?;
        self.journal_seq_blacklist_rebuild(&sb)
    }
}

#[c_export]
pub fn bch2_journal_seq_is_blacklisted(c: &Opaque<c::bch_fs>, seq: u64, dirty: bool) -> bool {
    Fs::from_c(c).journal_seq_is_blacklisted(seq, dirty)
}

#[c_export]
pub fn bch2_journal_seq_next_blacklisted(c: &Opaque<c::bch_fs>, seq: u64) -> u64 {
    Fs::from_c(c).journal_seq_blacklist.next_blacklisted(seq)
}

#[c_export]
pub fn bch2_journal_seq_next_nonblacklisted(c: &Opaque<c::bch_fs>, seq: u64) -> u64 {
    Fs::from_c(c).journal_seq_blacklist.next_nonblacklisted(seq)
}

#[c_export]
pub fn bch2_journal_last_blacklisted_seq(c: &Opaque<c::bch_fs>) -> u64 {
    Fs::from_c(c).journal_seq_blacklist.last_blacklisted()
}

#[c_export]
pub fn bch2_journal_seq_blacklist_add(c: &Opaque<c::bch_fs>, start: u64, end: u64) -> c_int {
    ret_to_c(Fs::from_c(c).journal_seq_blacklist_add(start, end))
}

/// At mount.
#[c_export]
pub fn bch2_blacklist_table_initialize(c: &Opaque<c::bch_fs>) -> c_int {
    let fs = Fs::from_c(c);
    ret_to_c(fs.journal_seq_blacklist_rebuild(&fs.sb_lock()))
}

/// At the end of recovery: drop the ranges nothing was found in that are
/// older than the journal on disk. Whether the section changed. The table
/// isn't rebuilt: it keeps them, harmlessly, until the next mount.
#[c_export]
pub fn bch2_blacklist_entries_gc(c: &Opaque<c::bch_fs>) -> bool {
    let fs = Fs::from_c(c);
    // SAFETY: C calls it under sb_lock, with recovery's last superblock update
    let mut sb = unsafe { fs.sb_lock_held() };
    let Some(f) = sb.field_mut::<Section>() else { return false };
    let bl = &mut f.start;
    let nr = bl.len();

    // SAFETY: under sb_lock, the writers' lock, and nothing's rebuilt here
    let t = unsafe { fs.journal_seq_blacklist.get_locked() };
    assert_eq!(t.len(), nr, "blacklist table out of sync with the superblock");

    let oldest = fs.journal.oldest_seq_found_ondisk;
    let mut new_nr = 0;
    // in order, the section's order
    for (src, i) in Eytzinger0::of(t).iter().enumerate() {
        let e = &t[i];
        assert!(e.start == bl[src].start.get() && e.end == bl[src].end.get(),
                "blacklist table out of sync with the superblock");

        if e.dirty.load(Relaxed) || e.end >= oldest {
            bl[new_nr] = bl[src];
            new_nr += 1;
        }
    }

    if new_nr == nr {
        return false;
    }

    bch_verbose!(fs, "nr blacklist entries was {nr}, now {new_nr}");

    if new_nr == 0 {
        sb.field_delete(c::bch_sb_field_type::BCH_SB_FIELD_journal_seq_blacklist);
    } else {
        let shrunk = sb.field_resize::<Section>(sb_field_u64s::<Section>(new_nr)).is_some();
        assert!(shrunk, "shrinking the blacklist section failed");
    }
    true
}
