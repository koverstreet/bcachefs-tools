// SPDX-License-Identifier: GPL-2.0

//! The superblock's downgrade section: for each version a downgrade goes back
//! past, the recovery passes to run and the errors to fix silently.
//!
//! It's written by the version that knows what downgrading from it costs, for
//! an older one to read - bch2_sb_set_downgrade(), when an older version
//! mounts a newer filesystem.

use core::mem::size_of;

use cstruct_macros::c_export;

use crate::c;
use crate::errcode::{bch_errcode, BchError};
use crate::fs::Fs;
use crate::init::passes;
use crate::prt;
use crate::sb::errors::prt_error_ids;
use crate::sb::io::{entries_end, sb_field_ops, SbFieldOps};
use crate::util::vstructs::{Head, Validated, ValidatedEntries};
use crate::util::Printbuf;

type Section = c::bch_sb_field_downgrade;
type Entry = c::bch_sb_field_downgrade_entry<Validated>;

const SECTION_HEAD: usize = size_of::<Head<Section>>();

/// Its whole entries. Each is 2 byte aligned and the section's length a
/// multiple of 8, so up to 6 bytes past the last are padding.
fn entries(f: &Section) -> ValidatedEntries<'_, Entry> {
    ValidatedEntries::new(&f.as_bytes()[SECTION_HEAD..])
}

impl SbFieldOps for Section {
    fn validate(sb: &c::bch_sb, f: &Self, _flags: c::bch_validate_flags,
                err: &mut Printbuf) -> Result<(), BchError> {
        let sb_major = c::BCH_VERSION_MAJOR(sb.version.get().into());
        for e in entries(f) {
            let major = c::BCH_VERSION_MAJOR(e.version.get().into());
            if major != sb_major {
                write!(err, "downgrade entry with mismatched major version ({major} != {sb_major})");
                return Err(bch_errcode::BCH_ERR_invalid_sb_downgrade.into());
            }
        }
        Ok(())
    }

    fn to_text(out: &mut Printbuf, _fs: Option<&Fs>, _sb: &c::bch_sb, f: &Self) {
        if out.nr_tabstops() <= 1 {
            out.tabstop_push(16);
        }

        for e in entries(f) {
            prt!(out, "version:\t{}\n", c::bcachefs_metadata_version(e.version.get().into()));

            write!(out, "recovery passes:\t");
            passes::prt_stable(out, e.recovery_passes[0].get());
            out.newline();

            write!(out, "errors:\t");
            prt_error_ids(out, e.errors.iter().map(|id| c::bch_sb_error_id(id.get().into())));
            out.newline();
        }
    }

    fn entries_end(b: &[u8]) -> Option<usize> {
        entries_end::<Entry>(b, SECTION_HEAD, |_| true)
    }
}

#[c_export]
pub static bch_sb_field_ops_downgrade: c::bch_sb_field_ops = sb_field_ops::<Section>();
