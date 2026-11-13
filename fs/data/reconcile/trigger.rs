// SPDX-License-Identifier: GPL-2.0

//! Reconcile's state in a key - trigger.c's, as it converts: the reconcile
//! entry, what work is pending and the io options it was computed from, and a
//! btree node pointer's reconcile_bp entry, its backpointer's position in the
//! reconcile_scan btree.

#![allow(non_camel_case_types)]

use crate::accounting::ReconcileType;
use crate::{accounting, bkey_fsck_err_on, fsck_err};
use crate::alloc::disk_groups::Target;
use crate::btree::bkey::{pos, BkeySC, BkeyV, BkeyVal};
use crate::btree::trigger::TriggerOp;
use crate::btree::bkey_buf::BkeyBuf;
use crate::btree::iter::{BtreeIter, BtreeIterFlags, CommitFlags, NoRoom, TransAttempt, TransBkey, UpdateTriggerFlags};
use crate::c;
use crate::data::ptrs::{BkeyVPtrs, BkeyVPtrsMut, ExtentEntry, ExtentEntryMut, Ptrs, PtrsMut};
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::init::error::{id, BkeyValidate};
use crate::util::vstructs::{plain_bytes, Validated};
use crate::prt;
use crate::util::printbuf::Msg;
use crate::util::{Printbuf, ToText};
use core::mem::size_of;
use cstruct_macros::{bch_enum, CStruct};
use crate::util::alloc::KVVec;
use crate::format::{Bpos, Btree};

#[bch_enum(closed)]
#[repr(u32)]
pub enum SetNeedsReconcileCtx {
    SET_NEEDS_RECONCILE_opt_change,
    SET_NEEDS_RECONCILE_opt_change_indirect,
    SET_NEEDS_RECONCILE_foreground,
    SET_NEEDS_RECONCILE_other,
}

/* Inodes in different snapshots may have different IO options: */
#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct snapshot_io_opts_entry {
    pub snapshot: u32,
    pub io_opts: c::bch_inode_opts,
}

#[derive(Default)]
pub struct per_snapshot_io_opts {
    pub cur_inum: u64,
    pub metadata: bool,
    pub fs_scan_cookie: bool,
    pub inum_scan_cookie: bool,
    pub dev_cookie: c::bch_devs_mask,

    pub fs_io_opts: c::bch_inode_opts,
    pub d: KVVec<c::snapshot_io_opts_entry>,
}

/// The work btree a key with @r is tracked in: rb_work_id().
pub fn rb_work_id(r: Option<&c::bch_extent_reconcile>) -> c::ReconcileWorkId {
    use c::ReconcileWorkId as w;
    match r {
        Some(r) if r.need_rb() != 0 =>
            if r.pending() != 0 { w::pending }
            else if r.hipri() != 0 { w::hipri }
            else { w::normal },
        _ => w::none,
    }
}

impl c::ReconcileWorkId {
    /// The btree this work is tracked in, none for none: reconcile_work_btree[].
    pub fn btree(self) -> Option<Btree> {
        use c::ReconcileWorkId as w;
        Some(match self {
            w::hipri   => Btree::reconcile_hipri,
            w::normal  => Btree::reconcile_work,
            w::pending => Btree::reconcile_pending,
            _ => return None,
        })
    }

    /// The work its backpointers' phys bits track: pending work has none,
    /// as there's nothing to move yet - rb_work_id_phys().
    pub fn phys(self) -> Self {
        if self == Self::pending { Self::none } else { self }
    }

    /// The phys btree work with this id is tracked in, by bucket position:
    /// reconcile_work_phys_btree[].
    pub fn phys_btree(self) -> Option<Btree> {
        use c::ReconcileWorkId as w;
        Some(match self {
            w::hipri  => Btree::reconcile_hipri_phys,
            w::normal => Btree::reconcile_work_phys,
            _ => return None,
        })
    }
}

/// Where a key at @p in @btree is in the work btrees: stripes at inode 0,
/// reflink at 1 - its inode 0, moved up - and extents, whose inodes start at
/// BCACHEFS_ROOT_INO, after them. data_to_rb_work_pos().
pub fn data_to_rb_work_pos(btree: Btree, mut p: Bpos) -> Bpos {
    if btree == Btree::reflink || btree == Btree::stripes {
        p = p.min(pos(0, u64::MAX));
    }
    if btree == Btree::extents {
        p = p.max(pos(c::BCACHEFS_ROOT_INO as u64, 0));
    }
    if btree == Btree::reflink {
        p.inode += 1;
    }
    p
}

/// The data btree position at @p in the work btrees: rb_work_to_data_pos(),
/// data_to_rb_work_pos() backwards.
pub fn rb_work_to_data_pos(mut p: Bpos) -> c::bbpos {
    let btree = if p.inode == 0 {
        Btree::stripes
    } else if p.inode < c::BCACHEFS_ROOT_INO as u64 {
        p.inode -= 1;
        Btree::reflink
    } else {
        Btree::extents
    };
    c::bbpos { btree, pos: p }
}

/// What a key carries for reconcile: a key with pointers' entries, and any
/// key's work btree.
pub trait BkeyVReconcile: BkeyV {
    /// Its reconcile entry: bch2_bkey_reconcile_opts().
    fn reconcile_opts(&self, fs: &Fs) -> Option<&c::bch_extent_reconcile>
    where Self: BkeyVPtrs
    {
        self.extent_entries(fs).find_map(|e| match e {
            ExtentEntry::reconcile(r) => Some(r),
            _ => None,
        })
    }

    /// The index its reconcile_bp entry has, 0 without one:
    /// bch2_bkey_get_reconcile_bp().
    fn reconcile_bp_idx(&self, fs: &Fs) -> u64
    where Self: BkeyVPtrs
    {
        self.extent_entries(fs).find_map(|e| match e {
            ExtentEntry::reconcile_bp(bp) => Some(bp.idx()),
            _ => None,
        }).unwrap_or(0)
    }

    /// Its reconcile backpointer's position: its work id, and its
    /// reconcile_bp entry's index.
    fn reconcile_bp_pos(&self, fs: &Fs) -> Bpos
    where Self: BkeyVPtrs
    {
        pos(rb_work_id(self.reconcile_opts(fs)).0.into(), self.reconcile_bp_idx(fs))
    }

    /// The work btree it's tracked in - a stripe's by its needs_reconcile, as
    /// stripes have no reconcile entry: bch2_bkey_reconcile_work_id().
    fn reconcile_work_id(&self, fs: &Fs) -> c::ReconcileWorkId
    where Self::V: BkeyVal<State = Validated>
    {
        let key = self.as_sc();
        if let Some(s) = key.arm::<c::Stripe>() {
            return if s.v().needs_reconcile() != 0 {
                c::ReconcileWorkId::hipri
            } else {
                c::ReconcileWorkId::none
            };
        }
        key.arm::<Ptrs>().map_or(c::ReconcileWorkId::none, |k| rb_work_id(k.reconcile_opts(fs)))
    }
}

impl<K: BkeyV> BkeyVReconcile for K {}

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
pub(crate) fn rb_bp(btree: Btree, level: u32, k: BkeySC<'_>) -> c::Backpointer {
    c::Backpointer { btree_id: btree.0 as u8, level: level as u8, pos: k.k.p, ..Default::default() }
}

/// Delete btree node pointer @k's reconcile backpointer at @bp_pos - if it is
/// @k's; else that's reported, and there's nothing to delete:
/// reconcile_bp_del().
fn del_reconcile_bp(t: &TransAttempt<'_, '_>, btree: Btree, level: u32, k: BkeySC<'_>,
                    bp_pos: Bpos) -> Result<(), BchError> {
    let fs = t.fs();
    let mut iter = BtreeIter::new(t, Btree::reconcile_scan, bp_pos, BtreeIterFlags::INTENT);
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
pub(crate) fn add_reconcile_bp(t: &TransAttempt<'_, '_>, btree: Btree, level: u32, k: BkeySC<'_>,
                    work: u64) -> Result<Bpos, BchError> {
    let mut iter = BtreeIter::uninit();
    t.bkey_get_empty_slot(&mut iter, Btree::reconcile_scan, pos(work, 1), pos(work, u64::MAX))?;

    let mut bp = t.bkey_alloc_typed::<c::Backpointer>()?;
    bp.k_mut().p = iter.pos();
    *bp.v_mut() = rb_bp(btree, level, k);
    t.update(&iter, &bp, UpdateTriggerFlags::empty())?;
    Ok(iter.pos())
}

// ── the trigger ──────────────────────────────────────────────────────────

/// What a reconcile entry counts toward reconcile_work accounting: its need_rb
/// bits - their accounting types are ReconcileOpt's order - pending in
/// place of target and replicas, or high_priority added: rb_accounting_counters().
fn rb_accounting_counters(r: Option<&c::bch_extent_reconcile>) -> u32 {
    let Some(r) = r else { return 0 };
    let mut ret = u32::from(r.need_rb());
    if r.pending() != 0 {
        ret |= 1 << ReconcileType::pending.0;
        ret &= !(1 << ReconcileType::target.0 | 1 << ReconcileType::replicas.0);
    } else if r.hipri() != 0 {
        ret |= 1 << ReconcileType::high_priority.0;
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
            accounting::add(t, accounting::DevLeaving { dev: d.ptr.dev().into() },
                            accounting::DevLeavingCounters { sectors: v },
                            flags.contains(UpdateTriggerFlags::GC))?;
        }
    }
    Ok(())
}

/// Reconcile's part of the extent trigger, for a key with or getting a
/// reconcile entry: bch2_trigger_extent_reconcile(). The key's work btree bit
/// - or a btree node pointer's reconcile backpointer, moved to its new work
/// id's and recorded in its reconcile_bp entry - then the reconcile_work
/// accounting and dev_leaving counters.
pub(crate) fn trigger_extent_reconcile(t: &TransAttempt<'_, '_>, op: &mut TriggerOp<'_>) -> Result<(), BchError> {
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

            let mut v = accounting::ReconcileWorkCounters::<i64>::default();
            let s = if sectors.is_some() { &mut v.metadata } else { &mut v.data };
            if old_a >> ty & 1 != 0 {
                *s -= old_size;
            }
            if new_a >> ty & 1 != 0 {
                *s += new_size;
            }
            accounting::add(t, accounting::ReconcileWork { type_: ty as u8 }, v,
                            flags.contains(UpdateTriggerFlags::GC))?;
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
                         r.need_rb() & (1 << c::ReconcileOpt::data_replicas.0) == 0,
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
        c::CsumOpt(r.data_checksum().into()).to_text(out);
        from_inode(out, r.data_checksum_from_inode());

        if r.background_compression() != 0 || r.background_compression_from_inode() != 0 {
            write!(out, " background_compression=");
            crate::data::compress::opt_to_text(out, r.background_compression().into());
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
    crate::util::names::prt_set::<c::ReconcileOpt>(out, r.need_rb().into());
    if r.hipri() != 0 {
        write!(out, " hipri");
    }
    if r.pending() != 0 {
        write!(out, " pending");
    }
    write!(out, " ");
    reconcile_opts_to_text!(out, fs, r);
}

// ── A btree node backpointer's node ──────────────────────────────────────

/// The btree node pointer reconcile_scan backpointer @bp is to, copied: None
/// if the node isn't reachable yet, or the pointer isn't there - reported, and
/// the backpointer deleted if fsck fixes it. @iter is made an iterator at
/// @bp's level, the pointer's, for updating it: reconcile_bp_get_key().
///
/// The node is peeked a level down, so it's the node that's locked, not its
/// parent - for btree_node_update_key() on a node not yet written - and it's
/// copied while it is.
pub(crate) fn reconcile_bp_get_key<'t>(t: &TransAttempt<'t, '_>, iter: &mut BtreeIter<'t>,
                                       bp: BkeySC<'_, c::Backpointer>) -> Result<Option<BkeyBuf>, BchError> {
    let fs = t.fs();
    let v = *bp.v();
    let btree = Btree(v.btree_id.into());

    if v.level == 0 {
        if fsck_err!(t, id::reconcile_bp_to_leaf_node_key, "reconcile bp to leaf node key\n{}",
                     bp.untyped().to_text(fs))? {
            delete_bp(t, bp)?;
        }
        return Ok(None);
    }

    *iter = BtreeIter::new_level(t, btree, v.pos, v.level.into(), BtreeIterFlags::empty());

    let mut node_iter = BtreeIter::new_level(t, btree, v.pos, u32::from(v.level) - 1, BtreeIterFlags::empty());
    let b = node_iter.peek_node(t)?;
    if let Some(b) = b {
        if b.will_make_reachable() {
            return Ok(None);
        }
        let k = BkeySC::from(b.contents().key_i());
        if k.arm::<Ptrs>().map_or(pos(0, 0), |k| k.reconcile_bp_pos(fs)) == bp.k.p {
            return Ok(Some(BkeyBuf::from_key(k)));
        }
    }

    let node_key = Msg(|out: &mut Printbuf| match b {
        Some(b) => prt!(out, "{}", BkeySC::from(b.contents().key_i()).to_text(fs)),
        None    => prt!(out, "(no key)"),
    });
    if fsck_err!(t, id::reconcile_bp_to_missing_btree_ptr, "reconcile backpointer to missing/incorrect btree ptr\n{}\n{}",
                 bp.untyped().to_text(fs), node_key)? {
        delete_bp(t, bp)?;
    }
    Ok(None)
}

fn delete_bp(t: &TransAttempt<'_, '_>, bp: BkeySC<'_, c::Backpointer>) -> Result<(), BchError> {
    t.delete(Btree::reconcile_scan, bp.k.p, UpdateTriggerFlags::empty())?;
    t.commit(None, CommitFlags::NO_ENOSPC)
}
