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
//! - the fsck passes: check_btree_backpointers, check_extents_to_backpointers
//!   and check_backpointers_to_extents, and the bucket bitmaps the second
//!   keeps of the buckets it has to look at
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

use crate::alloc::background::alloc_to_v4;
use crate::alloc::buckets::DiskReservation;
use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{pos, spos, BkeyI, BkeyS, BkeySC, POS_MAX, POS_MIN, SPOS_MAX};
use crate::btree::bkey_buf::BkeyBuf;
use crate::btree::cache::BtreeCachePins;
use crate::btree::iter::{bkey_s_c_to_result, commit_do, is_restart, lockrestart_do, polonius_key, BtreeIter,
                         BtreeIterFlags, BtreeTrans, CommitFlags, TransAttempt, UpdateTriggerFlags};
use crate::btree::write_buffer::{WbFlush, WbMaybeFlush};
use crate::c;
use crate::data::extents::Extents;
use crate::data::ptrs::{crc_is_compressed, extents_match, BkeyVPtrs, DecodedPtr, Ptrs};
use crate::data::reflink::make_extent_indirect;
use crate::debug::params as debug_params;
use crate::errcode::{bch_errcode, err_ptr, ret_to_c, ret_to_result, BchError};
use crate::fs::Fs;
use crate::init::error::{id, BkeyValidate};
use crate::init::passes;
use crate::init::progress::Progress;
use crate::opts::prt_data_type;
use crate::types::bits_to_longs;
use crate::util::bitmap::{AtomicBitmap, AtomicULong};
use crate::util::ffi::Opaque;
use crate::util::log::{log_msg_start, print_str, LogLevel, LogMsg, Ratelimit};
use crate::util::rcu::{read_lock, ReadGuard};
use crate::util::vstructs::plain_bytes;
use crate::util::Printbuf;
use crate::{bkey_fsck_err_on, fsck_err, inode_fsck_err, prt};
use core::ffi::{c_int, c_uint};
use core::marker::PhantomData;
use core::ops::{ControlFlow, RangeInclusive};
use core::sync::atomic::Ordering;
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
        let shift = fs.sb.extent_bp_shift;
        Self {
            dev:       p.inode,
            sector:    p.offset >> shift,
            suboffset: (p.offset & ((1 << shift) - 1)) as u32,
        }
    }

    pub fn to_bpos(self, fs: &Fs) -> c::bpos {
        pos(self.dev, (self.sector << fs.sb.extent_bp_shift) + u64::from(self.suboffset))
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

macro_rules! reconcile_work_id_strs {
    ($(($n:ident)),* $(,)?) => { [$(stringify!($n)),*] };
}
const RECONCILE_WORK_ID_STRS: [&str; 4] = c::RECONCILE_WORK_IDS!(reconcile_work_id_strs);

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
        (Some(fs), None)       => prt!(out, "sector={}:{} ", { p.inode }, { p.offset } >> fs.sb.extent_bp_shift),
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
        prt!(out, " reconcile_phys={}", RECONCILE_WORK_ID_STRS[v.reconcile_phys() as usize]);
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
    use c::reconcile_work_id as w;

    if level != 0 || !fs.dev_rotational(dev) {
        return w::RECONCILE_WORK_none;
    }

    match unsafe { c::bch2_bkey_reconcile_work_id(fs.as_ptr(), k.into()) } {
        w::RECONCILE_WORK_pending => w::RECONCILE_WORK_none,
        id => id,
    }
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
    let shift = fs.sb.extent_bp_shift;
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
    bp.v.set_reconcile_phys(bp_reconcile_phys(fs, level, dev, k).0.into());
    bp.v.set_erasure_coded(d.p.has_ec);
    bp.v.set_stripe_ptr(d.p.has_ec && dev == c::BCH_SB_MEMBER_INVALID);
    bp
}

// ── Updating backpointers ────────────────────────────────────────────────

fn reconcile_work_phys_btree(w: c::reconcile_work_id) -> c::btree_id {
    use c::reconcile_work_id as r;
    match w {
        r::RECONCILE_WORK_hipri  => c::btree_id::reconcile_hipri_phys,
        r::RECONCILE_WORK_normal => c::btree_id::reconcile_work_phys,
        w => panic!("reconcile work {} has no phys btree", w.0),
    }
}

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
    let phys = c::reconcile_work_id(bp.v.reconcile_phys() as u32);
    if phys != c::reconcile_work_id::RECONCILE_WORK_none {
        t.bit_mod_buffered(reconcile_work_phys_btree(phys), bp.k.p, insert)?;
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
fn backpointer_del(t: &TransAttempt<'_, '_>, btree: c::btree_id, pos: c::bpos) -> Result<(), BchError> {
    if !debug_params::backpointers_no_use_write_buffer() {
        t.bit_mod_buffered(btree, pos, false)?;
    } else {
        t.delete(btree, pos, UpdateTriggerFlags::empty())?;
    }
    t.commit(None, CommitFlags::NO_ENOSPC)
}

fn backpointers_maybe_flush(t: &TransAttempt<'_, '_>, visiting_k: BkeySC<'_>,
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
fn get_key<'i, 't>(t: &'i TransAttempt<'t, '_>, bp: BkeySC<'_, c::Backpointer>,
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

// ── fsck: check_btree_backpointers ───────────────────────────────────────

fn check_backpointer_has_valid_bucket(t: &TransAttempt<'_, '_>, k: BkeySC<'_>,
                                      last_flushed: &mut WbMaybeFlush) -> Result<(), BchError> {
    if k.k.key_type() != c::bch_bkey_type::KEY_TYPE_backpointer {
        return Ok(());
    }

    let fs = t.fs();
    let bucket = {
        let rcu = read_lock();
        fs.dev_rcu(k.k.p.inode as u32, &rcu).map(|ca| BackpointerPos::from_bpos(fs, k.k.p).bucket(ca))
    };

    let Some(bucket) = bucket else {
        backpointers_maybe_flush(t, k, last_flushed.borrow())?;

        if fsck_err!(t, id::backpointer_to_missing_device, "backpointer for missing device:\n{}",
                     k.to_text(fs))? {
            backpointer_del(t, c::btree_id::backpointers, k.k.p)?;
        }
        return Ok(());
    };

    let mut alloc_iter = BtreeIter::new(t, c::btree_id::alloc, bucket, BtreeIterFlags::empty());
    let alloc_k = alloc_iter.peek_slot(t)?.expect("a slot always has a key");

    if alloc_k.k.key_type() != c::bch_bkey_type::KEY_TYPE_alloc_v4 {
        backpointers_maybe_flush(t, k, last_flushed.borrow())?;

        let p = alloc_iter.pos();
        if fsck_err!(t, id::backpointer_to_missing_alloc, "backpointer for nonexistent alloc key: {}:{}:0\n{}",
                     { p.inode }, { p.offset }, k.to_text(fs))? {
            backpointer_del(t, c::btree_id::backpointers, k.k.p)?;
        }
    }
    Ok(())
}

fn check_btree_backpointers(fs: &Fs) -> Result<(), BchError> {
    let progress = Progress::recovery(fs, c"bch2_check_btree_backpointers", &[c::btree_id::backpointers], &[]);
    let mut last_flushed = WbMaybeFlush::new()?;

    let trans = crate::btree_trans!(fs);
    let mut iter = BtreeIter::new(&trans, c::btree_id::backpointers, POS_MIN, BtreeIterFlags::empty());
    iter.for_each_commit(&trans, None, CommitFlags::NO_ENOSPC, |t, iter, k| {
        progress.update(t, iter)?;
        check_backpointer_has_valid_bucket(t, k, &mut last_flushed)
    })
}

crate::recovery_pass!(bch2_check_btree_backpointers => check_btree_backpointers);

// ── fsck: an extent's backpointer ────────────────────────────────────────

fn bp_missing(t: &TransAttempt<'_, '_>, extent: BkeySC<'_>, bp: &mut BkeyI<c::Backpointer>,
              bp_found: BkeySC<'_>) -> Result<(), BchError> {
    let fs = t.fs();
    let mut buf = Printbuf::new();
    prt!(&mut buf, "missing backpointer\nfor:  {}", extent.to_text(fs));
    prt!(&mut buf, "\nwant: {}", BkeySC::<c::Backpointer>::from(&*bp).to_text(fs));
    if !bp_found.k.is_deleted() {
        prt!(&mut buf, "\ngot:  {}", bp_found.to_text(fs));
    }

    if fsck_err!(t, id::ptr_to_missing_backpointer, "{}", buf)? {
        bucket_backpointer_mod(t, extent, bp, true)?;
    }
    Ok(())
}

/// A pointer to a bucket @dev doesn't have counts as stale.
fn bkey_dev_ptr_stale(fs: &Fs, k: BkeySC<'_>, dev: u32) -> bool {
    let rcu = read_lock();
    let Some(ca) = fs.dev_rcu(dev, &rcu) else { return false };
    let Some(kp) = k.arm::<Ptrs>() else { return false };

    let stale = kp.ptrs(fs).any(|p| u32::from(p.dev()) == dev && ca.ptr_stale(p, &rcu).is_none_or(|s| s != 0));
    stale
}

fn drop_double_allocation(t: &TransAttempt<'_, '_>, btree: c::btree_id, level: u8, k: BkeySC<'_>, dev: u32)
    -> Result<(), BchError>
{
    t.result(unsafe {
        c::bch2_bkey_drop_device_and_update(t.raw(), btree, level.into(), k.into(), dev,
                                         c::bch_key_type_errors::KEY_TYPE_ERROR_double_allocation)
    })
}

/// One of two extents sharing physical space, and its logical range in the overlap.
struct DupExtent<'k> {
    btree:   c::btree_id,
    level:   u8,
    k:       BkeySC<'k>,
    overlap: core::ops::Range<u64>,
}

/// Just the overlapping part of each becomes a reflink_p of one shared reflink_v;
/// the rest of each stays an extent, split off when the reflink_p is inserted.
fn extents_to_reflink<'a>(t: &TransAttempt<'a, '_>, e1: &DupExtent<'_>, e2: &DupExtent<'_>)
    -> Result<(), BchError>
{
    let fs = t.fs();
    // as C's: a u32
    let overlap_size = (e1.overlap.end - e1.overlap.start) as u32;
    assert_eq!(u64::from(overlap_size), e2.overlap.end - e2.overlap.start, "overlaps of different sizes");

    let p1 = e1.k.k.p;
    let mut iter1 = BtreeIter::new_level(t, e1.btree, p1, e1.level.into(), BtreeIterFlags::INTENT);
    t.iter_traverse(&mut iter1)?;

    // With room for the reflink_p make_extent_indirect() makes it, in place
    let mut k1_mut = t.bkey_reassemble_room(e1.k, size_of::<BkeyI<c::ReflinkP>>())?;
    {
        let mut e = k1_mut.as_mut().arm::<Extents>().expect("an extent");
        e.cut_front_in_place(fs, spos(p1.inode, e1.overlap.start, p1.snapshot));
        e.cut_back_in_place(spos(p1.inode, e1.overlap.end, p1.snapshot));
    }

    make_extent_indirect(t, &iter1, &mut k1_mut, false)?;

    let r_v_idx = k1_mut.as_ref().arm::<c::ReflinkP>().expect("a reflink_p, as just made").v.idx();

    let p2 = e2.k.k.p;
    let mut iter2 = BtreeIter::new_level(t, e2.btree, p2, e2.level.into(), BtreeIterFlags::INTENT);
    t.iter_traverse(&mut iter2)?;

    let mut rp = t.bkey_alloc_typed::<c::ReflinkP>()?;
    let k = rp.k_mut();
    k.p = spos(p2.inode, e2.overlap.end, p2.snapshot);
    k.size = overlap_size;
    k.bversion = e2.k.k.bversion;
    rp.v_mut().set_idx(r_v_idx);

    t.update(&iter2, &rp, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)
}

fn check_bp_dup(t: &TransAttempt<'_, '_>, extent: BkeySC<'_>, bp: &mut BkeyI<c::Backpointer>,
                other_bp: BkeySC<'_, c::Backpointer>) -> Result<(), BchError> {
    let fs = t.fs();
    let dev = bp.k.p.inode as u32;
    let ov = *other_bp.v();
    let other_btree = c::btree_id(ov.btree_id.into());

    let mut other_extent_iter = BtreeIter::uninit();
    let other_extent = match get_key(t, other_bp, &mut other_extent_iter, BtreeIterFlags::empty(), None, false) {
        Err(e) if e.matches(bch_errcode::BCH_ERR_backpointer_to_overwritten_btree_node) =>
            return bp_missing(t, extent, bp, other_bp.untyped()),
        Err(e)      => return Err(e),
        Ok(None)    => return bp_missing(t, extent, bp, other_bp.untyped()),
        Ok(Some(k)) => k,
    };

    if bkey_dev_ptr_stale(fs, other_extent, dev) {
        return drop_double_allocation(t, other_btree, ov.level, other_extent, dev);
    }

    if extents_match(fs, extent, other_extent) {
        let mut msg = LogMsg::new(fs);
        prt!(&mut msg, "duplicate versions of same extent, deleting smaller\n{}\n{}",
             extent.to_text(fs), other_extent.to_text(fs));

        if other_extent.k.size <= extent.k.size {
            return drop_double_allocation(t, other_btree, ov.level, other_extent, dev);
        } else {
            drop_double_allocation(t, c::btree_id(bp.v.btree_id.into()), bp.v.level, extent, dev)?;
            return bp_missing(t, extent, bp, other_bp.untyped());
        }
    }

    let kill_replica_if_checksum_bad = |btree: c::btree_id, level: u8, k: BkeySC<'_>,
                                        other_btree: c::btree_id, other_k: BkeySC<'_>| {
        ret_to_result(unsafe {
            c::bch2_kill_replica_if_checksum_bad(t.raw(), btree, level.into(), k.into(), other_btree,
                                              other_k.into(), dev)
        }).map(|killed| killed != 0)
    };

    let btree = c::btree_id(bp.v.btree_id.into());
    if kill_replica_if_checksum_bad(other_btree, ov.level, other_extent, btree, extent)? {
        return bp_missing(t, extent, bp, other_bp.untyped());
    }
    if kill_replica_if_checksum_bad(btree, bp.v.level, extent, other_btree, other_extent)? {
        return Ok(());
    }

    // Both replicas verify clean: a physical region is referenced from two
    // distinct logical positions, possibly with partial overlap. This can
    // happen after btree topology recovery from media errors - convert the
    // overlapping portion to a reflink_v shared by two reflink_p's so neither
    // file loses data. Non-overlapping prefix/suffix of each extent stays as
    // a regular extent.
    //
    // Only valid for leaf extents; btree pointers can't be reflinked.
    let is_extent = |k: BkeySC<'_>| k.k.key_type() == c::bch_bkey_type::KEY_TYPE_extent;
    if is_extent(extent) && is_extent(other_extent) {
        let (bp_sub, other_bp_sub) = {
            let rcu = read_lock();
            let Some(ca) = fs.dev_rcu(dev, &rcu) else { return Ok(()) };
            (BackpointerPos::from_bpos(fs, bp.k.p).bucket_and_offset(ca).1,
             BackpointerPos::from_bpos(fs, other_bp.k.p).bucket_and_offset(ca).1)
        };

        // as C's: u32s
        let overlap_sub_start = bp_sub.max(other_bp_sub);
        let overlap_sub_end = bp_sub.wrapping_add(bp.v.bucket_len)
            .min(other_bp_sub.wrapping_add(ov.bucket_len));
        if overlap_sub_end <= overlap_sub_start {
            return Ok(()); // bps don't actually overlap
        }

        let overlap_len = u64::from(overlap_sub_end - overlap_sub_start);
        let k1_start = extent.k.start_offset() + u64::from(overlap_sub_start - bp_sub);
        let k2_start = other_extent.k.start_offset() + u64::from(overlap_sub_start - other_bp_sub);

        let mut buf = Printbuf::new();
        prt!(&mut buf, "duplicate extents pointing to overlapping space on dev {}, converting overlap to reflink\n{}\n{}",
             { bp.k.p.inode }, extent.to_text(fs), other_extent.to_text(fs));

        // Both files are damaged - sharing physical space means at least one
        // of them reads the other's bytes. The pos-ful report records against
        // the first inode; record the second explicitly (KEY_TYPE_extent only
        // lives in the extents btree, so both attribute):
        if inode_fsck_err!(t, extent.k.p, id::dup_extents_to_reflink, "{}", buf)? {
            t.result(unsafe { c::bch2_damage_record(t.raw(), other_extent.k.p, id::dup_extents_to_reflink) })?;
            extents_to_reflink(t,
                &DupExtent { btree, level: bp.v.level, k: extent, overlap: k1_start..k1_start + overlap_len },
                &DupExtent { btree: other_btree, level: ov.level, k: other_extent,
                             overlap: k2_start..k2_start + overlap_len })?;
        }
        return Ok(());
    }

    let mut msg = LogMsg::new(fs);
    prt!(&mut msg, "duplicate extents pointing to same space on dev {}\n{}\n{}",
         { bp.k.p.inode }, extent.to_text(fs), other_extent.to_text(fs));
    fs.throw(bch_errcode::BCH_ERR_fsck_repair_unimplemented)
}

/// Repairs as check_extents_to_backpointers does: makes a missing one, resolves
/// one that's someone else's.
pub fn check_bp_exists(t: &TransAttempt<'_, '_>, extent: BkeySC<'_>, bp: &mut BkeyI<c::Backpointer>,
                       mut last_flushed: WbFlush<'_>) -> Result<(), BchError> {
    let mut bp_iter = BtreeIter::new(t, bp.v.btree(), bp.k.p, BtreeIterFlags::empty());
    let bp_found = bp_iter.peek_slot(t)?.expect("a slot always has a key");

    match bp_found.arm::<c::Backpointer>() {
        None => {
            last_flushed.maybe_flush(t, extent)?;
            bp_missing(t, extent, bp, bp_found)
        }
        Some(found) if !backpointers_match(found.v(), &bp.v) => {
            last_flushed.maybe_flush(t, extent)?;
            check_bp_dup(t, extent, bp, found)
        }
        Some(_) => Ok(()),
    }
}

/// # Safety
/// @trans is in an attempt; @bp is a backpointer key, all of it;
/// @last_flushed is the caller's.
#[c_export]
pub unsafe fn bch2_check_bp_exists<'t>(trans: &'t BtreeTrans<'t>, extent: BkeySC<'_>,
                                       bp: *mut c::bkey_i_backpointer,
                                       last_flushed: *mut c::wb_maybe_flush) -> c_int {
    let t = unsafe { trans.attempt_in_progress() };
    // SAFETY: C's struct bkey_i_backpointer is a BkeyI<Backpointer>
    let bp = unsafe { &mut *bp.cast::<BkeyI<c::Backpointer>>() };
    let last_flushed = unsafe { WbFlush::from_raw(last_flushed) }.expect("a wb_maybe_flush");
    ret_to_c(check_bp_exists(&t, extent, bp, last_flushed))
}

// ── fsck: check_extents_to_backpointers ──────────────────────────────────
//
// Backpointers are checked by bucket, not one by one: each bucket's, of its
// generation, are summed against its alloc key's sector counts. More than
// it says, and they're checked against what they point to; not what it
// says, and the bucket goes in ca->bucket_backpointer_mismatch - and
// _empty, if there are none - for the walk of every extent and btree node
// pointer that follows, which checks the pointers into those buckets.

/// An alloc key's sector counts, or what its backpointers add up to.
#[derive(Default, PartialEq)]
struct BucketSectors {
    dirty:  u32,
    cached: u32,
    stripe: u32,
}

impl BucketSectors {
    fn of(a: &c::AllocV4) -> Self {
        Self { dirty: a.dirty_sectors, cached: a.cached_sectors, stripe: a.stripe_sectors }
    }

    fn add(&mut self, bp: &c::Backpointer) {
        use c::bch_data_type as dt;
        let counter = match c::bch_data_type(bp.data_type.into()) {
            dt::BCH_DATA_btree | dt::BCH_DATA_user    => &mut self.dirty,
            dt::BCH_DATA_cached                       => &mut self.cached,
            dt::BCH_DATA_stripe | dt::BCH_DATA_parity => &mut self.stripe,
            _ => return,
        };
        // as C's: u32s
        *counter = counter.wrapping_add(bp.bucket_len);
    }

    fn any_over(&self, a: &Self) -> bool {
        self.dirty > a.dirty || self.cached > a.cached || self.stripe > a.stripe
    }

    fn is_empty(&self) -> bool {
        self.dirty.wrapping_add(self.stripe).wrapping_add(self.cached) == 0
    }
}

/// If the backpointers are over, they're checked against what they point to, once
/// (@checked_bad), and the attempt restarted; if off, the bucket goes in the
/// bitmaps. Returns whether it did.
fn check_bucket_backpointer_mismatch(t: &TransAttempt<'_, '_>, alloc_k: BkeySC<'_>,
                                     mut last_flushed: WbFlush<'_>, checked_bad: &mut c::bpos)
    -> Result<bool, BchError>
{
    use c::bch_data_type as dt;

    let fs = t.fs();
    let a = alloc_to_v4(alloc_k);

    if matches!(c::bch_data_type(a.data_type.into()), dt::BCH_DATA_sb | dt::BCH_DATA_journal | dt::BCH_DATA_parity) {
        return Ok(false);
    }

    let Some(ca) = fs.dev_bucket_tryget_noerror(alloc_k.k.p) else { return Ok(false) };
    let bp_range = BackpointerPos::bucket_range(&ca, alloc_k.k.p);
    let before_bucket_gen = fs.version_upgrade_complete().0 <
        c::bcachefs_metadata_version::bcachefs_metadata_version_backpointer_bucket_gen.0;

    let mut sectors = BucketSectors::default();
    let mut nr_deletes = 0u32;
    let mut need_commit = false;

    let mut iter = BtreeIter::new(t, c::btree_id::backpointers, *bp_range.start(), BtreeIterFlags::empty());
    iter.for_each_max_norestart(t, *bp_range.end(), |_, bp_k| {
        let Some(bp) = bp_k.arm::<c::Backpointer>() else { return Ok(ControlFlow::Continue(())) };
        let v = *bp.v();

        if before_bucket_gen && (v.bucket_gen != a.generation || { v.flags } != 0) {
            backpointer_del(t, c::btree_id::backpointers, bp_k.k.p)?;
            nr_deletes += 1;

            if nr_deletes > 256 {
                t.commit(None, CommitFlags::NO_ENOSPC)?;
                t.trans().write_buffer_flush_sync()?;
                return Err(t.restart(bch_errcode::BCH_ERR_transaction_restart_write_buffer_flush));
            }

            need_commit = true;
            return Ok(ControlFlow::Continue(()));
        }

        if v.bucket_gen == a.generation {
            sectors.add(&v);
        }
        Ok(ControlFlow::<()>::Continue(()))
    })?;

    if need_commit {
        t.commit(None, CommitFlags::NO_ENOSPC)?;
    }

    let have = BucketSectors::of(&a);

    if sectors.any_over(&have) {
        if *checked_bad != alloc_k.k.p {
            *checked_bad = alloc_k.k.p;
            check_bucket_backpointers_to_extents(t.trans(), &ca, alloc_k.k.p, last_flushed)?;
            return Err(t.restart(bch_errcode::BCH_ERR_transaction_restart_nested));
        }

        backpointers_maybe_flush(t, alloc_k, last_flushed.reborrow())?;

        let p = alloc_k.k.p;
        let mut msg = LogMsg::new(fs);
        prt!(&mut msg, "backpointer sectors > bucket sectors, but found no bad backpointers\n\
                        bucket {}:{} data type ", { p.inode }, { p.offset });
        prt_data_type(&mut msg, c::bch_data_type(a.data_type.into()));
        prt!(&mut msg, ", counters\n");
        if sectors.dirty > have.dirty {
            prt!(&mut msg, "dirty: {} > {}\n", sectors.dirty, have.dirty);
        }
        if sectors.cached > have.cached {
            prt!(&mut msg, "cached: {} > {}\n", sectors.cached, have.cached);
        }
        if sectors.stripe > have.stripe {
            prt!(&mut msg, "stripe: {} > {}\n", sectors.stripe, have.stripe);
        }

        // as C's: whatever of the bucket it reads
        let mut iter = BtreeIter::new(t, c::btree_id::backpointers, *bp_range.start(), BtreeIterFlags::empty());
        let _ = iter.for_each_max_norestart(t, *bp_range.end(), |_, bp_k| {
            prt!(&mut msg, "{}\n", bp_k.to_text(fs));
            Ok(ControlFlow::<()>::Continue(()))
        });
        crate::warn_on!(true);
    }

    if sectors == have {
        return Ok(false);
    }

    // Post 1.14 upgrade, we assume that backpointers are mostly correct and
    // a sector count mismatch is probably due to a write buffer race.
    //
    // Pre upgrade, we expect all the buckets to be wrong, a write buffer
    // flush is pointless:
    if !before_bucket_gen {
        if c::bch_data_type(a.data_type.into()) == dt::BCH_DATA_btree {
            t.trans().unlock_long();
            unsafe { c::bch2_btree_interior_updates_flush(fs.as_ptr()) };
        }
        backpointers_maybe_flush(t, alloc_k, last_flushed)?;
    }

    ca.bucket_backpointer_mismatch.set(&ca, alloc_k.k.p.offset)?;
    if sectors.is_empty() {
        ca.bucket_backpointer_empty.set(&ca, alloc_k.k.p.offset)?;
    }
    Ok(true)
}

/// Schedules check_extents_to_backpointers - ratelimited from copygc, until 1/128
/// of the device's buckets are short.
fn check_one_bucket_backpointer_mismatch(trans: &BtreeTrans<'_>, ca: &c::bch_dev, bucket: u64,
                                         copygc: bool, mut last_flushed: WbFlush<'_>)
    -> Result<(), BchError>
{
    let fs = ca.fs();

    // Outside the restart loop: after checking a bucket with too many
    // backpointers against their extents, check_bucket_backpointer_mismatch()
    // restarts - reset this and it does that forever.
    let mut checked_bad = POS_MAX;
    let had_mismatch = lockrestart_do(trans, |t| {
        // Not BTREE_ITER_cached: scrub calls this once per bucket for a whole
        // device (bch2_move_data_phys() fills in the gaps between
        // backpointers), and a cached iterator would leave a key cache entry
        // behind for every one of them. Uncached still sees a dirty cached key
        // - alloc is a cached btree, so bch2_btree_iter_flags() gives us
        // BTREE_ITER_with_key_cache, which consults an existing entry without
        // creating one.
        let mut alloc_iter = BtreeIter::new(t, c::btree_id::alloc, pos(ca.dev_idx.into(), bucket),
                                            BtreeIterFlags::empty());
        let k = alloc_iter.peek_slot(t)?.expect("a slot always has a key");
        check_bucket_backpointer_mismatch(t, k, last_flushed.reborrow(), &mut checked_bad)
    })?;

    if !had_mismatch {
        return Ok(());
    }

    let nr = ca.bucket_backpointer_mismatch.nr();
    let allowed = if copygc { ca.mi.nbuckets >> 7 } else { 0 };

    let mut buf = Printbuf::new();
    log_msg_start(&mut buf, ca.name());
    prt!(&mut buf, "Detected missing backpointers in bucket {bucket}, now have {nr}/{} with missing\n",
         { ca.mi.nbuckets });

    let flags = if nr < allowed {
        c::bch_run_recovery_pass_flags::RUN_RECOVERY_PASS_ratelimit
    } else {
        c::bch_run_recovery_pass_flags::from_bits_retain(0)
    };
    // as C's: the message says whether it's scheduled
    let _ = passes::run_explicit(fs, &mut buf,
                                 c::bch_recovery_pass::BCH_RECOVERY_PASS_check_extents_to_backpointers, flags);

    static RATELIMIT: Ratelimit = Ratelimit::new();
    if !RATELIMIT.suppress(fs, c"bch2_check_bucket_backpointer_mismatch") {
        print_str(fs, LogLevel::ERR, &mut buf);
    }
    Ok(())
}

/// # Safety
/// @trans isn't in an attempt; @last_flushed is the caller's.
#[c_export]
pub unsafe fn bch2_check_bucket_backpointer_mismatch<'t>(trans: &'t BtreeTrans<'t>, ca: &c::bch_dev,
                                                         bucket: u64, copygc: bool,
                                                         last_flushed: *mut c::wb_maybe_flush) -> c_int {
    let last_flushed = unsafe { WbFlush::from_raw(last_flushed) }.expect("a wb_maybe_flush");
    ret_to_c(check_one_bucket_backpointer_mismatch(trans, ca, bucket, copygc, last_flushed))
}

struct ExtentsToBpState {
    bp_range:     RangeInclusive<c::bpos>,
    last_flushed: WbMaybeFlush,
}

/// A bucket with no backpointers has them all put back without looking.
fn check_extent_to_backpointers(t: &TransAttempt<'_, '_>, s: &mut ExtentsToBpState, btree: c::btree_id,
                                level: u32, k: BkeySC<'_>) -> Result<(), BchError> {
    let fs = t.fs();
    let Some(kp) = k.arm::<Ptrs>() else { return Ok(()) };

    for d in kp.ptrs_decoded(fs) {
        let mut bp = extent_ptr_to_bp(fs, btree, level, k, &d);
        let dev = u32::from(d.p.ptr.dev());

        if dev == c::BCH_SB_MEMBER_INVALID {
            if d.p.has_ec {
                check_bp_exists(t, k, &mut bp, s.last_flushed.borrow())?;
            }
            continue;
        }

        let empty = {
            let rcu = read_lock();
            let Some(ca) = fs.dev_rcu(dev, &rcu) else { continue };

            if d.p.ptr.cached() != 0 && ca.ptr_stale(&d.p.ptr, &rcu).is_none_or(|s| s != 0) {
                continue;
            }

            let b = ca.ptr_bucket(&d.p.ptr);
            if !ca.bucket_backpointer_mismatch.test(b) {
                continue;
            }
            ca.bucket_backpointer_empty.test(b)
        };

        if !s.bp_range.contains(&bp.k.p) {
            continue;
        }

        if !empty {
            check_bp_exists(t, k, &mut bp, s.last_flushed.borrow())?;
        } else {
            bucket_backpointer_mod(t, k, &mut bp, true)?;
        }
    }
    Ok(())
}

/// The root's key is checked a level above the root. Returns the root's level.
fn check_btree_root_to_backpointers(t: &TransAttempt<'_, '_>, s: &mut ExtentsToBpState, btree: c::btree_id)
    -> Result<u32, BchError>
{
    let fs = t.fs();
    let root_level = fs.btree_id_root(btree.0).expect("a btree with a root").c.level;

    let mut iter = BtreeIter::new_level(t, btree, POS_MIN, root_level.into(), BtreeIterFlags::empty());
    let b = iter.peek_node(t)?.expect("a btree's root node");

    if !fs.btree_id_root(b.c.btree_id.into()).is_some_and(|root| core::ptr::eq(root, b)) {
        return Err(t.restart(bch_errcode::BCH_ERR_transaction_restart_lock_root_race));
    }

    let level = u32::from(b.c.level);
    check_extent_to_backpointers(t, s, btree, level + 1, BkeySC::from(b.key_i()))?;
    Ok(level)
}

fn check_extents_to_backpointers_pass(fs: &Fs, trans: &BtreeTrans<'_>, s: &mut ExtentsToBpState)
    -> Result<(), BchError>
{
    let progress = Progress::recovery_masks(fs, c"extents_to_backpointers", crate::BTREE_HAS_DATA_PTRS_MASK, !0);
    // extents btree updates always require a disk res passed in
    let res = DiskReservation::new(fs);

    for btree in (0..fs.btree_id_nr_alive()).map(c::btree_id) {
        let depth = if btree.has_data_ptrs() { 0 } else { 1 };

        let mut root_level = 0;
        commit_do(trans, Some(&res), CommitFlags::NO_ENOSPC, |t| {
            root_level = check_btree_root_to_backpointers(t, s, btree)?;
            Ok(())
        })?;

        for level in (depth..=root_level).rev() {
            let mut iter = BtreeIter::new_level(trans, btree, POS_MIN, level, BtreeIterFlags::PREFETCH);
            iter.for_each_commit(trans, Some(&res), CommitFlags::NO_ENOSPC, |t, iter, k| {
                progress.update(t, iter)?;
                s.last_flushed.inc();
                check_extent_to_backpointers(t, s, btree, level, k)
            })?;
        }
    }
    Ok(())
}

/// Buckets with none don't count - theirs are recreated without looking. A
/// btree_ptr has no min_key, so might.
fn backpointer_node_has_missing(fs: &Fs, k: BkeySC<'_>) -> bool {
    use c::bch_bkey_type as t;

    match k.k.key_type() {
        t::KEY_TYPE_btree_ptr_v2 => {
            let rcu = read_lock();
            let mut p = k.arm::<c::BtreePtrV2>().expect("a btree_ptr_v2").v().min_key;

            while p.inode <= k.k.p.inode && p.inode < u64::from(fs.sb.nr_devices) {
                if let Some(ca) = fs.dev_rcu(p.inode as u32, &rcu) {
                    let bucket = BackpointerPos::from_bpos(fs, p).bucket(ca);
                    let nbuckets = ca.mi.nbuckets;
                    let mismatch = ca.bucket_backpointer_mismatch.words(&rcu);
                    let empty = ca.bucket_backpointer_empty.words(&rcu);

                    let next = AtomicBitmap::from_words(mismatch)
                        .next_andnot(AtomicBitmap::from_words(empty),
                                     bucket.offset.min(nbuckets) as usize, nbuckets as usize);

                    if let Some(next) = next {
                        let next = pos(bucket.inode, next as u64);
                        if *BackpointerPos::bucket_range(ca, next).end() <= k.k.p {
                            return true;
                        }
                    }
                }
                p = spos(p.inode + 1, 0, 0);
            }
            false
        }
        t::KEY_TYPE_btree_ptr => true,
        _ => false,
    }
}

fn btree_node_get_and_pin(t: &TransAttempt<'_, '_>, pins: &BtreeCachePins<'_>, k: &BkeyI, btree: c::btree_id,
                          level: u32) -> Result<(), BchError> {
    let mut iter = BtreeIter::new_level(t, btree, k.k.p, level, BtreeIterFlags::empty());
    if let Some(b) = iter.peek_node(t)? {
        pins.node(b);
    }
    Ok(())
}

/// All prefetched first, so they're read in parallel. Returns where the pinned
/// range ends.
fn pin_backpointer_nodes_with_missing(fs: &Fs, trans: &BtreeTrans<'_>, pins: &BtreeCachePins<'_>,
                                      start: c::bpos) -> Result<c::bpos, BchError> {
    let node_size = i64::from(fs.opts.btree_node_size);
    let mut tmp = BkeyBuf::new();

    pins.unpin();

    let mut mem_may_pin = pins.budget() as i64;
    let mut iter = BtreeIter::new_level(trans, c::btree_id::backpointers, start, 1, BtreeIterFlags::PREFETCH);
    iter.for_each(trans, |t, iter, k| {
        if !backpointer_node_has_missing(fs, k) {
            return Ok(ControlFlow::Continue(()));
        }

        mem_may_pin -= node_size;
        if mem_may_pin <= 0 {
            return Ok(ControlFlow::Break(()));
        }

        tmp.reassemble(k);
        iter.prefetch_child(t, tmp.as_ref())?;
        Ok(ControlFlow::Continue(()))
    })?;

    let mut end = SPOS_MAX;
    let mut pinned = SPOS_MAX;
    let mut mem_may_pin = pins.budget() as i64;
    let mut iter = BtreeIter::new_level(trans, c::btree_id::backpointers, start, 1, BtreeIterFlags::PREFETCH);
    iter.for_each(trans, |t, _, k| {
        if !backpointer_node_has_missing(fs, k) {
            return Ok(ControlFlow::Continue(()));
        }

        mem_may_pin -= node_size;
        if mem_may_pin <= 0 {
            end = pinned;
            return Ok(ControlFlow::Break(()));
        }

        tmp.reassemble(k);
        btree_node_get_and_pin(t, pins, tmp.as_ref(), c::btree_id::backpointers, 0)?;
        pinned = tmp.as_ref().k.p;
        Ok(ControlFlow::Continue(()))
    })?;

    Ok(end)
}

fn check_extents_to_backpointers(fs: &Fs) -> Result<(), BchError> {
    let trans = crate::btree_trans!(fs);
    let pins = fs.btree_cache_pins();
    let mut s = ExtentsToBpState { bp_range: POS_MIN..=SPOS_MAX, last_flushed: WbMaybeFlush::new()? };
    let mut checked_bad = POS_MAX;

    let mut iter = BtreeIter::new(&trans, c::btree_id::alloc, POS_MIN, BtreeIterFlags::PREFETCH);
    iter.for_each(&trans, |t, _, k| {
        passes::cancelled(fs)?;
        check_bucket_backpointer_mismatch(t, k, s.last_flushed.borrow(), &mut checked_bad)?;
        Ok(ControlFlow::<()>::Continue(()))
    })?;

    let (mut nr_buckets, mut nr_mismatches, mut nr_empty) = (0u64, 0u64, 0u64);
    for ca in fs.members() {
        nr_buckets += ca.mi.nbuckets;
        nr_mismatches += ca.bucket_backpointer_mismatch.nr();
        nr_empty += ca.bucket_backpointer_empty.nr();
    }

    #[cfg(not(CONFIG_BCACHEFS_DEBUG))]
    if nr_mismatches == 0 {
        return Ok(());
    }

    crate::bch_info!(fs, "scanning for missing backpointers in {}/{nr_buckets} buckets, \
                          {nr_empty} buckets with no backpointers", nr_mismatches.wrapping_sub(nr_empty));

    let ret = (|| {
        let mut start = POS_MIN;
        loop {
            let end = pin_backpointer_nodes_with_missing(fs, &trans, &pins, start)?;

            if start == POS_MIN && end != SPOS_MAX {
                crate::bch_info!(fs, "bch2_check_extents_to_backpointers(): alloc info does not fit in ram, \
                                      running in multiple passes with {} nodes per pass",
                                 pins.budget() / u64::from(fs.opts.btree_node_size));
            }

            if start != POS_MIN || end != SPOS_MAX {
                crate::bch_verbose!(fs, "check_extents_to_backpointers(): {start}-{end}");
            }

            s.bp_range = start..=end;
            check_extents_to_backpointers_pass(fs, &trans, &mut s)?;

            if end == SPOS_MAX {
                return Ok(());
            }
            start = end.successor();
        }
    })();

    for ca in fs.members() {
        ca.bucket_backpointer_mismatch.free();
        ca.bucket_backpointer_empty.free();
    }
    ret
}

crate::recovery_pass!(bch2_check_extents_to_backpointers => check_extents_to_backpointers);

// ── fsck: check_backpointers_to_extents ──────────────────────────────────

fn check_one_backpointer(t: &TransAttempt<'_, '_>, range: &RangeInclusive<c::bbpos>,
                         bp: BkeySC<'_, c::Backpointer>, last_flushed: WbFlush<'_>)
    -> Result<(), BchError>
{
    let v = bp.v();
    if !range.contains(&c::bbpos { btree: c::btree_id(v.btree_id.into()), pos: v.pos }) {
        return Ok(());
    }

    let mut iter = BtreeIter::uninit();
    match get_key(t, bp, &mut iter, BtreeIterFlags::empty(), Some(last_flushed), true) {
        Err(e) if e.matches(bch_errcode::BCH_ERR_backpointer_to_overwritten_btree_node) => Ok(()),
        r => r.map(|_| ()),
    }
}

/// Inside the caller's attempt: a restart is the caller's.
fn check_bucket_backpointers_to_extents(trans: &BtreeTrans<'_>, ca: &c::bch_dev, bucket: c::bpos,
                                        last_flushed: WbFlush<'_>) -> Result<(), BchError> {
    let restart_count = trans.restart_count();
    let range = BackpointerPos::bucket_range(ca, bucket);

    BpScanIter::new(c::btree_id::backpointers, *range.start(), None)
        .for_each(trans, *range.end(), last_flushed, |t, bp, last_flushed| {
            check_one_backpointer(t, &(c::bbpos::MIN..=c::bbpos::MAX), bp, last_flushed)
        })?;

    // make sure bad backpointers that were deleted are visible
    trans.write_buffer_flush_sync()?;
    if trans.restart_count() != restart_count {
        return Err(BchError::from(bch_errcode::BCH_ERR_transaction_restart_nested));
    }
    Ok(())
}

fn check_backpointers_to_extents_pass(fs: &Fs, trans: &BtreeTrans<'_>, range: RangeInclusive<c::bbpos>)
    -> Result<(), BchError>
{
    let mut last_flushed = WbMaybeFlush::new()?;
    let btrees = [c::btree_id::backpointers, c::btree_id::stripe_backpointers];
    let progress = Progress::recovery(fs, c"backpointers_to_extents", &btrees, &[]);

    for btree in btrees {
        BpScanIter::new(btree, POS_MIN, Some(&progress))
            .for_each(trans, POS_MAX, last_flushed.borrow(), |t, bp, last_flushed| {
                check_one_backpointer(t, &range, bp, last_flushed)
            })?;
    }
    Ok(())
}

fn check_backpointers_to_extents(fs: &Fs) -> Result<(), BchError> {
    let trans = crate::btree_trans!(fs);
    let pins = fs.btree_cache_pins();
    let leaves = 1 << c::btree_id::extents.0 | 1 << c::btree_id::reflink.0;
    let mut start = c::bbpos::MIN;

    loop {
        let end = pins.pin(&trans, leaves, !0, start..=c::bbpos::MAX)?;

        if start == c::bbpos::MIN && end != c::bbpos::MAX {
            crate::bch_verbose!(fs, "bch2_check_backpointers_to_extents(): extents do not fit in ram, \
                                     running in multiple passes with {} nodes per pass",
                                pins.budget() / u64::from(fs.opts.btree_node_size));
        }

        if start != c::bbpos::MIN || end != c::bbpos::MAX {
            crate::bch_verbose!(fs, "check_backpointers_to_extents(): {start}-{end}");
        }

        check_backpointers_to_extents_pass(fs, &trans, start..=end)?;

        if end == c::bbpos::MAX {
            return Ok(());
        }
        start = end.successor();
    }
}

crate::recovery_pass!(bch2_check_backpointers_to_extents => check_backpointers_to_extents);

// ── Bucket bitmaps ───────────────────────────────────────────────────────

impl c::bucket_bitmap {
    pub fn nr(&self) -> u64 {
        self.bits.nr.get()
    }

    /// Empty if it isn't allocated.
    pub fn words<'a>(&'a self, rcu: &'a ReadGuard) -> &'a [AtomicULong] {
        self.bits.buckets.read(rcu)
    }

    pub fn test(&self, b: u64) -> bool {
        self.bits.buckets.with(|w| AtomicBitmap::from_words(w).test(b as usize))
    }

    /// The first allocates the bitmap.
    pub fn set(&self, ca: &c::bch_dev, b: u64) -> Result<(), BchError> {
        let mut l = self.bits.lock();
        let w = l.published();

        if w.buckets.with(|words| words.is_empty()) {
            w.buckets.rebuild(bits_to_longs(ca.mi.nbuckets as usize), |_| ())
                .or_else(|_| ca.fs().throw(bch_errcode::BCH_ERR_ENOMEM_backpointer_mismatches_bitmap))?;
        }

        if !w.buckets.with(|words| AtomicBitmap::from_words(words).test_and_set(b as usize)) {
            *w.nr.place() += 1;
        }
        Ok(())
    }

    /// Only if it's allocated: a copy, the old words freed after a grace period.
    pub fn resize(&self, ca: &c::bch_dev, nbuckets: u64) -> Result<(), BchError> {
        let mut l = self.bits.lock();
        let w = l.published();

        if w.buckets.with(|old| old.is_empty()) {
            return Ok(());
        }

        w.buckets.rebuild(bits_to_longs(nbuckets as usize), |new| {
            w.buckets.with(|old| {
                for (n, o) in new.iter_mut().zip(old) {
                    *n.get_mut() = o.load(Ordering::Relaxed);
                }
            })
        })
        .or_else(|_| ca.fs().throw(bch_errcode::BCH_ERR_ENOMEM_backpointer_mismatches_bitmap))
    }

    /// The words are freed after a grace period.
    pub fn free(&self) {
        let mut l = self.bits.lock();
        let w = l.published();

        w.buckets.clear();
        w.nr.set(0);
    }
}

#[c_export]
pub fn bch2_bucket_bitmap_test(b: &c::bucket_bitmap, bucket: u64) -> bool {
    b.test(bucket)
}

#[c_export]
pub fn bch2_bucket_bitmap_resize(ca: &c::bch_dev, b: &c::bucket_bitmap, nbuckets: u64) -> c_int {
    ret_to_c(b.resize(ca, nbuckets))
}

#[c_export]
pub fn bch2_bucket_bitmap_free(b: &c::bucket_bitmap) {
    b.free()
}
