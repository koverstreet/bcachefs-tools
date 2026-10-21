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

use core::cmp::{Ordering, Reverse};
use core::ffi::c_int;

use cstruct_macros::c_export;

use crate::c;
use crate::util::vstructs::Unvalidated;
use crate::errcode::{bch_errcode, ret_to_c, BchError};
use crate::fs::Fs;
use crate::prt;
use crate::sb::io::{sb_field_u64s, SbFieldOps};
use crate::util::alloc::{flags::GFP_KERNEL, kvvec_insert, KVVec};
use crate::util::ffi::{c_char, Opaque};
use crate::util::kernel::ktime_get_real_seconds;
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

/// @ids by name, comma separated.
pub fn prt_error_ids(out: &mut Printbuf, ids: impl IntoIterator<Item = c::bch_sb_error_id>) {
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
    fn validate(_sb: &c::bch_sb<Unvalidated>, f: &c::bch_sb_field_errors<Unvalidated>,
                _flags: c::bch_validate_flags, err: &mut Printbuf) -> Result<(), BchError> {
        validate(&f.entries, err)
    }

    fn to_text(out: &mut Printbuf, _fs: Option<&Fs>, _sb: &c::bch_sb, f: &Self) {
        to_text(out, &f.entries)
    }
}

impl SbFieldOps for c::bch_sb_field_errors_v2 {
    fn validate(_sb: &c::bch_sb<Unvalidated>, f: &c::bch_sb_field_errors_v2<Unvalidated>,
                _flags: c::bch_validate_flags, err: &mut Printbuf) -> Result<(), BchError> {
        validate(&f.entries, err)
    }

    fn to_text(out: &mut Printbuf, _fs: Option<&Fs>, _sb: &c::bch_sb, f: &Self) {
        to_text(out, &f.entries)
    }
}

// ── Which error an errcode is counted as ─────────────────────────────────────
//
// Each is passed a failure, negative, and matches on its magnitude.

/// @err a BCH_ERR_BLK_STS_* errcode, as blk_status_to_bch_err() throws: its
/// status's error, blk_sts_unknown for one BCH_BLK_STS_SB_ERRS() leaves out.
#[c_export]
pub fn bch2_blk_sts_sb_err(err: c_int) -> c::bch_sb_error_id {
    macro_rules! map {
        ($(($sts:ident, $name:ident)),* $(,)?) => { ::paste::paste! {
            match bch_errcode(err.unsigned_abs()) {
                $(bch_errcode::[<BCH_ERR_BLK_STS_ $sts>] => c::bch_sb_error_id::[<BCH_FSCK_ERR_blk_sts_ $name>],)*
                _ => c::bch_sb_error_id::BCH_FSCK_ERR_blk_sts_unknown,
            }
        } };
    }
    c::BCH_BLK_STS_SB_ERRS!(map)
}

/// @err's variant of $ty: $prefix and its BCH_DECOMPRESS_SB_ERRS() name -
/// zstd_unknown for a zstd error the list leaves out, unknown for the rest.
macro_rules! decompress_map {
    ([$err:expr, $ty:ident, $prefix:ident] $(($code:ident, $name:ident)),* $(,)?) => { ::paste::paste! {
        match bch_errcode($err.unsigned_abs()) {
            $(bch_errcode::[<BCH_ERR_ $code>] => c::$ty::[<$prefix $name>],)*
            code if BchError::from(code).matches(bch_errcode::BCH_ERR_zstd_error) =>
                c::$ty::[<$prefix zstd_unknown>],
            _ => c::$ty::[<$prefix unknown>],
        }
    } };
}

/// @err a BCH_ERR_decompress errcode, as buf_uncompress() throws: what it's
/// counted as.
#[c_export]
pub fn bch2_decompress_sb_err(err: c_int) -> c::bch_sb_error_id {
    c::BCH_DECOMPRESS_SB_ERRS!(decompress_map [err, bch_sb_error_id, BCH_FSCK_ERR_data_decompress_err_])
}

/// The same, as the reason stamped into the KEY_TYPE_error key when it's
/// what we finally gave up on: the count says how often decompression failed
/// this way, the key which extent it cost us.
#[c_export]
pub fn bch2_decompress_key_type_error(err: c_int) -> c::bch_key_type_errors {
    c::BCH_DECOMPRESS_SB_ERRS!(decompress_map [err, bch_key_type_errors, KEY_TYPE_ERROR_decompress_])
}

// ── The counts, in memory ────────────────────────────────────────────────────
//
// c->errors.counts, sorted by id: read in from the superblock at mount, counted
// in as errors happen, and written out with every superblock write. Lock
// order: sb_lock, then counts - bch2_sb_errors_from_cpu() takes counts under
// bch2_write_super()'s sb_lock.

/// The largest in-memory count: nr is 48 bits.
const CPU_NR_MAX: u64 = (1 << 48) - 1;

fn cpu_entry(id: c::bch_sb_error_id, nr: u64, first: u64, last: u64) -> c::bch_sb_error_entry_cpu {
    let mut e = c::bch_sb_error_entry_cpu {
        first_error_time: first,
        last_error_time:  last,
        ..Default::default()
    };
    e.set_id(id.0 as u16);
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
fn merge(a: c::bch_sb_error_entry_cpu, b: c::bch_sb_error_entry_cpu) -> c::bch_sb_error_entry_cpu {
    cpu_entry(c::bch_sb_error_id(a.id().into()), a.nr().saturating_add(b.nr()),
              a.first_error_time.min(b.first_error_time),
              a.last_error_time.max(b.last_error_time))
}

impl Fs {
    /// Count @err, as having happened now. With no memory for a new entry,
    /// it isn't counted.
    pub fn sb_error_count(&self, err: c::bch_sb_error_id) {
        let now = ktime_get_real_seconds() as u64;
        let mut counts = self.errors.counts.lock();

        match counts.binary_search_by_key(&err.0, |e| e.id().into()) {
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

    /// The superblock's counts into memory, at mount. Both sections present
    /// means the filesystem was downgraded: writing v2 always deletes v1 in
    /// the same superblock write, so a v1 alongside v2 was written by an old
    /// kernel since, and holds exactly the counts accrued since. They're
    /// merged - nothing lost or counted twice across a downgrade and upgrade
    /// - and the next superblock write folds everything back into v2.
    pub fn sb_errors_to_cpu(&self) -> Result<(), BchError> {
        let sb = self.sb_lock();
        let v2: &[c::bch_sb_field_error_entry_v2] = sb.field::<c::bch_sb_field_errors_v2>().map_or(&[], |f| &f.entries);
        let v1: &[c::bch_sb_field_error_entry]    = sb.field::<c::bch_sb_field_errors>().map_or(&[], |f| &f.entries);

        let mut counts = self.errors.counts.lock();
        counts.clear();
        counts.reserve(v2.len() + v1.len(), GFP_KERNEL)?;

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
            counts.push(e, GFP_KERNEL)?;
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
            prt!(out, "{}\t{}\t", c::bch_sb_error_id(e.id().into()), e.nr());
            out.datetime(e.first_error_time as i64);
            out.tab();
            out.datetime(e.last_error_time as i64);
            out.newline();
        }
    }
}

#[c_export]
pub fn bch2_sb_error_count(c: &Opaque<c::bch_fs>, err: c::bch_sb_error_id) {
    Fs::from_c(c).sb_error_count(err)
}

#[c_export]
pub fn bch2_sb_errors_to_cpu(c: &Opaque<c::bch_fs>) -> c_int {
    ret_to_c(Fs::from_c(c).sb_errors_to_cpu())
}

/// For a superblock write: the counts in memory into v2, and v1 deleted -
/// to_cpu() merged it in. If v2 can't be grown, the superblock keeps what it
/// had until the next write.
#[c_export]
pub fn bch2_sb_errors_from_cpu(c: &Opaque<c::bch_fs>) {
    let fs = Fs::from_c(c);
    // SAFETY: C calls it under sb_lock, from bch2_write_super()
    let mut sb = unsafe { fs.sb_lock_held() };
    let counts = fs.errors.counts.lock();

    let u64s = sb_field_u64s::<c::bch_sb_field_errors_v2>(counts.len());
    let Some(f) = sb.field_resize::<c::bch_sb_field_errors_v2>(u64s) else { return };

    // every field of each entry is set: what the resize left there doesn't matter
    for (d, s) in f.entries.iter_mut().zip(counts.iter()) {
        d.v.set_id(s.id());
        d.v.set_nr(s.nr().min(c::BCH_SB_ERROR_ENTRY_V2_NR_MAX) as u32);
        d.v.set_first_error_time(s.first_error_time.min(c::BCH_SB_ERROR_ENTRY_V2_TIME_MAX));
        d.v.set_last_error_time(s.last_error_time.min(c::BCH_SB_ERROR_ENTRY_V2_TIME_MAX));
    }

    sb.field_delete(c::bch_sb_field_type::BCH_SB_FIELD_errors);
}
