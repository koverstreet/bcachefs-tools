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

use cstruct_macros::c_export;

use crate::c;
use crate::errcode::{bch_errcode, BchError};
use crate::fs::Fs;
use crate::sb::io::{sb_field_ops, SbFieldOps};
use crate::util::Printbuf;

type Section = c::bch_sb_field_journal_seq_blacklist;

impl SbFieldOps for Section {
    fn validate(_sb: &c::bch_sb, f: &Self, _flags: c::bch_validate_flags,
                err: &mut Printbuf) -> Result<(), BchError> {
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

    fn to_text(out: &mut Printbuf, _fs: Option<&Fs>, _sb: &c::bch_sb, f: &Self) {
        for (i, e) in f.start.iter().enumerate() {
            if i != 0 {
                write!(out, " ");
            }
            write!(out, "{}-{}", e.start.get(), e.end.get());
        }
        out.newline();
    }
}

#[c_export]
pub static bch_sb_field_ops_journal_seq_blacklist: c::bch_sb_field_ops = sb_field_ops::<Section>();
