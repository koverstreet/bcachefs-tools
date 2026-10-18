//! ptrs.rs's wrappers for C: each a C name over a Rust function, with C's
//! types - the filesystem &Fs where C's pointer is const and &Opaque where it
//! isn't, keys untyped, a key without pointers answered as an empty list of
//! them.

use super::*;
use super::ops::{btree_ptr_validate, btree_ptr_v2_validate};
use crate::errcode::ret_to_c;
use crate::init::error::BkeyValidate;
use crate::util::ffi::Opaque;
use crate::util::Printbuf;
use core::ffi::{c_int, c_uint};
use core::mem::MaybeUninit;
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

// ── queries ──────────────────────────────────────────────────────────────

#[c_export]
pub fn bch2_bkey_has_device_c(c: &Fs, k: BkeySC<'_>, dev: c_uint) -> *const c::bch_extent_ptr {
    k.arm::<Ptrs>()
        .and_then(|k| k.has_device(c, dev).map(|p| p as *const _))
        .unwrap_or(core::ptr::null())
}

/// @ret is written, not read: C's callers leave it uninitialized.
#[c_export]
pub fn bch2_bkey_has_device_decode(c: &Fs, k: BkeySC<'_>, dev: c_uint,
                                   ret: &mut MaybeUninit<c::extent_ptr_decoded>) -> bool {
    let Some(k) = k.arm::<Ptrs>() else { return false };
    let Some(d) = k.has_device_decode(c, dev) else { return false };
    ret.write(d.p);
    true
}

#[c_export]
pub fn bch2_bkey_devs_rw(c: &Opaque<c::bch_fs>, k: BkeySC<'_>) -> bool {
    k.arm::<Ptrs>().is_none_or(|k| k.devs_rw(Fs::from_c(c)))
}

#[c_export]
pub fn bch2_bkey_has_target(c: &Opaque<c::bch_fs>, k: BkeySC<'_>, target: c_uint) -> bool {
    k.arm::<Ptrs>().is_some_and(|k| k.has_target(Fs::from_c(c), Target::decode(target)))
}

#[c_export]
pub fn bch2_bkey_in_target(c: &Opaque<c::bch_fs>, k: BkeySC<'_>, target: c_uint) -> bool {
    k.arm::<Ptrs>().is_none_or(|k| k.in_target(Fs::from_c(c), Target::decode(target)))
}

#[c_export]
pub fn bch2_bkey_matches_ptr(c: &Opaque<c::bch_fs>, k: BkeySC<'_>, m: c::bch_extent_ptr, offset: u64) -> bool {
    k.arm::<Ptrs>().is_some_and(|k| k.matches_ptr(Fs::from_c(c), &m, offset))
}

#[c_export]
pub fn bch2_bkey_ptrs_match(k1: BkeySC<'_>, p1: c::extent_ptr_decoded,
                            k2: BkeySC<'_>, p2: c::extent_ptr_decoded) -> bool {
    ptrs_match(k1.k, &p1, k2.k, &p2)
}

#[c_export]
pub fn bch2_extents_match(c: &Fs, k1: BkeySC<'_>, k2: BkeySC<'_>) -> bool {
    extents_match(c, k1, k2)
}
