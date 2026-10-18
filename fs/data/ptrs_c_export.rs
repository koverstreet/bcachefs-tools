//! ptrs.rs's wrappers for C: each a C name over a Rust function, with C's
//! types - the filesystem &Fs where C's pointer is const and &Opaque where it
//! isn't, keys untyped, a key without pointers answered as an empty list of
//! them.

use super::*;
use super::ops::{btree_ptr_validate, btree_ptr_v2_validate};
use crate::btree::bkey::BkeyI;
use crate::btree::iter::{BtreeTrans, TransBkey};
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

// ── reading ──────────────────────────────────────────────────────────────

#[cold]
#[c_export]
pub fn bch2_io_failures_to_text(out: &mut Printbuf, c: &Opaque<c::bch_fs>, failed: &c::bch_io_failures) {
    failed.to_text(out, Fs::from_c(c));
}

#[c_export]
pub fn bch2_io_failures_all_dev_removed(failed: &c::bch_io_failures) -> bool {
    failed.all_dev_removed()
}

/// Mutable, as C's: a caller may update the record.
#[c_export]
pub fn bch2_dev_io_failures(failed: &mut c::bch_io_failures, dev: c_uint) -> *mut c::bch_dev_io_failures {
    let nr = usize::from(failed.nr);
    failed.data[..nr].iter_mut().find(|f| u32::from(f.dev) == dev).map_or(core::ptr::null_mut(), |f| f)
}

#[c_export]
pub fn bch2_dev_io_failures_mut(failed: &mut c::bch_io_failures, dev: c_uint) -> *mut c::bch_dev_io_failures {
    failed.dev_mut(dev)
}

#[c_export]
pub fn bch2_mark_io_failure(failed: &mut c::bch_io_failures, p: &c::extent_ptr_decoded, err: c_int) {
    failed.mark(p, BchError::from_raw(-err));
}

/// 1 and @pick written, 0 for nothing to read, or an error. @failed may be
/// NULL, and @pick is written, not read.
#[c_export]
pub fn bch2_bkey_pick_read_device(c: &Opaque<c::bch_fs>, k: BkeySC<'_>, failed: Option<&c::bch_io_failures>,
                                  pick: &mut MaybeUninit<c::extent_ptr_decoded>, preferred_dev: c_uint,
                                  flags: c_uint) -> c_int {
    match k.pick_read_device(Fs::from_c(c), failed, preferred_dev, c::bch_read_flags::from_bits_retain(flags)) {
        Ok(Some(p)) => {
            pick.write(p);
            1
        }
        Ok(None) => 0,
        Err(e) => -e.raw(),
    }
}

// ── editing ──────────────────────────────────────────────────────────────

/// Where @p, a pointer C has into one of @k's entries, is: its EntryPos.
/// Panics if it isn't in the value, or isn't u64 aligned there.
fn entry_pos_of(k: BkeySC<'_>, p: *const u8) -> EntryPos {
    let val = k.val_bytes();
    let off = (p as usize).wrapping_sub(val.as_ptr() as usize);
    assert!(off < val.len() && off % size_of::<u64>() == 0,
            "an entry pointer {off} bytes into a value of {}", val.len());
    EntryPos(off / size_of::<u64>())
}

/// A key C hands as a struct bkey_i *.
///
/// # Safety
/// @k is a key, all of it, the caller's to edit for 'a.
unsafe fn bkey_i_s<'a>(k: *mut c::bkey_i) -> BkeyS<'a> {
    // SAFETY: the caller's
    BkeyS::from(unsafe { &mut *BkeyI::from_raw_mut(k) })
}

fn ptrs_mut(k: BkeyS<'_>) -> BkeyS<'_, PtrsMut> {
    k.arm::<PtrsMut>().expect("editing the entries of a key whose pointers can't be")
}

fn ptrs(k: BkeyS<'_>) -> BkeyS<'_, Ptrs> {
    k.arm::<Ptrs>().expect("dropping pointers of a key without them")
}

/// @entry is one of @k's entries: only its address is used.
#[c_export]
pub fn bch2_bkey_extent_entry_drop_s(c: &Fs, k: BkeyS<'_>, entry: *mut c::bch_extent_entry) {
    let at = entry_pos_of(k.as_sc(), entry.cast());
    ptrs_mut(k).drop_entry(c, at);
}

/// @ptr is one of @k's pointers: only its address is used.
#[c_export]
pub fn bch2_bkey_drop_ptr_noerror(c: &Fs, k: BkeyS<'_>, ptr: *mut c::bch_extent_ptr) {
    let at = entry_pos_of(k.as_sc(), ptr.cast());
    ptrs(k).drop_ptr_noerror(c, at);
}

/// @ptr is one of @k's pointers: only its address is used.
#[c_export]
pub fn bch2_bkey_drop_ptr(c: &Fs, k: BkeyS<'_>, ptr: *mut c::bch_extent_ptr) {
    let at = entry_pos_of(k.as_sc(), ptr.cast());
    ptrs(k).drop_ptr(c, at);
}

/// # Safety
/// @k is a key the caller can edit.
#[c_export]
pub unsafe fn bch2_bkey_drop_ptrs_mask(c: &Fs, k: *mut c::bkey_i, mask: c_uint) {
    // SAFETY: the caller's
    ptrs(unsafe { bkey_i_s(k) }).drop_ptrs_mask(c, mask);
}

#[c_export]
pub fn bch2_bkey_drop_device_noerror(c: &Fs, k: BkeyS<'_>, dev: c_uint) {
    ptrs(k).drop_device_noerror(c, dev);
}

#[c_export]
pub fn bch2_bkey_drop_device(c: &Fs, k: BkeyS<'_>, dev: c_uint) {
    ptrs(k).drop_device(c, dev);
}

/// # Safety
/// @k is a key the caller can edit.
#[c_export]
pub unsafe fn bch2_bkey_drop_ec(c: &Fs, k: *mut c::bkey_i, dev: c_uint) {
    // SAFETY: the caller's
    ptrs_mut(unsafe { bkey_i_s(k) }).drop_ec(c, dev);
}

/// # Safety
/// @k is a key the caller can edit.
#[c_export]
pub unsafe fn bch2_bkey_drop_ec_mask(c: &Fs, k: *mut c::bkey_i, mask: c_uint) {
    // SAFETY: the caller's
    ptrs_mut(unsafe { bkey_i_s(k) }).drop_ec_mask(c, mask);
}

#[c_export]
pub fn bch2_bkey_drop_extra_cached_ptrs(c: &Fs, opts: &c::bch_inode_opts, k: BkeyS<'_>) {
    if let Some(mut k) = k.arm::<Ptrs>() {
        k.drop_extra_cached_ptrs(c, opts);
    }
}

/// # Safety
/// C has an attempt in progress on @trans.
#[c_export]
pub unsafe fn bch2_bkey_drop_stale_ptrs(trans: &BtreeTrans<'_>, iter: &BtreeIter<'_>, k: BkeySC<'_>) -> c_int {
    // SAFETY: the caller's
    let t = unsafe { trans.attempt_in_progress() };
    ret_to_c(drop_stale_ptrs(&t, iter, k))
}

/// @k is in @trans's memory, @buf_u64s long: room for a flags entry, if it
/// has none, that the caller allocates.
///
/// # Safety
/// C has an attempt in progress on @trans, and @k is a key in its memory,
/// @buf_u64s u64s of it its.
#[c_export]
pub unsafe fn bch2_bkey_extent_flags_set(trans: &BtreeTrans<'_>, k: *mut c::bkey_i, buf_u64s: c_uint,
                                         flags: u64) -> c_int {
    // SAFETY: the caller's
    let t = unsafe { trans.attempt_in_progress() };
    let fs = t.fs();

    ret_to_c((|| -> Result<(), BchError> {
        fs.request_incompat_feature(c::bcachefs_metadata_version::extent_flags)?;

        // SAFETY: the caller's
        let k = unsafe { TransBkey::from_raw_room(&t, k, buf_u64s) };
        k.arm::<PtrsMut>().expect("extent flags on a key whose entries can't be edited")
            .extent_flags_set(fs, flags).expect("room for an extent flags entry: its caller allocates it");
        Ok(())
    })())
}

/// # Safety
/// @dst is a key the caller can edit.
#[c_export]
pub unsafe fn bch2_bkey_propagate_incompressible(c: &Fs, dst: *mut c::bkey_i, src: BkeySC<'_>) {
    // SAFETY: the caller's
    if let Some(mut dst) = unsafe { bkey_i_s(dst) }.arm::<Ptrs>() {
        dst.propagate_incompressible(c, src);
    }
}
