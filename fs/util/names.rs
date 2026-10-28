// SPDX-License-Identifier: GPL-2.0
//! What an enum built from an x-macro list is named: Names, which c_enum!
//! implements for each.
//!
//! A static description, not a visitor: ENTRIES is each variant's name and
//! value - a flags type's, its bit - and a value with none is "(unknown 7)".
//! Free to make, as it's only data; a dense table by
//! value, as C's string tables are, is names_by_value!(T), made where it's
//! wanted.
//!
//! What prints them is generic code over that, not code per enum: c_enum!'s
//! ToText for an open enum is a call to prt_name(), one copy, and #[inline] -
//! an enum nothing prints costs nothing.

use crate::util::ffi::c_char;
use crate::util::Printbuf;

pub trait Names {
    /// Each labelled variant's name and value - a flags type's, its bit - in
    /// the list's order.
    const ENTRIES: &'static [(&'static str, u64)];
}

/// @v's name in @entries.
pub fn name_of(entries: &[(&'static str, u64)], v: u64) -> Option<&'static str> {
    entries.iter().find(|&&(_, e)| e == v).map(|&(n, _)| n)
}

/// @v's name in @entries, or "(unknown @v)".
pub fn prt_name(out: &mut Printbuf, entries: &[(&'static str, u64)], v: u64) {
    match name_of(entries, v) {
        Some(name) => write!(out, "{name}"),
        None       => write!(out, "(unknown {v})"),
    };
}

/// The names of the bits set in @v, comma separated.
pub fn prt_flags(out: &mut Printbuf, entries: &[(&'static str, u64)], v: u64) {
    let set = entries.iter().filter(|&&(_, bit)| bit < 64 && v & 1 << bit != 0);
    for (i, (name, _)) in set.enumerate() {
        if i != 0 {
            write!(out, ",");
        }
        write!(out, "{name}");
    }
}

/// The names of @T's values, indexed by value - "" for a value none has: a
/// table where one's wanted, for an enum whose values are dense - an option's
/// choices, C's string tables. Built at compile time: two names for one value
/// fail the build.
#[macro_export]
macro_rules! names_by_value {
    ($t:ty) => {{
        const N: usize = $crate::util::names::table_len(<$t as $crate::util::names::Names>::ENTRIES);
        const T: [&str; N] = $crate::util::names::by_value(<$t as $crate::util::names::Names>::ENTRIES);
        &T
    }};
}

/// The length a table of @entries needs: the largest value, plus one.
pub const fn table_len(entries: &[(&'static str, u64)]) -> usize {
    let mut len = 0;
    let mut i = 0;
    while i < entries.len() {
        if entries[i].1 as usize + 1 > len {
            len = entries[i].1 as usize + 1;
        }
        i += 1;
    }
    len
}

/// @entries' names, each at its value.
pub const fn by_value<const N: usize>(entries: &[(&'static str, u64)]) -> [&'static str; N] {
    let mut names = [""; N];
    let mut i = 0;
    while i < entries.len() {
        let (name, v) = entries[i];
        assert!(names[v as usize].is_empty(), "two names for one value");
        names[v as usize] = name;
        i += 1;
    }
    names
}

/// A flags type's bit, for @mask: a flag is one bit.
pub const fn bit(mask: u64) -> u64 {
    assert!(mask.is_power_of_two(), "a flag that isn't one bit");
    mask.trailing_zeros() as u64
}

/// The values set in @mask - a bit each - by name, comma separated: a set of
/// @T, a btree id mask's ids.
#[inline]
pub fn prt_set<T: Names>(out: &mut Printbuf, mask: u64) {
    prt_flags(out, T::ENTRIES, mask)
}

// ── C's string tables ────────────────────────────────────────────────────

/// The length of @T's C table: its values, and the NULL after.
pub const fn c_table_len<T: Names>() -> usize {
    table_len(T::ENTRIES) + 1
}

/// @T's names as C's string tables have them - by value, each NUL
/// terminated, NULL for a value none has, and NULL after the last - for a
/// static of [*const c_char; c_table_len::<T>()]. Built at compile time: the
/// names, NUL terminated, in a static of their own, and the table into it.
#[macro_export]
macro_rules! c_names_table {
    ($t:ty) => {{
        const E: &[(&str, u64)] = <$t as $crate::util::names::Names>::ENTRIES;
        static BUF: [u8; $crate::util::names::nul_len(E)] = $crate::util::names::nul_names(E);
        $crate::util::names::c_table(&BUF, E)
    }};
}

/// The length of @entries' names, each NUL terminated.
pub const fn nul_len(entries: &[(&str, u64)]) -> usize {
    let mut len = 0;
    let mut i = 0;
    while i < entries.len() {
        len += entries[i].0.len() + 1;
        i += 1;
    }
    len
}

/// @entries' names, each NUL terminated, in order.
pub const fn nul_names<const B: usize>(entries: &[(&str, u64)]) -> [u8; B] {
    let mut buf = [0; B];
    let (mut i, mut at) = (0, 0);
    while i < entries.len() {
        let name = entries[i].0.as_bytes();
        let mut j = 0;
        while j < name.len() {
            buf[at + j] = name[j];
            j += 1;
        }
        at += name.len() + 1;
        i += 1;
    }
    buf
}

/// C's table of @entries' names, nul_names()'s @buf: by value, NULL for a
/// value none has, and NULL after the last.
pub const fn c_table<const N: usize>(buf: &'static [u8], entries: &[(&str, u64)]) -> [*const c_char; N] {
    let mut t = [core::ptr::null(); N];
    let (mut i, mut at) = (0, 0);
    while i < entries.len() {
        t[entries[i].1 as usize] = buf.as_ptr().wrapping_add(at).cast();
        at += entries[i].0.len() + 1;
        i += 1;
    }
    t
}
