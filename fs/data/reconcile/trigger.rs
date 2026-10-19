// SPDX-License-Identifier: GPL-2.0

//! Reconcile's state in a key - trigger.c's, as it converts: the reconcile
//! entry, what work is pending and the io options it was computed from, and a
//! btree node pointer's reconcile_bp entry, its backpointer's position in the
//! reconcile_scan btree.

use crate::bkey_fsck_err_on;
use crate::alloc::disk_groups::Target;
use crate::btree::bkey::{pos, BkeyS, BkeySC, BkeyV};
use crate::btree::iter::{BtreeTrans, NoRoom, TransBkey};
use crate::c;
use crate::data::ptrs::{BkeyVPtrs, BkeyVPtrsMut, ExtentEntry, ExtentEntryMut, Ptrs, PtrsMut};
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::init::error::{id, BkeyValidate};
use crate::util::ffi::Opaque;
use crate::util::Printbuf;
use core::ffi::c_uint;
use cstruct_macros::c_export;

/// The work btree a key with @r is tracked in: rb_work_id().
pub fn rb_work_id(r: Option<&c::bch_extent_reconcile>) -> c::reconcile_work_id {
    use c::reconcile_work_id as w;
    match r {
        Some(r) if r.need_rb() != 0 =>
            if r.pending() != 0 { w::RECONCILE_WORK_pending }
            else if r.hipri() != 0 { w::RECONCILE_WORK_hipri }
            else { w::RECONCILE_WORK_normal },
        _ => w::RECONCILE_WORK_none,
    }
}

/// What a key with pointers carries for reconcile.
pub trait BkeyVReconcile: BkeyVPtrs {
    /// Its reconcile entry: bch2_bkey_reconcile_opts().
    fn reconcile_opts(&self, fs: &Fs) -> Option<&c::bch_extent_reconcile> {
        self.extent_entries(fs).find_map(|e| match e {
            ExtentEntry::reconcile(r) => Some(r),
            _ => None,
        })
    }

    /// The index its reconcile_bp entry has, 0 without one:
    /// bch2_bkey_get_reconcile_bp().
    fn reconcile_bp_idx(&self, fs: &Fs) -> u64 {
        self.extent_entries(fs).find_map(|e| match e {
            ExtentEntry::reconcile_bp(bp) => Some(bp.idx()),
            _ => None,
        }).unwrap_or(0)
    }

    /// Its reconcile backpointer's position: its work id, and its
    /// reconcile_bp entry's index. bch2_bkey_get_reconcile_bp_pos().
    fn reconcile_bp_pos(&self, fs: &Fs) -> c::bpos {
        pos(rb_work_id(self.reconcile_opts(fs)).0.into(), self.reconcile_bp_idx(fs))
    }
}

impl<K: BkeyVPtrs> BkeyVReconcile for K {}

impl BkeySC<'_> {
    /// The work btree it's tracked in - a stripe's by its needs_reconcile, as
    /// stripes have no reconcile entry: bch2_bkey_reconcile_work_id().
    pub fn reconcile_work_id(self, fs: &Fs) -> c::reconcile_work_id {
        if let Some(s) = self.arm::<c::Stripe>() {
            return if s.v().needs_reconcile() != 0 {
                c::reconcile_work_id::RECONCILE_WORK_hipri
            } else {
                c::reconcile_work_id::RECONCILE_WORK_none
            };
        }
        self.arm::<Ptrs>().map_or(c::reconcile_work_id::RECONCILE_WORK_none, |k| rb_work_id(k.reconcile_opts(fs)))
    }
}

impl TransBkey<'_, '_, PtrsMut> {
    /// Its reconcile_bp entry made @idx - dropped for 0, appended if it has
    /// none: bch2_bkey_set_reconcile_bp().
    pub fn set_reconcile_bp(&mut self, fs: &Fs, idx: u64) -> Result<(), NoRoom> {
        let bp = self.extent_entries_mut_at(fs).find_map(|(at, e)| match e {
            ExtentEntryMut::reconcile_bp(bp) => Some((at, bp)),
            _ => None,
        });

        match bp {
            Some((at, _)) if idx == 0 => self.drop_entry(fs, at),
            Some((_, bp)) => bp.set_idx(idx),
            None if idx != 0 => {
                let end = self.entries_end();
                self.insert_entry(end, &[c::bch_extent_reconcile_bp::new().with_idx(idx).into_bits()])?;
            }
            None => {}
        }
        Ok(())
    }
}

/// A reconcile entry against itself: bch2_extent_reconcile_validate().
pub fn extent_reconcile_validate(v: &BkeyValidate<'_, '_>, r: &c::bch_extent_reconcile) -> Result<(), BchError> {
    bkey_fsck_err_on!(v, r.pending() != 0 && r.need_rb() == 0, id::extent_reconcile_bad_pending,
                      "pending incorrectly set")?;
    bkey_fsck_err_on!(v, r.hipri() != 0 &&
                         r.need_rb() & (1 << c::bch_reconcile_opts::BCH_RECONCILE_data_replicas.0) == 0,
                      id::extent_reconcile_bad_hipri, "hipri incorrectly set")?;
    bkey_fsck_err_on!(v, r.data_replicas() == 0, id::extent_reconcile_bad_replicas, "bad replicas")
}

/// The io options a reconcile entry, or a rebalance_v1 one, holds - each
/// marked if it's the inode's: what the two to_text share.
macro_rules! reconcile_opts_to_text {
    ($out:expr, $fs:expr, $r:expr) => {{
        let (out, fs, r): (&mut Printbuf, Option<&Fs>, _) = ($out, $fs, $r);
        let from_inode = |out: &mut Printbuf, v: u8| if v != 0 { write!(out, " (inode)") };
        let target = |out: &mut Printbuf, t: u16| match fs {
            Some(fs) => fs.target_to_text(out, Target::decode(t.into())),
            None => write!(out, "{t}"),
        };

        write!(out, "replicas={}", r.data_replicas());
        from_inode(out, r.data_replicas_from_inode());

        write!(out, " checksum=");
        // SAFETY: @out's printbuf, borrowed
        unsafe { c::bch2_prt_csum_opt(out.as_raw(), c::bch_csum_opt(r.data_checksum().into())) };
        from_inode(out, r.data_checksum_from_inode());

        if r.background_compression() != 0 || r.background_compression_from_inode() != 0 {
            write!(out, " background_compression=");
            // SAFETY: @out's printbuf, borrowed
            unsafe { c::bch2_compression_opt_to_text(out.as_raw(), r.background_compression().into()) };
            from_inode(out, r.background_compression_from_inode());
        }
        if r.background_target() != 0 || r.background_target_from_inode() != 0 {
            write!(out, " background_target=");
            target(out, r.background_target());
            from_inode(out, r.background_target_from_inode());
        }
        if r.promote_target() != 0 || r.promote_target_from_inode() != 0 {
            write!(out, " promote_target=");
            target(out, r.promote_target());
            from_inode(out, r.promote_target_from_inode());
        }
        if r.erasure_code() != 0 || r.erasure_code_from_inode() != 0 {
            write!(out, " ec={}", r.erasure_code());
            from_inode(out, r.erasure_code_from_inode());
        }
    }};
}

/// bch2_extent_rebalance_v1_to_text().
pub fn extent_rebalance_v1_to_text(out: &mut Printbuf, fs: Option<&Fs>, r: &c::bch_extent_rebalance_v1) {
    reconcile_opts_to_text!(out, fs, r);
}

/// What it needs, then its options: bch2_extent_reconcile_to_text().
pub fn extent_reconcile_to_text(out: &mut Printbuf, fs: Option<&Fs>, r: &c::bch_extent_reconcile) {
    write!(out, "need_rb=");
    // SAFETY: C's NULL-terminated table of the BCH_RECONCILE_OPTS() names
    unsafe { out.prt_bitflags(c::bch2_reconcile_opts.as_ptr(), r.need_rb().into()) };
    if r.hipri() != 0 {
        write!(out, " hipri");
    }
    if r.pending() != 0 {
        write!(out, " pending");
    }
    write!(out, " ");
    reconcile_opts_to_text!(out, fs, r);
}

// ── C's ──────────────────────────────────────────────────────────────────

#[cold]
#[c_export]
pub fn bch2_extent_reconcile_to_text(out: &mut Printbuf, c: Option<&Opaque<c::bch_fs>>,
                                     r: &c::bch_extent_reconcile) {
    extent_reconcile_to_text(out, c.map(Fs::from_c), r);
}

#[c_export]
pub fn bch2_bkey_reconcile_opts(c: &Fs, k: BkeySC<'_>) -> *const c::bch_extent_reconcile {
    k.arm::<Ptrs>().and_then(|k| k.reconcile_opts(c).map(|r| r as *const _)).unwrap_or(core::ptr::null())
}

#[c_export]
pub fn bch2_bkey_reconcile_work_id(c: &Fs, k: BkeySC<'_>) -> c::reconcile_work_id {
    k.reconcile_work_id(c)
}

/// POS_MIN for a key without pointers.
#[c_export]
pub fn bch2_bkey_get_reconcile_bp_pos(c: &Fs, k: BkeySC<'_>) -> c::bpos {
    k.arm::<Ptrs>().map_or(pos(0, 0), |k| k.reconcile_bp_pos(c))
}

/// # Safety
/// C has an attempt in progress on @trans, and @k is a key it can edit while
/// that runs, @buf_u64s u64s of it its.
#[c_export]
pub unsafe fn bch2_bkey_set_reconcile_bp(trans: &BtreeTrans<'_>, k: BkeyS<'_>, buf_u64s: c_uint, idx: u64) {
    // SAFETY: the caller's
    let t = unsafe { trans.attempt_in_progress() };
    // SAFETY: the caller's - a struct bkey heads its struct bkey_i
    let k = unsafe { TransBkey::from_raw_room(&t, core::ptr::from_mut(k.k).cast(), buf_u64s) };
    k.arm::<PtrsMut>().expect("a reconcile bp on a key whose entries can't be edited")
        .set_reconcile_bp(t.fs(), idx)
        .expect("room for a reconcile_bp entry: its caller allocates it");
}
