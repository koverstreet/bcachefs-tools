// SPDX-License-Identifier: GPL-2.0

//! Reconcile's state in a key - trigger.c's, as it converts: the reconcile
//! entry, what work is pending and the io options it was computed from, and a
//! btree node pointer's reconcile_bp entry, its backpointer's position in the
//! reconcile_scan btree.

use crate::{accounting, bkey_fsck_err_on, fsck_err};
use crate::alloc::disk_groups::Target;
use crate::btree::bkey::{bpos_max, bpos_min, pos, BkeyS, BkeySC, BkeyV};
use crate::btree::bkey_methods::TriggerOp;
use crate::btree::iter::{BtreeIter, BtreeIterFlags, BtreeTrans, NoRoom, TransAttempt, TransBkey,
                         UpdateTriggerFlags};
use crate::c;
use crate::data::ptrs::{BkeyVPtrs, BkeyVPtrsMut, ExtentEntry, ExtentEntryMut, Ptrs, PtrsMut};
use crate::errcode::{ret_to_c, BchError};
use crate::fs::Fs;
use crate::init::error::{id, BkeyValidate};
use crate::util::ffi::Opaque;
use crate::util::vstructs::plain_bytes;
use crate::util::Printbuf;
use core::ffi::{c_int, c_uint};
use core::mem::size_of;
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

impl c::reconcile_work_id {
    /// The btree this work is tracked in, none for none: reconcile_work_btree[].
    pub fn btree(self) -> Option<c::btree_id> {
        use c::reconcile_work_id as w;
        Some(match self {
            w::RECONCILE_WORK_hipri   => c::btree_id::reconcile_hipri,
            w::RECONCILE_WORK_normal  => c::btree_id::reconcile_work,
            w::RECONCILE_WORK_pending => c::btree_id::reconcile_pending,
            _ => return None,
        })
    }
}

/// Where a key at @p in @btree is in the work btrees: stripes at inode 0,
/// reflink at 1 - its inode 0, moved up - and extents, whose inodes start at
/// BCACHEFS_ROOT_INO, after them. data_to_rb_work_pos().
pub fn data_to_rb_work_pos(btree: c::btree_id, mut p: c::bpos) -> c::bpos {
    use c::btree_id as b;
    if btree == b::reflink || btree == b::stripes {
        p = bpos_min(p, pos(0, u64::MAX));
    }
    if btree == b::extents {
        p = bpos_max(p, pos(c::BCACHEFS_ROOT_INO as u64, 0));
    }
    if btree == b::reflink {
        p.inode += 1;
    }
    p
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

// ── reconcile backpointers ───────────────────────────────────────────────

/// The reconcile backpointer btree node pointer @k, in @btree at @level, has:
/// rb_bp().
fn rb_bp(btree: c::btree_id, level: u32, k: BkeySC<'_>) -> c::Backpointer {
    c::Backpointer { btree_id: btree.0 as u8, level: level as u8, pos: k.k.p, ..Default::default() }
}

/// Delete btree node pointer @k's reconcile backpointer at @bp_pos - if it is
/// @k's; else that's reported, and there's nothing to delete:
/// reconcile_bp_del().
fn del_reconcile_bp(t: &TransAttempt<'_, '_>, btree: c::btree_id, level: u32, k: BkeySC<'_>,
                    bp_pos: c::bpos) -> Result<(), BchError> {
    let fs = t.fs();
    let mut iter = BtreeIter::new(t, c::btree_id::reconcile_scan, bp_pos, BtreeIterFlags::INTENT);
    let bp_k = iter.peek_slot(t)?.expect("a slot");
    let want = rb_bp(btree, level, k);

    if !bp_k.arm::<c::Backpointer>().is_some_and(|bp| plain_bytes(bp.v()) == plain_bytes(&want)) {
        fsck_err!(t, id::btree_ptr_to_bad_reconcile_bp,
                  "btree ptr points to bad/missing reconcile bp\n{}\n{}", k.to_text(fs), bp_k.to_text(fs))?;
        return Ok(());
    }
    t.delete_at(&iter, UpdateTriggerFlags::empty())
}

/// A reconcile backpointer to btree node pointer @k, in the first empty slot
/// of work id @work's: where it went. reconcile_bp_add().
fn add_reconcile_bp(t: &TransAttempt<'_, '_>, btree: c::btree_id, level: u32, k: BkeySC<'_>,
                    work: u64) -> Result<c::bpos, BchError> {
    let mut iter = BtreeIter::uninit();
    t.bkey_get_empty_slot(&mut iter, c::btree_id::reconcile_scan, pos(work, 1), pos(work, u64::MAX))?;

    let mut bp = t.bkey_alloc_typed::<c::Backpointer>()?;
    bp.k_mut().p = iter.pos();
    *bp.v_mut() = rb_bp(btree, level, k);
    t.update(&iter, &bp, UpdateTriggerFlags::empty())?;
    Ok(iter.pos())
}

// ── the trigger ──────────────────────────────────────────────────────────

/// What a reconcile entry counts toward reconcile_work accounting: its need_rb
/// bits - their accounting types are BCH_RECONCILE_OPTS()'s order - pending in
/// place of target and replicas, or high_priority added: rb_accounting_counters().
fn rb_accounting_counters(r: Option<&c::bch_extent_reconcile>) -> u32 {
    use c::bch_reconcile_accounting_type as a;
    let Some(r) = r else { return 0 };
    let mut ret = u32::from(r.need_rb());
    if r.pending() != 0 {
        ret |= 1 << a::BCH_RECONCILE_ACCOUNTING_pending.0;
        ret &= !(1 << a::BCH_RECONCILE_ACCOUNTING_target.0 | 1 << a::BCH_RECONCILE_ACCOUNTING_replicas.0);
    } else if r.hipri() != 0 {
        ret |= 1 << a::BCH_RECONCILE_ACCOUNTING_high_priority.0;
    }
    ret
}

/// dev_leaving accounting for @k's pointers moving off their devices - @r's
/// ptrs_moving, by index among the pointers - unless the move is pending:
/// trigger_dev_counters().
fn trigger_dev_counters(t: &TransAttempt<'_, '_>, sectors: Option<u32>, k: BkeySC<'_>,
                        r: Option<&c::bch_extent_reconcile>, flags: UpdateTriggerFlags) -> Result<(), BchError> {
    let Some(r) = r.filter(|r| r.ptrs_moving() != 0 && r.pending() == 0) else { return Ok(()) };
    let Some(k) = k.arm::<Ptrs>() else { return Ok(()) };

    for (i, d) in k.ptrs_decoded(t.fs()).enumerate() {
        if u32::from(r.ptrs_moving()) >> i & 1 != 0 {
            let v = i64::from(sectors.unwrap_or(d.p.crc.compressed_size));
            let v = if flags.contains(UpdateTriggerFlags::OVERWRITE) { -v } else { v };
            let acct = c::disk_accounting_pos::from_arm(c::bch_acct_dev_leaving { dev: d.ptr.dev().into() });
            accounting::add(t, &acct, &[v], flags.contains(UpdateTriggerFlags::GC))?;
        }
    }
    Ok(())
}

/// Reconcile's part of the extent trigger, for a key with or getting a
/// reconcile entry: bch2_trigger_extent_reconcile(). The key's work btree bit
/// - or a btree node pointer's reconcile backpointer, moved to its new work
/// id's and recorded in its reconcile_bp entry - then the reconcile_work
/// accounting and dev_leaving counters.
fn trigger_extent_reconcile(t: &TransAttempt<'_, '_>, mut op: TriggerOp<'_>) -> Result<(), BchError> {
    let fs = t.fs();
    let old_r = op.old.arm::<Ptrs>().and_then(|k| k.reconcile_opts(fs).copied());
    let new_r = BkeySC::from(&op.new).untyped().arm::<Ptrs>().and_then(|k| k.reconcile_opts(fs).copied());

    let needs_trigger = |r: Option<c::bch_extent_reconcile>| r.is_some_and(|r| r.need_rb() | r.ptrs_moving() != 0);
    if !needs_trigger(old_r) && !needs_trigger(new_r) {
        return Ok(());
    }

    let (old_r, new_r) = (old_r.as_ref(), new_r.as_ref());
    let flags = op.flags;

    if flags.contains(UpdateTriggerFlags::TRANSACTIONAL) {
        let (old_work, new_work) = (rb_work_id(old_r), rb_work_id(new_r));

        if op.level == 0 {
            if old_work != new_work {
                // the work btrees index reflink and stripes ahead of extents
                let pos = data_to_rb_work_pos(op.btree, op.new.k.p);
                for (w, set) in [(old_work, false), (new_work, true)] {
                    if let Some(btree) = w.btree() {
                        t.bit_mod_buffered(btree, pos, set)?;
                    }
                }
            }
        } else {
            let old_bp = op.old.arm::<Ptrs>().map_or(0, |k| k.reconcile_bp_idx(fs));
            let mut bp = pos(old_work.0.into(), old_bp);

            if bp.inode != u64::from(new_work.0) && bp.offset != 0 {
                del_reconcile_bp(t, op.btree, op.level, op.old, bp)?;
                bp.offset = 0;
            }
            bp.inode = new_work.0.into();

            if flags.contains(UpdateTriggerFlags::INSERT) {
                let (btree, level) = (op.btree, op.level);
                let needed = u32::from(op.new.k.u64s) +
                    (size_of::<c::bch_extent_reconcile_bp>() / size_of::<u64>()) as u32;
                let k = op.new_with_room(t, needed)?;

                if bp.inode != 0 && bp.offset == 0 {
                    bp = add_reconcile_bp(t, btree, level, k.as_sc(), bp.inode)?;
                }
                k.arm::<PtrsMut>().expect("a btree node pointer, whose entries can be edited")
                    .set_reconcile_bp(fs, bp.offset)
                    .expect("room for a reconcile_bp entry: just made");
            }
        }
    }

    if flags.intersects(UpdateTriggerFlags::TRANSACTIONAL | UpdateTriggerFlags::GC) {
        let new = BkeySC::from(&op.new).untyped();
        // a btree node's sectors, where its pointer's size is 0
        let sectors = (op.level != 0).then(|| fs.btree_sectors());
        let old_size = i64::from(sectors.unwrap_or(op.old.k.size));
        let new_size = i64::from(sectors.unwrap_or(new.k.size));

        let (old_a, new_a) = (rb_accounting_counters(old_r), rb_accounting_counters(new_r));
        let mut delta = if old_size == new_size { old_a ^ new_a } else { old_a | new_a };

        while delta != 0 {
            let ty = delta.trailing_zeros();
            delta &= delta - 1;

            // [data, metadata] sectors
            let mut v = [0i64; 2];
            let i = usize::from(sectors.is_some());
            if old_a >> ty & 1 != 0 {
                v[i] -= old_size;
            }
            if new_a >> ty & 1 != 0 {
                v[i] += new_size;
            }
            let acct = c::disk_accounting_pos::from_arm(c::bch_acct_reconcile_work { type_: ty as u8 });
            accounting::add(t, &acct, &v, flags.contains(UpdateTriggerFlags::GC))?;
        }

        trigger_dev_counters(t, sectors, op.old, old_r, flags - UpdateTriggerFlags::INSERT)?;
        trigger_dev_counters(t, sectors, new, new_r, flags - UpdateTriggerFlags::OVERWRITE)?;
    }
    Ok(())
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

/// # Safety
/// @op is the update the extent trigger is running for, in its commit.
#[c_export]
pub unsafe fn bch2_trigger_extent_reconcile(trans: &BtreeTrans<'_>, mut op: c::btree_trigger_op) -> c_int {
    // SAFETY: the caller's
    let (t, op) = unsafe { (trans.attempt_in_progress(), TriggerOp::from_raw(&mut op)) };
    ret_to_c(trigger_extent_reconcile(&t, op))
}

/// @bp_pos: in, the work id - its inode; out, where the backpointer went.
///
/// # Safety
/// C has an attempt in progress on @trans.
#[c_export]
pub unsafe fn reconcile_bp_add(trans: &BtreeTrans<'_>, btree: c::btree_id, level: c_uint, k: BkeyS<'_>,
                               bp_pos: &mut c::bpos) -> c_int {
    // SAFETY: the caller's
    let t = unsafe { trans.attempt_in_progress() };
    ret_to_c(add_reconcile_bp(&t, btree, level, k.as_sc(), bp_pos.inode).map(|p| *bp_pos = p))
}
