// SPDX-License-Identifier: GPL-2.0

//! The superblock's error counts: how often each kind of error -
//! BCH_SB_ERRS(), errors_format.rs - has happened to this filesystem, and when
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

use core::cmp::Reverse;

use cstruct_macros::c_export;

use crate::c;
use crate::errcode::{bch_errcode, BchError};
use crate::fs::Fs;
use crate::prt;
use crate::sb::io::{sb_field_ops, SbFieldOps};
use crate::util::alloc::{flags::GFP_KERNEL, KVVec};
use crate::util::ffi::c_char;
use crate::util::{Printbuf, ToText};

// ── Error ids, and their names ───────────────────────────────────────────────

/// The ids, MAX's included: it's an entry, as C's table has it.
const NR: usize = c::bch_sb_error_id::BCH_FSCK_ERR_MAX.0 as usize + 1;

macro_rules! names {
    ($(($name:ident, $id:literal $(, $($rest:tt)*)?)),* $(,)?) => {
        /// By id, NUL terminated for C - BCH_SB_ERRS() has them in no order.
        /// Evaluated at build time, so an id past MAX, a reused id or a gap
        /// fails the build.
        const NAMES: [&str; NR] = {
            let mut by_id: [Option<&str>; NR] = [None; NR];
            $(
                assert!(by_id[$id].is_none(), concat!("sb error id reused: ", stringify!($name)));
                by_id[$id] = Some(concat!(stringify!($name), "\0"));
            )*

            let mut names = [""; NR];
            let mut i = 0;
            while i < NR {
                names[i] = match by_id[i] {
                    Some(n) => n,
                    None => panic!("sb error ids have a gap"),
                };
                i += 1;
            }
            names
        };
    };
}
c::BCH_SB_ERRS!(names);

#[c_export]
#[c("const char * const bch2_sb_error_strs[]")]
pub static bch2_sb_error_strs: [*const c_char; NR] = {
    let mut strs = [core::ptr::null(); NR];
    let mut i = 0;
    while i < NR {
        strs[i] = NAMES[i].as_ptr().cast();
        i += 1;
    }
    strs
};

impl c::bch_sb_error_id {
    /// Its BCH_SB_ERRS() name: None for one this version doesn't know, as a
    /// newer version may have written.
    pub fn name(self) -> Option<&'static str> {
        let n = NAMES[..NR - 1].get(self.0 as usize)?;
        Some(&n[..n.len() - 1])
    }
}

impl ToText for c::bch_sb_error_id {
    fn to_text(&self, out: &mut Printbuf) {
        match self.name() {
            Some(n) => write!(out, "{n}"),
            None => write!(out, "(unknown error {})", self.0),
        }
    }
}

#[c_export]
pub fn bch2_sb_error_id_to_text(out: &mut Printbuf, id: c::bch_sb_error_id) {
    id.to_text(out)
}

/// A v2 count, which saturates: one that did is a floor, not an exact value.
pub fn prt_error_nr(out: &mut Printbuf, nr: u64) {
    write!(out, "{nr}");
    if nr >= c::BCH_SB_ERROR_ENTRY_V2_NR_MAX {
        write!(out, "+");
    }
}

#[c_export]
pub fn bch2_prt_error_nr(out: &mut Printbuf, nr: u64) {
    prt_error_nr(out, nr)
}

// ── The sections ─────────────────────────────────────────────────────────────

/// An entry of either section, as v2 has it: see the top of the file.
trait ErrorEntry: Copy {
    /// v2's: a first time, and a count that saturates.
    const V2: bool;

    fn id(&self) -> c::bch_sb_error_id;
    fn nr(&self) -> u64;
    fn first(&self) -> u64;
    fn last(&self) -> u64;
}

impl ErrorEntry for c::bch_sb_field_error_entry {
    const V2: bool = false;

    fn id(&self) -> c::bch_sb_error_id {
        c::bch_sb_error_id(self.bch_sb_error_entry_id() as u32)
    }

    fn nr(&self) -> u64 {
        self.bch_sb_error_entry_nr()
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

    fn id(&self) -> c::bch_sb_error_id {
        c::bch_sb_error_id(self.v.id().into())
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

fn invalid() -> Result<(), BchError> {
    Err(bch_errcode::BCH_ERR_invalid_sb_errors.into())
}

fn validate<E: ErrorEntry>(entries: &[E], err: &mut Printbuf) -> Result<(), BchError> {
    for (i, e) in entries.iter().enumerate() {
        if e.nr() == 0 {
            prt!(err, "entry with count 0 (id {})", e.id());
            return invalid();
        }

        // never for v1, whose first is its last
        if e.first() > e.last() {
            prt!(err, "entry with first occurrence after last (id {})", e.id());
            return invalid();
        }

        if entries.get(i + 1).is_some_and(|n| e.id().0 >= n.id().0) {
            write!(err, "entries out of order");
            return invalid();
        }
    }
    Ok(())
}

/// Newest first. If there's no memory to sort them in, in id order.
fn to_text<E: ErrorEntry>(out: &mut Printbuf, entries: &[E]) {
    if out.nr_tabstops() <= 1 {
        out.tabstop_push(16);
    }

    let mut sorted = KVVec::new();
    let entries = match sorted.extend_from_slice(entries, GFP_KERNEL) {
        Ok(()) => {
            sorted.sort_unstable_by_key(|e: &E| Reverse(e.last()));
            &sorted[..]
        }
        Err(_) => entries,
    };

    for e in entries {
        prt!(out, "{}\t", e.id());
        if E::V2 {
            prt_error_nr(out, e.nr());
            out.tab();
            out.datetime(e.first() as i64);
        } else {
            write!(out, "{}", e.nr());
        }
        out.tab();
        out.datetime(e.last() as i64);
        out.newline();
    }
}

impl SbFieldOps for c::bch_sb_field_errors {
    fn validate(_sb: &c::bch_sb, f: &Self, _flags: c::bch_validate_flags,
                err: &mut Printbuf) -> Result<(), BchError> {
        validate(&f.entries, err)
    }

    fn to_text(out: &mut Printbuf, _fs: Option<&Fs>, _sb: &c::bch_sb, f: &Self) {
        to_text(out, &f.entries)
    }
}

impl SbFieldOps for c::bch_sb_field_errors_v2 {
    fn validate(_sb: &c::bch_sb, f: &Self, _flags: c::bch_validate_flags,
                err: &mut Printbuf) -> Result<(), BchError> {
        validate(&f.entries, err)
    }

    fn to_text(out: &mut Printbuf, _fs: Option<&Fs>, _sb: &c::bch_sb, f: &Self) {
        to_text(out, &f.entries)
    }
}

#[c_export]
pub static bch_sb_field_ops_errors: c::bch_sb_field_ops = sb_field_ops::<c::bch_sb_field_errors>();

#[c_export]
pub static bch_sb_field_ops_errors_v2: c::bch_sb_field_ops = sb_field_ops::<c::bch_sb_field_errors_v2>();
