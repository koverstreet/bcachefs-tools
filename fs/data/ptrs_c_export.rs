//! ptrs.rs's wrappers for C: each a C name over a Rust function, with C's
//! types - the filesystem &Fs where C's pointer is const and &Opaque where it
//! isn't, keys untyped, a key without pointers answered as an empty list of
//! them.

use crate::format::MetadataVersion;
use crate::fs::Fs;
use super::*;
use crate::btree::iter::{BtreeTrans, TransBkey};
use crate::errcode::ret_to_c;
use crate::util::ffi::Opaque;
use crate::util::Printbuf;
use core::ffi::{c_int, c_uint};
use core::mem::MaybeUninit;
use cstruct_macros::c_export;

// ── queries ──────────────────────────────────────────────────────────────

#[c_export]
pub fn bch2_bkey_has_device_c(c: &Fs, k: BkeySC<'_>, dev: c_uint) -> *const c::bch_extent_ptr {
    k.arm::<Ptrs>()
        .and_then(|k| k.has_device(c, dev).map(|p| p as *const _))
        .unwrap_or(core::ptr::null())
}

#[c_export]
pub fn bch2_bkey_devs_rw(c: &Opaque<Fs>, k: BkeySC<'_>) -> bool {
    k.arm::<Ptrs>().is_none_or(|k| k.devs_rw(Fs::from_c(c)))
}

#[c_export]
pub fn bch2_bkey_in_target(c: &Opaque<Fs>, k: BkeySC<'_>, target: c_uint) -> bool {
    k.arm::<Ptrs>().is_none_or(|k| k.in_target(Fs::from_c(c), Target::decode(target)))
}

#[c_export]
pub fn bch2_bkey_matches_ptr(c: &Opaque<Fs>, k: BkeySC<'_>, m: c::bch_extent_ptr, offset: u64) -> bool {
    k.arm::<Ptrs>().is_some_and(|k| k.matches_ptr(Fs::from_c(c), &m, offset))
}


// ── reading ──────────────────────────────────────────────────────────────

#[c_export]
pub fn bch2_io_failures_init() -> c::bch_io_failures {
    Default::default()
}

#[c_export]
pub fn bch2_io_failures_exit(failed: &mut c::bch_io_failures) {
    failed.ec_msg = Printbuf::new();
}

#[cold]
#[c_export]
pub fn bch2_io_failures_to_text(out: &mut Printbuf, c: &Opaque<Fs>, failed: &c::bch_io_failures) {
    failed.to_text(out, Fs::from_c(c));
}

#[c_export]
pub fn bch2_io_failures_all_dev_removed(failed: &c::bch_io_failures) -> bool {
    failed.all_dev_removed()
}

#[c_export]
pub fn bch2_io_failures_empty(failed: &c::bch_io_failures) -> bool {
    failed.is_empty()
}

#[c_export]
pub fn bch2_io_failures_reset_devs(failed: &mut c::bch_io_failures) {
    failed.reset_devs()
}

#[c_export]
pub fn bch2_io_failures_has_dev(failed: &c::bch_io_failures, dev: c_uint) -> bool {
    failed.dev(dev).is_some()
}

/// @dev's io error, 0 for none.
#[c_export]
pub fn bch2_io_failures_dev_errcode(failed: &c::bch_io_failures, dev: c_uint) -> c_int {
    failed.dev(dev).map_or(0, |f| f.errcode.into())
}

#[c_export]
pub fn bch2_io_failures_set_errcode(failed: &mut c::bch_io_failures, dev: c_uint, err: c_int) {
    failed.set_errcode(dev, err);
}

/// The erasure coded reconstruct's message, for the read path to write.
#[c_export]
pub fn bch2_io_failures_ec_msg(failed: &mut c::bch_io_failures) -> *mut Printbuf {
    &mut failed.ec_msg
}

#[c_export]
pub fn bch2_io_failures_to_err_mask(failed: &c::bch_io_failures) -> u32 {
    failed.err_mask()
}

#[c_export]
pub fn bch2_io_failures_count_blk_errors(c: &Opaque<Fs>, failed: &c::bch_io_failures) {
    failed.count_blk_errors(Fs::from_c(c))
}

#[c_export]
pub fn bch2_mark_io_failure(failed: &mut c::bch_io_failures, p: &c::extent_ptr_decoded, err: c_int) {
    failed.mark(p, BchError::from_raw(-err));
}

/// 1 and @pick written, 0 for nothing to read, or an error. @failed may be
/// NULL, and @pick is written, not read.
#[c_export]
pub fn bch2_bkey_pick_read_device(c: &Opaque<Fs>, k: BkeySC<'_>, failed: Option<&c::bch_io_failures>,
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

fn ptrs(k: BkeyS<'_>) -> BkeyS<'_, Ptrs> {
    k.arm::<Ptrs>().expect("dropping pointers of a key without them")
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
        fs.request_incompat_feature(MetadataVersion::extent_flags)?;

        // SAFETY: the caller's
        let k = unsafe { TransBkey::from_raw_room(&t, k, buf_u64s) };
        k.arm::<PtrsMut>().expect("extent flags on a key whose entries can't be edited")
            .extent_flags_set(fs, flags).expect("room for an extent flags entry: its caller allocates it");
        Ok(())
    })())
}

/// @k is in @trans's memory, @buf_u64s long: room for a crc128 entry more
/// than it has. narrow_crc(), for read.c until it's Rust.
///
/// # Safety
/// C has an attempt in progress on @trans, and @k is a key in its memory,
/// @buf_u64s u64s of it its.
#[c_export]
pub unsafe fn bch2_bkey_narrow_crc(trans: &BtreeTrans<'_>, k: *mut c::bkey_i, buf_u64s: c_uint,
                                   old: c::bch_extent_crc_unpacked, new: c::bch_extent_crc_unpacked) -> bool {
    let t = unsafe { trans.attempt_in_progress() };
    let k = unsafe { TransBkey::from_raw_room(&t, k, buf_u64s) };
    k.arm::<PtrsMut>().expect("narrowing a crc of a key whose entries can't be edited")
        .narrow_crc(t.fs(), old, new)
}
