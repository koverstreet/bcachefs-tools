// SPDX-License-Identifier: GPL-2.0

//! BCH_SB_FIELD_clean: btree roots, and a few other things, are recovered
//! from the journal after an unclean shutdown - but after a clean shutdown, to
//! avoid having to read the journal, we can store them in the superblock.
//!
//! The section is a list of journal entries, stored exactly as they would be
//! in the journal, after a header saying which journal seq they're as of.

use core::mem::size_of;

use cstruct_macros::c_export;

use crate::c;
use crate::fs::Fs;
use crate::sb::io::{entries_end, sb_field_ops, SbFieldOps};
use crate::util::vstructs::{Head, Validated};
use crate::util::Printbuf;

type Section = c::bch_sb_field_clean;

const SECTION_HEAD: usize = size_of::<Head<Section>>();

/// Nothing of its own to validate: its header whole, and an entry overrunning
/// its end, are sb_field_validate()'s, and the entries themselves are
/// validated late, with the filesystem - bch2_sb_clean_validate_late().
impl SbFieldOps for Section {
    fn to_text(out: &mut Printbuf, fs: Option<&Fs>, _sb: &c::bch_sb, f: &Self) {
        write!(out, "flags:          {:x}\n", f.flags.get());
        write!(out, "journal_seq:    {}\n", f.journal_seq.get());

        let fs = fs.map_or(core::ptr::null_mut(), Fs::as_ptr);
        for e in f.entries() {
            if e.type_ == c::bch_jset_entry_type::BCH_JSET_ENTRY_btree_keys.0 as u8 && e.u64s() == 0 {
                continue;
            }
            // SAFETY: a whole entry; C takes NULL for no filesystem
            unsafe { c::bch2_journal_entry_to_text(out.as_raw(), fs, e.as_ffi().cast_mut()) };
            out.newline();
        }
    }

    fn entries_end(b: &[u8]) -> Option<usize> {
        entries_end::<c::jset_entry<Validated>>(b, SECTION_HEAD, |_| true)
    }
}

#[c_export]
pub static bch_sb_field_ops_clean: c::bch_sb_field_ops = sb_field_ops::<Section>();
