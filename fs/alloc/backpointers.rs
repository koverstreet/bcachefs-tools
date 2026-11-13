// SPDX-License-Identifier: GPL-2.0

//! The backpointer btrees: an index of the extents and btree nodes by the
//! device and sector they point to - for evacuating a device or a bucket,
//! scrub, and fsck of the allocation information.
//!
//! A backpointer's position is dev:(sector << extent_bp_shift) + suboffset -
//! the sector its pointer is at, and the pointer's crc.offset: the extents an
//! encoded (checksummed or compressed) extent is split into share its
//! pointer's sector, at different offsets into its data. extent_bp_shift is
//! the filesystem's, in its superblock, grown to fit encoded_extent_max: the
//! encoding isn't a constant, and converting to and from bpos takes the
//! filesystem. A bucket's backpointers are one range of positions.
//!
//! In BTREE_ID_stripe_backpointers, for erasure coded pointers to a removed
//! device, "dev" is synthetic: (stripe idx << 8) | block - at least 256,
//! never a real device's.
//!
//! What's here:
//! - the key type, and an extent pointer's backpointer: extent_ptr_to_bp()
//! - inserting and deleting them, from the triggers: bucket_backpointer_mod()
//! - what one points to, checked that it points back: backpointer_get_key()
//! - walking a bucket's backpointers in the order of what they point to:
//!   BpScanIter
//!
//! Their fsck is check_backpointers.rs.
//!
//! Backpointers are maintained through the btree write buffer, so a lookup
//! can see a stale one: before calling a backpointer bad, a pass flushes the
//! write buffer, once per key (WbMaybeFlush), and looks again. A repair that
//! deletes one commits inside the caller's attempt - and get_key() returns
//! None for it - as looping on a restart wouldn't see the deletion without
//! another flush: the caller moves on to the next backpointer.

#![allow(non_camel_case_types)]

use crate::init::passes::RecoveryPass;
use crate::fs::Dev;
use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{pos, spos, BkeyI, BkeyS, BkeySC, BkeyOps};
use crate::btree::iter::{lockrestart_do, polonius_key, BtreeIter, BtreeIterFlags, BtreeTrans,
                         CommitFlags, TransAttempt, UpdateTriggerFlags};
use crate::btree::trans::TransGuard;
use crate::btree::write_buffer::WbFlush;
use crate::btree::node::BtreeNode;
use crate::btree::path::LockedNode;
use crate::c;
use crate::data::ptrs::{ptr_data_type, ptr_disk_sectors, DecodedPtr, Ptrs};
use crate::data::reconcile::trigger::BkeyVReconcile;
use crate::debug::params as debug_params;
use crate::errcode::{Errcode, BchError};
use crate::fs::Fs;
use crate::init::error::{id, BkeyValidate};
use crate::init::passes;
use crate::init::progress::Progress;
use crate::locking::MemallocFlags;
use crate::util::alloc::KVVec;
use crate::os::kernel::{totalram_bytes, unlikely, PAGE_SIZE};
use crate::util::log::LogMsg;
use crate::util::rcu::read_lock;
use crate::data_structures::sort::heapsort_by;
use crate::util::vstructs::plain_bytes;
use crate::util::{Printbuf, ToText};
use crate::{bkey_fsck_err_on, fsck_err, prt};
use core::ops::{ControlFlow, RangeInclusive};
use crate::format::{Bpos, Btree, DataType, BkeyType, MetadataVersion};

/// A backpointer btree position, ordered as the bpos it's encoded in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct BackpointerPos {
    pub dev:       u64,
    pub sector:    u64,
    /// < 1 << extent_bp_shift
    pub suboffset: u32,
}

impl BackpointerPos {
    /// In stripe_backpointers, the dev of block @block of stripe @idx, on a
    /// removed device: bp_dev_for_ec_removed_dev().
    pub fn ec_removed_dev(idx: u64, block: u32) -> u64 {
        idx << 8 | u64::from(block)
    }

    pub fn from_bpos(fs: &Fs, p: Bpos) -> Self {
        let shift = fs.sb.extent_bp_shift.get();
        Self {
            dev:       p.inode,
            sector:    p.offset >> shift,
            suboffset: (p.offset & ((1 << shift) - 1)) as u32,
        }
    }

    pub fn to_bpos(self, fs: &Fs) -> Bpos {
        pos(self.dev, (self.sector << fs.sb.extent_bp_shift.get()) + u64::from(self.suboffset))
    }

    pub fn bucket(self, ca: &Dev) -> Bpos {
        self.bucket_and_offset(ca).0
    }

    pub fn bucket_and_offset(self, ca: &Dev) -> (Bpos, u32) {
        let (bucket, offset) = ca.sector_to_bucket_and_offset(self.sector);
        (pos(self.dev, bucket), offset)
    }

    pub fn bucket_range(ca: &Dev, bucket: Bpos) -> RangeInclusive<Bpos> {
        let fs = ca.fs();
        let start = |b: u64| Self { dev: bucket.inode, sector: ca.bucket_to_sector(b), suboffset: 0 }
            .to_bpos(fs);

        // stripe backpointers put the stripe index in the snapshot field
        start(bucket.offset)..=Bpos { snapshot: u32::MAX, ..start(bucket.offset + 1).nosnap_predecessor() }
    }
}

// ── The key type ─────────────────────────────────────────────────────────

impl BkeyOps for c::Backpointer {
    fn validate(v: &BkeyValidate<'_, '_>) -> Result<(), BchError> {
        let bp = v.k.arm::<c::Backpointer>().expect("a backpointer");
        let level = bp.v().level;

        bkey_fsck_err_on!(v, u32::from(level) > c::BTREE_MAX_DEPTH, id::backpointer_level_bad,
                          "backpointer level bad: {level} >= {}", c::BTREE_MAX_DEPTH)?;
        bkey_fsck_err_on!(v, bp.k.p.inode == u64::from(c::BCH_SB_MEMBER_INVALID), id::backpointer_dev_bad,
                          "backpointer for BCH_SB_MEMBER_INVALID")
    }

    #[cold]
    fn to_text(out: &mut Printbuf, fs: Option<&Fs>, k: BkeySC<'_>) {
        let bp = k.arm::<c::Backpointer>().expect("a backpointer");
        let p = bp.k.p;
        let v = bp.v();

        let bucket = fs.and_then(|fs| {
            let rcu = read_lock();
            let ca = fs.dev_rcu(p.inode as u32, &rcu)?;
            Some(BackpointerPos::from_bpos(fs, p).bucket_and_offset(ca))
        });

        match (fs, bucket) {
            (_, Some((b, offset))) => prt!(out, "bucket={}:{}:{offset} ", { b.inode }, { b.offset }),
            (Some(fs), None)       => prt!(out, "sector={}:{} ", { p.inode }, { p.offset } >> fs.sb.extent_bp_shift.get()),
            (None, _)              => prt!(out, "pos={}:{} ", { p.inode }, { p.offset }),
        }

        prt!(out, "btree={} level={}", Btree(v.btree_id.into()), v.level);
        prt!(out, " data_type=");
        DataType(v.data_type.into()).to_text(out);
        match fs {
            Some(fs) => prt!(out, " suboffset={} len={} gen={} pos=",
                             BackpointerPos::from_bpos(fs, p).suboffset, { v.bucket_len }, v.bucket_gen),
            None     => prt!(out, " len={} gen={} pos=", { v.bucket_len }, v.bucket_gen),
        }
        prt!(out, "{}", { v.pos });

        if v.reconcile_phys() != 0 {
            prt!(out, " reconcile_phys={}", v.reconcile_work_phys());
        }
        if v.erasure_coded() {
            prt!(out, " ec");
        }
        if v.stripe_ptr() {
            prt!(out, " stripe");
        }
    }

    fn swab(_fs: Option<&Fs>, k: BkeyS<'_>) {
        let mut bp = k.arm::<c::Backpointer>().expect("a backpointer");
        let v = bp.v_mut();

        v.flags = { v.flags }.swap_bytes();
        v.bucket_len = { v.bucket_len }.swap_bytes();
        let mut pos = v.pos;
        pos.swab();
        v.pos = pos;
    }
}

// ── An extent pointer's backpointer ──────────────────────────────────────

impl c::Backpointer {
    /// The btree it's in - not the one it points into, target()'s: stripe
    /// blocks on removed devices have a btree of their own.
    pub fn bp_btree(&self) -> Btree {
        if self.stripe_ptr() { Btree::stripe_backpointers } else { Btree::backpointers }
    }

    /// What it points to.
    pub fn target(&self) -> c::bbpos {
        c::bbpos { btree: Btree(self.btree_id.into()), pos: self.pos }
    }

    /// Its reconcile phys bits: its extent's work, for a pointer on a
    /// rotational device - bp_reconcile_phys().
    pub fn reconcile_work_phys(&self) -> c::ReconcileWorkId {
        c::ReconcileWorkId(self.reconcile_phys() as u32)
    }

    pub fn set_reconcile_work_phys(&mut self, w: c::ReconcileWorkId) {
        self.set_reconcile_phys(w.0.into());
    }
}

/// All but the reconcile phys bits: a cache of the extent's pending work, which
/// goes stale when a device's rotational flag flips - check_reconcile_work's to
/// repair, not a mismatch.
pub fn backpointers_match(l: &c::Backpointer, r: &c::Backpointer) -> bool {
    let (mut l, mut r) = (*l, *r);
    l.set_reconcile_phys(0);
    r.set_reconcile_phys(0);
    plain_bytes(&l) == plain_bytes(&r)
}

/// Pending work isn't tracked by bucket.
pub fn bp_reconcile_phys(fs: &Fs, level: u32, dev: u32, k: BkeySC<'_>) -> c::ReconcileWorkId {
    if level != 0 || !fs.dev_rotational(dev) {
        return c::ReconcileWorkId::none;
    }
    k.reconcile_work_id(fs).phys()
}

/// A stripe's is past the end of its blocks, clear of its extents' backpointers
/// - and, with the stripe index, of each other's: while a new stripe reuses an
/// old stripe's block both reference it, and without the index, deleting the
/// old stripe deletes the new one's backpointer. Stripe indices are below
/// U32_MAX, so they fit.
fn extent_ptr_to_bp_pos(fs: &Fs, k: BkeySC<'_>, p: &c::extent_ptr_decoded) -> Bpos {
    let shift = fs.sb.extent_bp_shift.get();
    let dev = u64::from(p.ptr.dev());

    if let Some(s) = k.arm::<c::Stripe>() {
        // 1.40's, with inline data's checksums
        let snapshot = if fs.version() >= MetadataVersion::inline_data_csum {
            s.k.p.offset as u32
        } else {
            0
        };
        return spos(dev, ((p.ptr.offset() + u64::from(s.v().sectors.get())) << shift).wrapping_sub(1), snapshot);
    }

    let mut ret = pos(dev, (p.ptr.offset() << shift) + u64::from(p.crc.offset));
    if u32::from(p.ptr.dev()) == c::BCH_SB_MEMBER_INVALID && p.has_ec {
        ret.inode = BackpointerPos::ec_removed_dev(p.ec.idx(), p.ec.block().into());
    }
    ret
}

pub fn extent_ptr_to_bp(fs: &Fs, btree: Btree, level: u32, k: BkeySC<'_>, d: &DecodedPtr<'_>)
    -> BkeyI<c::Backpointer>
{
    let dev = u32::from(d.p.ptr.dev());
    let sectors = if level != 0 { fs.btree_sectors().into() } else { k.k.size.into() };

    let mut bp = BkeyI::<c::Backpointer>::at(extent_ptr_to_bp_pos(fs, k, &d.p));
    bp.v = c::Backpointer {
        btree_id:   btree.0 as u8,
        level:      level as u8,
        data_type:  ptr_data_type(fs, k, d).0 as u8,
        bucket_gen: d.p.ptr.generation(),
        bucket_len: ptr_disk_sectors(sectors, &d.p) as u32,
        pos:        k.k.p,
        ..Default::default()
    };
    bp.v.set_reconcile_work_phys(bp_reconcile_phys(fs, level, dev, k));
    bp.v.set_erasure_coded(d.p.has_ec);
    bp.v.set_stripe_ptr(d.p.has_ec && dev == c::BCH_SB_MEMBER_INVALID);
    bp
}

// ── Updating backpointers ────────────────────────────────────────────────

/// check_extents_to_backpointers repairs this: schedule it, if it isn't going to run.
#[cold]
fn backpointer_mod_err(t: &TransAttempt<'_, '_>, orig_k: BkeySC<'_>, new_bp: &BkeyI<c::Backpointer>,
                       found_bp: BkeySC<'_>, insert: bool) -> Result<(), BchError> {
    let fs = t.fs();
    let pass = RecoveryPass::check_extents_to_backpointers;

    if passes::will_run(fs, pass) {
        return Ok(());
    }

    let new_bp = BkeySC::<c::Backpointer>::from(new_bp);
    let mut msg = LogMsg::new(fs);
    if insert {
        prt!(&mut msg, "existing backpointer found when inserting {}\n", new_bp.to_text(fs));
        let mut out = msg.indent(2);
        prt!(&mut out, "found {}\n", found_bp.to_text(fs));
        prt!(&mut out, "for {}", orig_k.to_text(fs));
    } else {
        prt!(&mut msg, "backpointer not found when deleting\n");
        let mut out = msg.indent(2);
        prt!(&mut out, "searching for {}\n", new_bp.to_text(fs));
        prt!(&mut out, "got {}\n", found_bp.to_text(fs));
        prt!(&mut out, "for {}", orig_k.to_text(fs));
    }

    passes::run_explicit(fs, &mut msg, pass, c::RunRecoveryPassFlags::from_bits_retain(0))
}

fn bucket_backpointer_mod_nowritebuffer(t: &TransAttempt<'_, '_>, orig_k: BkeySC<'_>,
                                        bp: &mut BkeyI<c::Backpointer>, insert: bool)
    -> Result<(), BchError>
{
    let mut iter = BtreeIter::new(t, bp.v.bp_btree(), bp.k.p, BtreeIterFlags::INTENT);
    let k = iter.peek_slot(t)?.expect("a slot always has a key");

    let wrong = if insert {
        k.k.key_type() != BkeyType::deleted
    } else {
        k.arm::<c::Backpointer>().is_none_or(|found| !backpointers_match(found.v(), &bp.v))
    };
    if wrong {
        backpointer_mod_err(t, orig_k, bp, k, insert)?;
    }

    if !insert {
        bp.k.set_key_type(BkeyType::deleted);
        bp.k.set_val_bytes(0);
    }

    // The update points at its key until commit: copied, as @bp is the caller's
    let n = t.bkey_copy(bp.as_ref())?;
    t.update(&iter, &n, UpdateTriggerFlags::empty())
}

/// Also its reconcile phys btree entry. Deleting turns @bp into a deletion.
pub fn bucket_backpointer_mod(t: &TransAttempt<'_, '_>, orig_k: BkeySC<'_>, bp: &mut BkeyI<c::Backpointer>,
                              insert: bool) -> Result<(), BchError> {
    let phys = bp.v.reconcile_work_phys();
    if phys != c::ReconcileWorkId::none {
        let btree = phys.phys_btree().expect("phys bits that are never pending: bp_reconcile_phys()'s");
        t.bit_mod_buffered(btree, bp.k.p, insert)?;
    }

    if debug_params::backpointers_no_use_write_buffer() {
        return bucket_backpointer_mod_nowritebuffer(t, orig_k, bp, insert);
    }

    if !insert {
        bp.k.set_key_type(BkeyType::deleted);
        bp.k.set_val_bytes(0);
    }

    t.update_buffered(bp.v.bp_btree(), &*bp)
}

// ── What a backpointer points to ─────────────────────────────────────────

fn extent_matches_bp(fs: &Fs, btree: Btree, level: u32, k: BkeySC<'_>,
                     bp: BkeySC<'_, c::Backpointer>) -> bool {
    let Some(kp) = k.arm::<Ptrs>() else { return false };

    let matches = kp.ptrs_decoded(fs).any(|d| {
        let bp2 = extent_ptr_to_bp(fs, btree, level, k, &d);
        bp.k.p == bp2.k.p && backpointers_match(bp.v(), &bp2.v)
    });
    matches
}

/// Commits.
pub(super) fn backpointer_del(t: &TransAttempt<'_, '_>, btree: Btree, pos: Bpos) -> Result<(), BchError> {
    if !debug_params::backpointers_no_use_write_buffer() {
        t.bit_mod_buffered(btree, pos, false)?;
    } else {
        t.delete(btree, pos, UpdateTriggerFlags::empty())?;
    }
    t.commit(None, CommitFlags::NO_ENOSPC)
}

pub(super) fn backpointers_maybe_flush(t: &TransAttempt<'_, '_>, visiting_k: BkeySC<'_>,
                            mut last_flushed: WbFlush<'_>) -> Result<(), BchError> {
    if debug_params::backpointers_no_use_write_buffer() {
        return Ok(());
    }
    last_flushed.maybe_flush(t, visiting_k)
}

/// Reported, and on repair deleted - a commit, and with @commit another.
fn backpointer_target_not_found(t: &TransAttempt<'_, '_>, bp: BkeySC<'_, c::Backpointer>,
                                target: BkeySC<'_>, last_flushed: Option<WbFlush<'_>>,
                                commit: bool) -> Result<(), BchError> {
    let fs = t.fs();
    let v = *bp.v();

    // If we're using the btree write buffer, the backpointer we were looking
    // at may have already been deleted - failure to find what it pointed to
    // is not an error:
    if let Some(last_flushed) = last_flushed {
        backpointers_maybe_flush(t, bp.untyped(), last_flushed)?;
    }

    let mut buf = Printbuf::new();
    prt!(&mut buf, "backpointer doesn't match {} it points to:\n",
         if v.level != 0 { "btree node" } else { "extent" });
    prt!(&mut buf, "{}", target.to_text(fs));
    prt!(&mut buf, "\nfound: {}", bp.to_text(fs));

    if let Some(kp) = target.arm::<Ptrs>() {
        for d in kp.ptrs_decoded(fs).filter(|d| u64::from(d.p.ptr.dev()) == bp.k.p.inode) {
            let bp2 = extent_ptr_to_bp(fs, Btree(v.btree_id.into()), v.level.into(), target, &d);
            prt!(&mut buf, "\nwant:  {}", BkeySC::<c::Backpointer>::from(&bp2).to_text(fs));
        }
    }

    if fsck_err!(t, id::backpointer_to_missing_ptr, "{}", buf)? {
        backpointer_del(t, v.bp_btree(), bp.k.p)?;

        // Normally, on transaction commit from inside a transaction, we'll
        // return -BCH_ERR_transaction_restart_nested, since a transaction
        // commit invalidates pointers given out by peek().
        //
        // However, since we're updating a write buffer btree, if we return a
        // transaction restart and loop we won't see that the backpointer has
        // been deleted without an additional write buffer flush - and those
        // are expensive.
        //
        // So we're relying on the caller immediately advancing to the next
        // backpointer and starting a new transaction immediately after
        // backpointer_get_key() returns None:
        if commit {
            t.commit(None, CommitFlags::NO_ENOSPC)?;
        }
    }
    Ok(())
}

/// None if it isn't there, which is reported.
fn get_node<'i, 't>(t: &'i TransAttempt<'t, '_>, bp: BkeySC<'_, c::Backpointer>,
                    iter: &'i mut BtreeIter<'t>, last_flushed: Option<WbFlush<'_>>, commit: bool)
    -> Result<Option<LockedNode<'i, BtreeNode>>, BchError>
{
    let fs = t.fs();
    let v = *bp.v();
    let btree = Btree(v.btree_id.into());

    assert!(v.level != 0, "a btree node's backpointer at level 0");

    *iter = BtreeIter::new_level(t, btree, v.pos, u32::from(v.level) - 1, BtreeIterFlags::empty());
    let Some(b) = iter.peek_node(t)? else {
        // Backpointer for nonexistent tree depth:
        let hole = BkeyI::<c::Deleted>::at(v.pos);
        backpointer_target_not_found(t, bp, BkeySC::<c::Deleted>::from(&hole).untyped(), last_flushed, commit)?;
        return Ok(None);
    };

    assert_eq!(u32::from(b.c.level), u32::from(v.level) - 1, "node at the wrong level");

    let node_k = BkeySC::from(b.contents().key_i());
    if extent_matches_bp(fs, btree, v.level.into(), node_k, bp) {
        return Ok(Some(b));
    }

    if b.will_make_reachable() {
        return fs.throw(Errcode::BCH_ERR_backpointer_to_overwritten_btree_node);
    }

    backpointer_target_not_found(t, bp, node_k, last_flushed, commit)?;
    Ok(None)
}

/// None if it isn't there, which is reported, or is a node not yet reachable.
pub(super) fn get_key<'i, 't>(t: &'i TransAttempt<'t, '_>, bp: BkeySC<'_, c::Backpointer>,
                   iter: &'i mut BtreeIter<'t>, flags: BtreeIterFlags,
                   last_flushed: Option<WbFlush<'_>>, commit: bool)
    -> Result<Option<BkeySC<'i>>, BchError>
{
    let fs = t.fs();
    let v = *bp.v();
    let btree = Btree(v.btree_id.into());

    if unlikely(u32::from(v.btree_id) >= fs.btree_id_nr_alive()) {
        return Ok(None);
    }

    *iter = BtreeIter::new_level(t, btree, v.pos, v.level.into(), flags);

    // peek_slot() doesn't normally return None - except when we ask for a
    // key at a btree level that doesn't exist:
    let hole = BkeyI::<c::Deleted>::at(v.pos);
    let k = iter.peek_slot(t)?.unwrap_or_else(|| BkeySC::<c::Deleted>::from(&hole).untyped());

    if extent_matches_bp(fs, btree, v.level.into(), k, bp) {
        // SAFETY: returned, ending this function's use of @iter
        return Ok(Some(unsafe { polonius_key(k) }));
    }

    if v.level == 0 {
        backpointer_target_not_found(t, bp, k, last_flushed, commit)?;
        return Ok(None);
    }

    match get_node(t, bp, iter, last_flushed, commit) {
        Ok(b) => Ok(b.map(|b| BkeySC::from(b.contents().key_i()))),
        Err(e) if e.matches(Errcode::BCH_ERR_backpointer_to_overwritten_btree_node) => Ok(None),
        Err(e) => Err(e),
    }
}

/// What @bp points to, and @iter, there - at its level, or for a btree node
/// pointer at the node below: None if there's nothing, reported, and the
/// backpointer maybe deleted, committing.
pub fn backpointer_get_key<'i, 't>(t: &'i TransAttempt<'t, '_>, bp: BkeySC<'_, c::Backpointer>,
                                   iter: &'i mut BtreeIter<'t>, flags: BtreeIterFlags,
                                   last_flushed: WbFlush<'_>)
    -> Result<Option<(&'i BtreeIter<'t>, BkeySC<'i>)>, BchError>
{
    // SAFETY: @iter is only read past here, shared with the key
    let k = get_key(t, bp, iter, flags, Some(last_flushed), true)?.map(|k| unsafe { polonius_key(k) });
    Ok(k.map(|k| (&*iter, k)))
}

pub fn backpointer_get_node<'i, 't>(t: &'i TransAttempt<'t, '_>, bp: BkeySC<'_, c::Backpointer>,
                                    iter: &'i mut BtreeIter<'t>, last_flushed: WbFlush<'_>)
    -> Result<Option<LockedNode<'i, BtreeNode>>, BchError>
{
    get_node(t, bp, iter, Some(last_flushed), true)
}

// ── Backpointers in the order of what they point to ──────────────────────

/// A backpointer btree walked in the order of what its backpointers point to: read
/// in batches as big as memory allows, sorted by target. What it hands out are
/// copies in its batch, which a restart doesn't invalidate.
///
/// A peek is in its caller's attempt, and refilling the batch is a nested
/// attempt: it walks the btree in attempts of its own, then returns
/// transaction_restart_nested - and the caller's retry is served from the
/// batch, so it converges.
pub struct BpScanIter<'p> {
    /* BTREE_ID_backpointers, BTREE_ID_stripe_backpointers */
    btree:      Btree,
    pos:        Bpos,
    nr_flushes: u64,
    progress:   Option<&'p Progress<'p>>,
    batch:      KVVec<BkeyI<c::Backpointer>>,
    /* a refill found nothing: the scan's at its end */
    done:       bool,
}

impl BpScanIter<'_> {
    /// The next backpointer, or None past @end: in the caller's attempt @t,
    /// which a refill ends - see BpScanIter.
    fn peek(&mut self, t: &TransAttempt<'_, '_>, end: Bpos, last_flushed: &WbFlush<'_>)
        -> Result<Option<BkeySC<'_, c::Backpointer>>, BchError>
    {
        if self.nr_flushes != last_flushed.nr_flushes() {
            if let Some(pos) = self.batch.last().map(|bp| bp.k.p) {
                let mut iter = BtreeIter::new(t, self.btree, pos, BtreeIterFlags::empty());
                let bp = iter.peek_slot(t)?.and_then(BkeyI::from_key);
                self.batch.pop();
                if let Some(bp) = bp {
                    self.batch.push(bp)?;
                }
            }
            self.nr_flushes = last_flushed.nr_flushes();
        }

        if self.batch.is_empty() && !self.done {
            return Err(t.owner_and_restart(|trans| self.refill(trans, end)));
        }
        Ok(self.batch.last().map(BkeySC::from))
    }

    fn advance(&mut self) {
        self.batch.pop().expect("advancing a backpointer scan with nothing peeked");
    }

    /// The next batch, from pos to @end, sorted last to first by what they
    /// point to - peek() takes from the end. At most a sixteenth of memory's
    /// worth, grown as it's read with nothing locked: a page's worth, then
    /// doubling. When it can't grow, the batch is what's been read - unless
    /// that's nothing.
    fn refill(&mut self, trans: &TransGuard<'_>, end: Bpos) -> Result<(), BchError> {
        let bp_size = size_of::<BkeyI<c::Backpointer>>();
        let max = totalram_bytes() / 16 / bp_size;
        let progress = self.progress;

        let mut iter = BtreeIter::new(trans, self.btree, self.pos, BtreeIterFlags::PREFETCH);
        iter.for_each_max(trans, end, |t, iter, k| {
            let Some(bp) = BkeyI::<c::Backpointer>::from_key(k) else {
                return Ok(ControlFlow::Continue(()));
            };
            if self.batch.len() >= max {
                return Ok(ControlFlow::Break(()));
            }
            if self.batch.len() == self.batch.capacity() {
                // Dropping SRCU too means beginning again, from this key.
                // NOWARN: past INT_MAX bytes kvmalloc() refuses with a
                // warning, and a failed vmalloc() warns too
                t.unlock_long();
                let more = self.batch.len().max(PAGE_SIZE / bp_size);
                let grew = {
                    let _nowarn = MemallocFlags::nowarn();
                    self.batch.reserve(more).is_ok()
                };
                if grew {
                    return Err(t.restart(Errcode::BCH_ERR_transaction_restart_backpointer_scan_grow));
                }
                if self.batch.is_empty() {
                    return t.fs().throw(Errcode::BCH_ERR_ENOMEM_backpointer_scan);
                }
                return Ok(ControlFlow::Break(()));
            }
            self.batch.push(bp)?;
            self.pos = k.k.p.successor();
            if let Some(p) = progress {
                p.update(t, iter)?;
            }
            Ok(ControlFlow::Continue(()))
        })?;
        trans.unlock_long();

        // not sort_unstable(): its recursion overflows the kernel stack
        heapsort_by(&mut self.batch, |a, b| b.v().target().cmp(&a.v().target()));
        self.done = self.batch.is_empty();
        Ok(())
    }
}

impl<'p> BpScanIter<'p> {
    pub fn new(btree: Btree, pos: Bpos, progress: Option<&'p Progress<'_>>) -> Self {
        Self { btree, pos, nr_flushes: 0, progress, batch: KVVec::new(), done: false }
    }

    /// Each backpointer in an attempt of its own, retried on restart; the peek and @f
    /// share @last_flushed.
    pub fn for_each<F>(&mut self, trans: &BtreeTrans<'_>, end: Bpos,
                       mut last_flushed: WbFlush<'_>, mut f: F) -> Result<(), BchError>
    where
        F: for<'a> FnMut(&TransAttempt<'a, '_>, BkeySC<'_, c::Backpointer>, WbFlush<'_>)
            -> Result<(), BchError>,
    {
        while lockrestart_do(trans, |t| {
            let Some(bp) = self.peek(t, end, &last_flushed)? else { return Ok(false) };
            f(t, bp, last_flushed.reborrow())?;
            Ok(true)
        })? {
            self.advance();
        }
        Ok(())
    }
}
