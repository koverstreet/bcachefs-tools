//! ptrs.rs's wrappers for C: each a C name over a Rust function, with C's
//! types - the filesystem &Fs where C's pointer is const and &Opaque where it
//! isn't, keys untyped.

use super::*;
use super::ops::{btree_ptr_validate, btree_ptr_v2_validate};
use crate::errcode::ret_to_c;
use crate::init::error::BkeyValidate;
use crate::util::ffi::Opaque;
use crate::util::Printbuf;
use core::ffi::{c_int, c_uint};
use cstruct_macros::c_export;

// ── bkey methods ─────────────────────────────────────────────────────────

#[c_export]
pub fn bch2_bkey_ptrs_validate(c: &Opaque<c::bch_fs>, k: BkeySC<'_>, from: &c::bkey_validate_context)
    -> c_int
{
    ret_to_c(bkey_ptrs_validate(&BkeyValidate { fs: Fs::from_c(c), k, from }))
}

/// For C, which may or may not be in a read-side section: this takes its own.
#[cold]
#[c_export]
pub fn bch2_extent_ptr_to_text(out: &mut Printbuf, c: Option<&Opaque<c::bch_fs>>, ptr: &c::bch_extent_ptr) {
    extent_ptr_to_text(out, c.map(Fs::from_c), ptr, &read_lock());
}

#[cold]
#[c_export]
pub fn bch2_bkey_ptrs_to_text(out: &mut Printbuf, c: Option<&Opaque<c::bch_fs>>, k: BkeySC<'_>) {
    bkey_ptrs_to_text(out, c.map(Fs::from_c), k);
}

#[c_export]
pub fn bch2_btree_ptr_validate(c: &Opaque<c::bch_fs>, k: BkeySC<'_>, from: &c::bkey_validate_context)
    -> c_int
{
    ret_to_c(btree_ptr_validate(&BkeyValidate { fs: Fs::from_c(c), k, from }))
}

#[cold]
#[c_export]
pub fn bch2_btree_ptr_to_text(out: &mut Printbuf, c: &Opaque<c::bch_fs>, k: BkeySC<'_>) {
    bkey_ptrs_to_text(out, Some(Fs::from_c(c)), k);
}

#[c_export]
pub fn bch2_btree_ptr_v2_validate(c: &Opaque<c::bch_fs>, k: BkeySC<'_>, from: &c::bkey_validate_context)
    -> c_int
{
    ret_to_c(btree_ptr_v2_validate(&BkeyValidate { fs: Fs::from_c(c), k, from }))
}

#[cold]
#[c_export]
pub fn bch2_btree_ptr_v2_to_text(out: &mut Printbuf, c: &Opaque<c::bch_fs>, k: BkeySC<'_>) {
    btree_ptr_v2_to_text(out, Fs::from_c(c), k);
}

#[c_export]
pub fn bch2_btree_ptr_v2_compat(btree: c::btree_id, version: c_uint, big_endian: c_uint, write: c_int,
                                k: BkeyS<'_>) {
    btree_ptr_v2_compat(btree, c::bcachefs_metadata_version(version), big_endian != 0, write != 0, k);
}

/// bkey_ops' ->swab takes the filesystem const.
#[c_export]
pub fn bch2_ptr_swab(c: &Fs, k: BkeyS<'_>) {
    ptr_swab(c, k);
}
