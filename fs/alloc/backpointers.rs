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
//! backpointers.h's inlines are the same encoding, for C's callers.
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
//!
//! Still C's: bp_scan_iter's peek, which migrate.c and reconcile walk with
//! too, and which begins the caller's attempt again inside it.

use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{pos, BkeyI, BkeyS, BkeySC};
use crate::btree::iter::{bkey_s_c_to_result, is_restart, polonius_key, BtreeIter, BtreeIterFlags, BtreeTrans,
                         CommitFlags, TransAttempt, UpdateTriggerFlags};
use crate::btree::write_buffer::WbFlush;
use crate::c;
use crate::data::ptrs::{crc_is_compressed, DecodedPtr, Ptrs};
use crate::debug::params as debug_params;
use crate::errcode::{bch_errcode, err_ptr, ret_to_c, BchError};
use crate::fs::Fs;
use crate::init::error::{id, BkeyValidate};
use crate::init::passes;
use crate::init::progress::Progress;
use crate::opts::prt_data_type;
use crate::util::ffi::Opaque;
use crate::util::log::LogMsg;
use crate::util::rcu::read_lock;
use crate::util::vstructs::plain_bytes;
use crate::util::Printbuf;
use crate::{bkey_fsck_err_on, fsck_err, prt};
use core::ffi::{c_int, c_uint};
use core::marker::PhantomData;
use core::ops::RangeInclusive;
use cstruct_macros::c_export;

/// A backpointer btree position, ordered as the bpos it's encoded in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct BackpointerPos {
    pub dev:       u64,
    pub sector:    u64,
    /// < 1 << extent_bp_shift
    pub suboffset: u32,
}

impl BackpointerPos {
    pub fn from_bpos(fs: &Fs, p: c::bpos) -> Self {
        let shift = fs.sb.extent_bp_shift.get();
        Self {
            dev:       p.inode,
            sector:    p.offset >> shift,
            suboffset: (p.offset & ((1 << shift) - 1)) as u32,
        }
    }

    pub fn to_bpos(self, fs: &Fs) -> c::bpos {
        pos(self.dev, (self.sector << fs.sb.extent_bp_shift.get()) + u64::from(self.suboffset))
    }

    pub fn bucket(self, ca: &c::bch_dev) -> c::bpos {
        self.bucket_and_offset(ca).0
    }

    pub fn bucket_and_offset(self, ca: &c::bch_dev) -> (c::bpos, u32) {
        let (bucket, offset) = ca.sector_to_bucket_and_offset(self.sector);
        (pos(self.dev, bucket), offset)
    }

    pub fn bucket_range(ca: &c::bch_dev, bucket: c::bpos) -> RangeInclusive<c::bpos> {
        let fs = ca.fs();
        let start = |b: u64| Self { dev: bucket.inode, sector: ca.bucket_to_sector(b), suboffset: 0 }
            .to_bpos(fs);

        start(bucket.offset)..=start(bucket.offset + 1).nosnap_predecessor()
    }
}

// ── The key type ─────────────────────────────────────────────────────────

fn backpointer_validate(v: &BkeyValidate<'_, '_>) -> Result<(), BchError> {
    let bp = v.k.arm::<c::Backpointer>().expect("a backpointer");
    let level = bp.v().level;

    bkey_fsck_err_on!(v, u32::from(level) > c::BTREE_MAX_DEPTH, id::backpointer_level_bad,
                      "backpointer level bad: {level} >= {}", c::BTREE_MAX_DEPTH)?;
    bkey_fsck_err_on!(v, bp.k.p.inode == u64::from(c::BCH_SB_MEMBER_INVALID), id::backpointer_dev_bad,
                      "backpointer for BCH_SB_MEMBER_INVALID")
}

#[c_export]
pub fn bch2_backpointer_validate(c: &Opaque<c::bch_fs>, k: BkeySC<'_>, from: &c::bkey_validate_context)
    -> c_int
{
    ret_to_c(backpointer_validate(&BkeyValidate { fs: Fs::from_c(c), k, from }))
}

#[cold]
#[c_export]
pub fn bch2_backpointer_to_text(out: &mut Printbuf, c: Option<&Opaque<c::bch_fs>>, k: BkeySC<'_>) {
    let fs = c.map(Fs::from_c);
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

    unsafe { c::bch2_btree_id_level_to_text(out.as_raw(), c::btree_id(v.btree_id.into()), v.level.into()) };
    prt!(out, " data_type=");
    prt_data_type(out, c::bch_data_type(v.data_type.into()));
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

#[c_export]
pub fn bch2_backpointer_swab(_c: Option<&Fs>, k: BkeyS<'_>) {
    let mut bp = k.arm::<c::Backpointer>().expect("a backpointer");
    let v = bp.v_mut();

    v.flags = { v.flags }.swap_bytes();
    v.bucket_len = { v.bucket_len }.swap_bytes();
    let mut pos = v.pos;
    pos.swab();
    v.pos = pos;
}

// ── An extent pointer's backpointer ──────────────────────────────────────

impl c::Backpointer {
    /// Stripe blocks on removed devices have a btree of their own.
    pub fn btree(&self) -> c::btree_id {
        if self.stripe_ptr() { c::btree_id::stripe_backpointers } else { c::btree_id::backpointers }
    }

    /// Its reconcile phys bits: its extent's work, for a pointer on a
    /// rotational device - bp_reconcile_phys().
    pub fn reconcile_work_phys(&self) -> c::reconcile_work_id {
        c::reconcile_work_id(self.reconcile_phys() as u32)
    }

    pub fn set_reconcile_work_phys(&mut self, w: c::reconcile_work_id) {
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
pub fn bp_reconcile_phys(fs: &Fs, level: u32, dev: u32, k: BkeySC<'_>) -> c::reconcile_work_id {
    if level != 0 || !fs.dev_rotational(dev) {
        return c::reconcile_work_id::RECONCILE_WORK_none;
    }
    k.reconcile_work_id(fs).phys()
}

fn ptr_data_type(fs: &Fs, k: BkeySC<'_>, d: &DecodedPtr<'_>) -> c::bch_data_type {
    use c::bch_bkey_type as t;
    use c::bch_data_type as dt;
    match k.k.key_type() {
        t::KEY_TYPE_btree_ptr | t::KEY_TYPE_btree_ptr_v2 => dt::BCH_DATA_btree,
        t::KEY_TYPE_extent | t::KEY_TYPE_reflink_v =>
            if d.p.has_ec { dt::BCH_DATA_stripe }
            else if d.p.ptr.cached() != 0 { dt::BCH_DATA_cached }
            else { dt::BCH_DATA_user },
        t::KEY_TYPE_stripe => {
            let s = k.arm::<c::Stripe>().expect("a stripe");
            let block = s.ptrs(fs).position(|p| core::ptr::eq(p, d.ptr))
                .expect("a pointer of the stripe's");
            let s = s.v();
            assert!(block < s.nr_blocks.into(), "stripe pointer past its blocks");
            if block + usize::from(s.nr_redundant) >= s.nr_blocks.into() { dt::BCH_DATA_parity } else { dt::BCH_DATA_user }
        }
        _ => panic!("the data type of a pointer in a key without pointers"),
    }
}

fn ptr_disk_sectors(sectors: u64, p: &c::extent_ptr_decoded) -> u64 {
    if crc_is_compressed(&p.crc) {
        (sectors * u64::from(p.crc.compressed_size)).div_ceil(p.crc.uncompressed_size.into())
    } else {
        sectors
    }
}

/// A stripe's is past the end of its blocks, clear of its extents' backpointers.
fn extent_ptr_to_bp_pos(fs: &Fs, k: BkeySC<'_>, p: &c::extent_ptr_decoded) -> c::bpos {
    let shift = fs.sb.extent_bp_shift.get();
    let dev = u64::from(p.ptr.dev());

    if let Some(s) = k.arm::<c::Stripe>() {
        return pos(dev, ((p.ptr.offset() + u64::from(s.v().sectors.get())) << shift).wrapping_sub(1));
    }

    let mut ret = pos(dev, (p.ptr.offset() << shift) + u64::from(p.crc.offset));
    if u32::from(p.ptr.dev()) == c::BCH_SB_MEMBER_INVALID && p.has_ec {
        ret.inode = p.ec.idx() << 8 | u64::from(p.ec.block());
    }
    ret
}

pub fn extent_ptr_to_bp(fs: &Fs, btree: c::btree_id, level: u32, k: BkeySC<'_>, d: &DecodedPtr<'_>)
    -> BkeyI<c::Backpointer>
{
    let dev = u32::from(d.p.ptr.dev());
    let sectors = if level != 0 { fs.btree_sectors().into() } else { k.k.size.into() };

    let mut bp = BkeyI::<c::Backpointer>::new();
    bp.k.p = extent_ptr_to_bp_pos(fs, k, &d.p);
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
    let pass = c::bch_recovery_pass::BCH_RECOVERY_PASS_check_extents_to_backpointers;

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

    passes::run_explicit(fs, &mut msg, pass, c::bch_run_recovery_pass_flags::from_bits_retain(0))
}

fn bucket_backpointer_mod_nowritebuffer(t: &TransAttempt<'_, '_>, orig_k: BkeySC<'_>,
                                        bp: &mut BkeyI<c::Backpointer>, insert: bool)
    -> Result<(), BchError>
{
    let mut iter = BtreeIter::new(t, bp.v.btree(), bp.k.p, BtreeIterFlags::INTENT);
    let k = iter.peek_slot(t)?.expect("a slot always has a key");

    let wrong = if insert {
        k.k.type_ != c::bch_bkey_type::KEY_TYPE_deleted.0 as u8
    } else {
        k.arm::<c::Backpointer>().is_none_or(|found| !backpointers_match(found.v(), &bp.v))
    };
    if wrong {
        backpointer_mod_err(t, orig_k, bp, k, insert)?;
    }

    if !insert {
        bp.k.type_ = c::bch_bkey_type::KEY_TYPE_deleted.0 as u8;
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
    if phys != c::reconcile_work_id::RECONCILE_WORK_none {
        let btree = phys.phys_btree().expect("phys bits that are never pending: bp_reconcile_phys()'s");
        t.bit_mod_buffered(btree, bp.k.p, insert)?;
    }

    if debug_params::backpointers_no_use_write_buffer() {
        return bucket_backpointer_mod_nowritebuffer(t, orig_k, bp, insert);
    }

    if !insert {
        bp.k.type_ = c::bch_bkey_type::KEY_TYPE_deleted.0 as u8;
        bp.k.set_val_bytes(0);
    }

    let n = t.bkey_copy(bp.as_ref())?;
    t.update_buffered(bp.v.btree(), &n)
}

/// # Safety
/// @trans is in an attempt; @bp is a backpointer key, all of it.
#[c_export]
pub unsafe fn bch2_bucket_backpointer_mod<'t>(trans: &'t BtreeTrans<'t>, orig_k: BkeySC<'_>,
                                              bp: *mut c::bkey_i_backpointer, insert: bool) -> c_int {
    let t = unsafe { trans.attempt_in_progress() };
    // SAFETY: C's struct bkey_i_backpointer is a BkeyI<Backpointer>
    let bp = unsafe { &mut *bp.cast::<BkeyI<c::Backpointer>>() };
    ret_to_c(bucket_backpointer_mod(&t, orig_k, bp, insert))
}

// ── What a backpointer points to ─────────────────────────────────────────

fn extent_matches_bp(fs: &Fs, btree: c::btree_id, level: u32, k: BkeySC<'_>,
                     bp: BkeySC<'_, c::Backpointer>) -> bool {
    let Some(kp) = k.arm::<Ptrs>() else { return false };

    let matches = kp.ptrs_decoded(fs).any(|d| {
        let bp2 = extent_ptr_to_bp(fs, btree, level, k, &d);
        bp.k.p == bp2.k.p && backpointers_match(bp.v(), &bp2.v)
    });
    matches
}

fn hole_at(pos: c::bpos) -> BkeyI<c::Deleted> {
    let mut k = BkeyI::<c::Deleted>::new();
    k.k.p = pos;
    k
}

/// Commits.
pub(super) fn backpointer_del(t: &TransAttempt<'_, '_>, btree: c::btree_id, pos: c::bpos) -> Result<(), BchError> {
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
            let bp2 = extent_ptr_to_bp(fs, c::btree_id(v.btree_id.into()), v.level.into(), target, &d);
            prt!(&mut buf, "\nwant:  {}", BkeySC::<c::Backpointer>::from(&bp2).to_text(fs));
        }
    }

    if fsck_err!(t, id::backpointer_to_missing_ptr, "{}", buf)? {
        backpointer_del(t, v.btree(), bp.k.p)?;

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
    -> Result<Option<&'i c::btree>, BchError>
{
    let fs = t.fs();
    let v = *bp.v();
    let btree = c::btree_id(v.btree_id.into());

    assert!(v.level != 0, "a btree node's backpointer at level 0");

    *iter = BtreeIter::new_level(t, btree, v.pos, u32::from(v.level) - 1, BtreeIterFlags::empty());
    let Some(b) = iter.peek_node(t)? else {
        // Backpointer for nonexistent tree depth:
        let hole = hole_at(v.pos);
        backpointer_target_not_found(t, bp, BkeySC::<c::Deleted>::from(&hole).untyped(), last_flushed, commit)?;
        return Ok(None);
    };

    assert_eq!(u32::from(b.c.level), u32::from(v.level) - 1, "node at the wrong level");

    let node_k = BkeySC::from(b.key_i());
    if extent_matches_bp(fs, btree, v.level.into(), node_k, bp) {
        return Ok(Some(b));
    }

    if b.will_make_reachable() {
        return fs.throw(bch_errcode::BCH_ERR_backpointer_to_overwritten_btree_node);
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
    let btree = c::btree_id(v.btree_id.into());

    if u32::from(v.btree_id) >= fs.btree_id_nr_alive() {
        return Ok(None);
    }

    *iter = BtreeIter::new_level(t, btree, v.pos, v.level.into(), flags);

    // peek_slot() doesn't normally return None - except when we ask for a
    // key at a btree level that doesn't exist:
    let hole = hole_at(v.pos);
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
        Ok(b) => Ok(b.map(|b| BkeySC::from(b.key_i()))),
        Err(e) if e.matches(bch_errcode::BCH_ERR_backpointer_to_overwritten_btree_node) => Ok(None),
        Err(e) => Err(e),
    }
}

/// None if there's nothing: reported, and the backpointer maybe deleted, committing.
pub fn backpointer_get_key<'i, 't>(t: &'i TransAttempt<'t, '_>, bp: BkeySC<'_, c::Backpointer>,
                                   iter: &'i mut BtreeIter<'t>, flags: BtreeIterFlags,
                                   last_flushed: WbFlush<'_>)
    -> Result<Option<BkeySC<'i>>, BchError>
{
    get_key(t, bp, iter, flags, Some(last_flushed), true)
}

pub fn backpointer_get_node<'i, 't>(t: &'i TransAttempt<'t, '_>, bp: BkeySC<'_, c::Backpointer>,
                                    iter: &'i mut BtreeIter<'t>, last_flushed: WbFlush<'_>)
    -> Result<Option<&'i c::btree>, BchError>
{
    get_node(t, bp, iter, Some(last_flushed), true)
}

/// An error as an ERR_PTR().
fn key_or_err_to_c(r: Result<Option<BkeySC<'_>>, BchError>) -> c::bkey_s_c {
    match r {
        Ok(Some(k)) => k.into(),
        Ok(None)    => c::bkey_s_c { k: core::ptr::null_mut(), v: core::ptr::null_mut() },
        Err(e)      => c::bkey_s_c { k: err_ptr(e), v: core::ptr::null_mut() },
    }
}

/// # Safety
/// @trans is in an attempt; @last_flushed is NULL, or the caller's.
#[c_export]
pub unsafe fn bch2_backpointer_get_key<'t>(trans: &'t BtreeTrans<'t>, bp: c::bkey_s_c_backpointer,
                                           iter: &mut BtreeIter<'t>, iter_flags: c_uint,
                                           last_flushed: *mut c::wb_maybe_flush) -> c::bkey_s_c {
    let t = unsafe { trans.attempt_in_progress() };
    let bp = c::bkey_s_c::from(bp);
    let bp = unsafe { BkeySC::from_c(&bp) }.arm::<c::Backpointer>().expect("a backpointer");
    let flags = BtreeIterFlags::from_bits_retain(iter_flags);
    key_or_err_to_c(get_key(&t, bp, iter, flags, unsafe { WbFlush::from_raw(last_flushed) }, true))
}

/// # Safety
/// @trans is in an attempt; @last_flushed is NULL, or the caller's.
#[c_export]
pub unsafe fn bch2_backpointer_get_node<'t>(trans: &'t BtreeTrans<'t>, bp: c::bkey_s_c_backpointer,
                                            iter: &mut BtreeIter<'t>, last_flushed: *mut c::wb_maybe_flush)
    -> *mut c::btree
{
    let t = unsafe { trans.attempt_in_progress() };
    let bp = c::bkey_s_c::from(bp);
    let bp = unsafe { BkeySC::from_c(&bp) }.arm::<c::Backpointer>().expect("a backpointer");
    match get_node(&t, bp, iter, unsafe { WbFlush::from_raw(last_flushed) }, true) {
        Ok(b)  => b.map_or(core::ptr::null_mut(), |b| core::ptr::from_ref(b).cast_mut()),
        Err(e) => err_ptr(e),
    }
}

// ── Backpointers in the order of what they point to ──────────────────────

/// A backpointer btree walked in the order of what its backpointers point to: read
/// in batches as big as memory allows, sorted by target. What it hands out are
/// copies in its batch, which a restart doesn't invalidate.
pub struct BpScanIter<'p> {
    raw:       c::bp_scan_iter,
    _progress: PhantomData<&'p c::progress_indicator>,
}

impl<'p> BpScanIter<'p> {
    pub fn new(btree: c::btree_id, start: c::bpos, progress: Option<&'p Progress<'_>>) -> Self {
        let raw = c::bp_scan_iter {
            btree,
            pos: start,
            progress: progress.map_or(core::ptr::null_mut(), Progress::as_raw),
            ..Default::default()
        };
        Self { raw, _progress: PhantomData }
    }

    /// Each backpointer in an attempt of its own, retried on restart; the peek and @f
    /// share @last_flushed.
    pub fn for_each<F>(&mut self, trans: &BtreeTrans<'_>, end: c::bpos,
                       mut last_flushed: WbFlush<'_>, mut f: F) -> Result<(), BchError>
    where
        F: for<'a> FnMut(&TransAttempt<'a, '_>, BkeySC<'_, c::Backpointer>, WbFlush<'_>)
            -> Result<(), BchError>,
    {
        loop {
            let t = trans.begin();

            // What's peeked is in our batch, until advanced past
            let bp = unsafe {
                c::rust_bp_scan_iter_peek(trans.raw(), &mut self.raw, end, last_flushed.as_raw())
            };
            let bp = match unsafe { bkey_s_c_to_result(bp) } {
                Ok(None)                 => return Ok(()),
                Ok(Some(bp))             => bp.arm::<c::Backpointer>().expect("a backpointer"),
                Err(e) if is_restart(&e) => continue,
                Err(e)                   => return Err(e),
            };

            match f(&t, bp, last_flushed.reborrow()) {
                Err(e) if is_restart(&e) => continue,
                Err(e)                   => return Err(e),
                Ok(())                   => {}
            }

            t.verify_not_restarted();
            // bch2_bp_scan_iter_advance()
            assert!(self.raw.bps.nr != 0, "advancing a backpointer scan with nothing peeked");
            self.raw.bps.nr -= 1;
        }
    }
}

impl Drop for BpScanIter<'_> {
    fn drop(&mut self) {
        // SAFETY: its batch, kvmalloc()ed by darray_push(), or NULL
        unsafe { c::rust_kvfree(self.raw.bps.data.cast()) }
    }
}
