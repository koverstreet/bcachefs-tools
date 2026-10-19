// SPDX-License-Identifier: GPL-2.0

//! Reflink: extents shared by more than one file, or one file at more than
//! one offset. An extent made indirect moves to BTREE_ID_reflink as a
//! reflink_v (or indirect_inline_data), with a refcount; the extents btree
//! maps it with reflink_p keys, each a range of the reflink btree's offsets
//! from its idx.
//!
//! A reflink_p's references: one on every indirect extent its refd range -
//! idx - front_pad to idx + size + back_pad - overlaps. Its trigger takes them
//! on insert, an indirect extent at a time, and widens the pads to take in all
//! of each one it found; overwriting it walks the refd range again and drops
//! them. The pads are what makes that the same set: an indirect extent can be
//! split later (copygc, reconcile), and a fragment past the live range would
//! otherwise keep a reference no reflink_p drops.
//!
//! An indirect extent whose refcount reaches zero is deleted by its own
//! trigger. gc recounts the references from scratch (ReflinkGc).
//!
//! A refd range with nothing there in the reflink btree is a fsck error,
//! repaired by narrowing the pads, or flagging the reflink_p REFLINK_P_ERROR
//! over the missing part of its live range - which stops the error from
//! being reported again, and is cleared if the indirect extent is found.
//!
//! An indirect extent is a key of the Indirect family, BKEY_TYPE_indirect: a
//! key narrows to one with arm::<Indirect>() - bkey_is_indirect() - and its
//! refcount is read and edited through it (BkeyVIndirect), in whichever form
//! it's held.

use crate::btree::bkey::prelude::*;
use crate::alloc::buckets::DiskReservation;
use crate::btree::bkey::{bkey_eq, bkey_family, bkey_ge, bkey_gt, bkey_lt, pos, BkeyI, BkeyS, BkeySC,
                         BkeyV, BkeyVal, BkeyVMut, POS_MAX, POS_MIN};
use crate::btree::bkey_methods::TriggerOp;
use crate::btree::iter::{is_restart, lockrestart_do, BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags,
                         TransAttempt, TransBkey, UpdateTriggerFlags};
use crate::c;
use crate::data::extents::Extents;
use crate::data::io_misc::fpunch_at;
use crate::errcode::{bch_errcode, err_ptr, ret_to_c, BchError};
use crate::fs::Fs;
use crate::init::error::{id, BkeyValidate};
use crate::snapshots::subvolume;
use crate::types::{GenRadix, Le32, Le64};
use crate::util::ffi::Opaque;
use crate::util::kernel::fatal_signal_pending;
use crate::util::vstructs::{cast_plain, cast_plain_mut, Validated};
use crate::util::Printbuf;
use crate::{bch_err, bch_err_fn, bkey_fsck_err_on, fsck_err_flags, fsck_err_on, inode, inode_fsck_err, prt};
use core::cell::UnsafeCell;
use core::ffi::{c_int, c_uint};
use core::mem::{offset_of, size_of};
use core::ops::Range;
use core::sync::atomic::{AtomicI32, AtomicUsize, Ordering::Relaxed};
use cstruct_macros::c_export;

/// A value with a refcount - of a BKEY_TYPE_indirect type, or Indirect,
/// either of them: where in it the refcount is.
pub trait IndirectBkey: BkeyVal {
    fn refcount_offset(k: BkeySC<'_, Self>) -> usize;
}

macro_rules! refcount_at {
    ($($ty:ident),*) => { $(
        impl IndirectBkey for c::$ty<Validated> {
            fn refcount_offset(_: BkeySC<'_, Self>) -> usize {
                offset_of!(c::$ty, repr.refcount)
            }
        }
    )* };
}
refcount_at!(ReflinkV, IndirectInlineData);

bkey_family!(Indirect);

macro_rules! indirect_bkey {
    ($(($name:ident, $nr:literal, $($rest:tt)*)),* $(,)?) => {
        impl IndirectBkey for Indirect {
            fn refcount_offset(k: BkeySC<'_, Indirect>) -> usize {
                match k.typed() {
                    $(IndirectTyped::$name(k) => IndirectBkey::refcount_offset(k),)*
                }
            }
        }
    };
}
c::BCH_BKEY_TYPES!(cstruct_macros::xmacro_select [BKEY_TYPE_indirect => indirect_bkey]);

/// The refcount of an indirect extent, in whichever form it's held: any BkeyV
/// whose value is an IndirectBkey - armed to Indirect, or to one of them.
pub trait BkeyVIndirect: BkeyV {
    /// bkey_refcount_c().
    fn refcount(&self) -> &Le64
    where Self::V: IndirectBkey
    {
        let k = self.as_sc();
        k.val_bytes().get(<Self::V as IndirectBkey>::refcount_offset(k)..)
            .and_then(cast_plain)
            .unwrap_or_else(|| panic!("{}: {} bytes, shorter than validate passes", k.k, k.val_bytes().len()))
    }

    /// bkey_refcount().
    fn refcount_mut(&mut self) -> &mut Le64
    where Self: BkeyVMut, Self::V: IndirectBkey
    {
        let k = *self.k();
        let at = <Self::V as IndirectBkey>::refcount_offset(self.as_sc());
        self.val_bytes_mut().get_mut(at..)
            .and_then(cast_plain_mut)
            .unwrap_or_else(|| panic!("{k}: {} bytes, shorter than validate passes", k.val_bytes()))
    }
}

impl<K: BkeyV> BkeyVIndirect for K {}

impl c::bkey {
    /// The type a direct extent of its type is, made indirect: deleted for a
    /// type that has none, as bkey_type_to_indirect() returns 0.
    pub fn indirect_type(&self) -> c::bch_bkey_type {
        use c::bch_bkey_type as t;

        match self.key_type() {
            t::KEY_TYPE_extent      => t::KEY_TYPE_reflink_v,
            t::KEY_TYPE_inline_data => t::KEY_TYPE_indirect_inline_data,
            _                       => t::KEY_TYPE_deleted,
        }
    }
}

// ── gc ───────────────────────────────────────────────────────────────────

/// An indirect extent, as gc counts the reflink_p references to it.
struct GcEntry {
    offset:   u64,
    size:     u32,
    refcount: AtomicI32,
}

// SAFETY: zeroed is offset and size 0 and a refcount of 0 - an AtomicI32 is
// an i32 - which pin-init doesn't say of atomics
unsafe impl ::pin_init::Zeroable for GcEntry {}

/*
 * gc's table of indirect extents: gc counts every reflink_p's references into
 * it, and at its end checks the reflink btree's refcounts against it. Rust's
 * alone, as GenRadix<GcEntry> - C reaches it through the functions below.
 *
 * Who uses it when - gc's phase, c->gc.pos, is what orders them:
 *
 *  - Built by gc_reflink_start(), in gc's own thread, gc.lock held for write
 *    and gc.pos still not_running. Nothing else reads it then: the readers are
 *    gc triggers, and commits run those only for keys gc_visited() - false
 *    while not_running. gc_pos_set() is a seqcount write, which the read in
 *    gc_visited() pairs with: a reader that sees gc running sees the table as
 *    built.
 *
 *  - Read while gc marks: by gc's thread, and by commits' gc triggers, at
 *    once. Only an entry's refcount changes - atomically; offset, size and the
 *    entries' number are as built.
 *
 *  - Its number zeroed by gc_reflink_done() - while commits' gc triggers may
 *    still read it, which then find nothing to count, as C's did. And the
 *    table freed, by bch2_gc_free(), once gc.pos is back to not_running,
 *    under mark_lock for write.
 *
 * Not fenced: the free from commits' gc triggers. A commit runs those outside
 * mark_lock, after gc_visited(), so one that saw gc running can still be
 * counting into the table when it's freed. Nothing commits during
 * check_allocations today, so it isn't reached; nothing stops it either. The
 * fix is a grace period between not_running and the free - a dedicated SRCU,
 * read around commit's gc trigger run, gc_visited() rechecked inside. Not
 * btree.trans.barrier, which every transaction already holds: gc would wait
 * for it holding gc.lock, which a btree update waits for holding it.
 *
 * So it's shared through &Fs, and its build and free - the two that need it
 * to themselves - are unsafe: their callers vouch for the phase.
 */
pub(crate) struct ReflinkGc {
    table: UnsafeCell<GenRadix<GcEntry>>,
    nr:    AtomicUsize,
}

// SAFETY: the protocol above - shared, the table is only read: genradix
// lookups are safe from any thread, and refcount is atomic
unsafe impl Send for ReflinkGc {}
unsafe impl Sync for ReflinkGc {}

impl ReflinkGc {
    pub(crate) fn new() -> Self {
        ReflinkGc { table: UnsafeCell::new(::pin_init::zeroed()), nr: AtomicUsize::new(0) }
    }

    fn table(&self) -> &GenRadix<GcEntry> {
        // SAFETY: shared, it's only read - as above
        unsafe { &*self.table.get() }
    }

    /// # Safety
    /// gc's thread, with nothing else using the table: before gc.pos leaves
    /// not_running, or after it's back.
    #[allow(clippy::mut_from_ref)]
    unsafe fn table_mut(&self) -> &mut GenRadix<GcEntry> {
        unsafe { &mut *self.table.get() }
    }

    fn nr(&self) -> usize {
        // Relaxed: set before gc.pos is, whose seqcount publishes it; read
        // after gc_reflink_done() zeroes it, it's 0, and nothing is counted
        self.nr.load(Relaxed)
    }
}

impl Drop for ReflinkGc {
    fn drop(&mut self) {
        self.table.get_mut().free();
    }
}

/// The first entry from @r_idx that may cover @idx - or @p's range up to the
/// next one, missing, if none does: counted, for gc, and its end the next
/// @idx. As gc_trigger_reflink_p_segment().
fn gc_segment(
    t:            &TransAttempt<'_, '_>,
    p:            BkeySC<'_, c::ReflinkP>,
    idx:          &mut u64,
    flags:        UpdateTriggerFlags,
    r_idx:        usize,
    check_repair: bool,
) -> Result<(), BchError> {
    let gc = &t.fs().reflink_gc;
    let add = if flags.contains(UpdateTriggerFlags::OVERWRITE) { -1 } else { 1 };
    let mut next_idx = p.refd().end;

    if r_idx < gc.nr() {
        let r = gc.table().get(r_idx).expect("a gc table entry below its number");
        next_idx = next_idx.min(r.offset - r.size as u64);

        if *idx >= next_idx {
            if flags.contains(UpdateTriggerFlags::GC) {
                assert!(r.refcount.fetch_add(add, Relaxed) + add >= 0,
                        "reflink gc refcount underflow at {}", r.offset);
            }
            *idx = r.offset;
            return Ok(());
        }
    }

    if check_repair {
        indirect_extent_missing_error(t, p, *idx..next_idx, false)?;
    }

    *idx = next_idx;
    Ok(())
}

/// Count @p's references over @refs into gc's table: as reflink_p_gc_idx().
/// With @check_repair, what isn't there is the missing indirect extent error.
fn gc_idx(
    t:            &TransAttempt<'_, '_>,
    p:            BkeySC<'_, c::ReflinkP>,
    refs:         Range<u64>,
    flags:        UpdateTriggerFlags,
    check_repair: bool,
) -> Result<(), BchError> {
    let gc = &t.fs().reflink_gc;
    let mut idx = refs.start;

    /* the first entry ending past @idx */
    let (mut l, mut r) = (0, gc.nr());
    while l < r {
        let m = l + (r - l) / 2;
        if gc.table().get(m).expect("a gc table entry below its number").offset <= idx {
            l = m + 1;
        } else {
            r = m;
        }
    }

    while idx < refs.end {
        gc_segment(t, p, &mut idx, flags, l, check_repair)?;
        l += 1;
    }
    Ok(())
}

/// Check @k's refcount against gc's count: as bch2_gc_write_reflink_key().
fn gc_write_reflink_key(
    t:    &TransAttempt<'_, '_>,
    iter: &BtreeIter<'_>,
    k:    BkeySC<'_>,
    idx:  &mut usize,
) -> Result<(), BchError> {
    let fs = t.fs();
    let Some(k) = k.arm::<Indirect>() else { return Ok(()) };
    let refcount = k.refcount();
    let table = fs.reflink_gc.table();

    let r = loop {
        match table.get(*idx) {
            Some(r) if r.offset < k.k.p.offset => *idx += 1,
            r                                   => break r,
        }
    };

    let Some(r) = r.filter(|r| r.offset == k.k.p.offset && r.size == k.k.size) else {
        bch_err!(fs, "unexpected inconsistency walking reflink table at gc finish");
        return fs.throw(bch_errcode::BCH_ERR_EINVAL_reflink_gc_table_mismatch);
    };

    let r_refcount = r.refcount.load(Relaxed) as u32;

    if fsck_err_on!(t, r_refcount as u64 != refcount.get(), id::reflink_v_refcount_wrong,
                    "reflink key has wrong refcount:\n{}\nshould be {}", k.to_text(fs), r_refcount)? {
        let mut new = t.bkey_make_mut_noupdate(k)?;

        if r_refcount == 0 {
            new.k_mut().type_ = c::bch_bkey_type::KEY_TYPE_deleted.0 as u8;
        } else {
            *new.refcount_mut() = (r_refcount as u64).into();
        }
        t.update(iter, &new, UpdateTriggerFlags::empty())?;
    }
    Ok(())
}

/// gc's end: the reflink btree's refcounts, checked against its counts.
fn gc_reflink_done(fs: &Fs) -> Result<(), BchError> {
    let trans = crate::btree_trans!(fs);
    let mut iter = BtreeIter::new(&trans, c::btree_id::reflink, POS_MIN, BtreeIterFlags::PREFETCH);
    let mut idx = 0;

    let ret = iter.for_each_commit(&trans, None, CommitFlags::NO_ENOSPC,
                                   |t, iter, k| gc_write_reflink_key(t, iter, k, &mut idx));
    fs.reflink_gc.nr.store(0, Relaxed);
    ret
}

#[c_export]
pub fn bch2_gc_reflink_done(c: &Opaque<c::bch_fs>) -> c_int {
    ret_to_c(gc_reflink_done(Fs::from_c(c)))
}

/// gc's start: a table entry for every indirect extent, its count zero.
fn gc_reflink_start(fs: &Fs) -> Result<(), BchError> {
    let gc = &fs.reflink_gc;
    // SAFETY: gc's thread, gc.pos still not_running - see ReflinkGc
    let table = unsafe { gc.table_mut() };
    let trans = crate::btree_trans!(fs);
    let mut iter = BtreeIter::new(&trans, c::btree_id::reflink, POS_MIN, BtreeIterFlags::PREFETCH);
    let mut nr = 0;

    gc.nr.store(0, Relaxed);

    let ret = iter.for_each(&trans, |_, _, k| {
        if k.arm::<Indirect>().is_none() {
            return Ok(());
        }

        let r = table.get_or_alloc(nr, c::GFP_KERNEL);
        nr += 1;
        gc.nr.store(nr, Relaxed);

        let r = r.map_err(|_| fs.err(bch_errcode::BCH_ERR_ENOMEM_gc_reflink_start))?;
        r.offset   = k.k.p.offset;
        r.size     = k.k.size;
        r.refcount = AtomicI32::new(0);
        Ok(())
    });

    bch_err_fn!(fs, ret)
}

#[c_export]
pub fn bch2_gc_reflink_start(c: &Opaque<c::bch_fs>) -> c_int {
    ret_to_c(gc_reflink_start(Fs::from_c(c)))
}

/// For bch2_gc_free(): the table, freed.
///
/// # Safety
/// gc.pos is back to not_running, and nothing is still counting into the
/// table - which isn't guaranteed: see ReflinkGc.
#[c_export]
pub unsafe fn bch2_gc_reflink_free(c: &Opaque<c::bch_fs>) {
    let gc = &Fs::from_c(c).reflink_gc;

    // SAFETY: the caller's
    unsafe { gc.table_mut() }.free();
}

// ── reflink pointers ─────────────────────────────────────────────────────

impl BkeySC<'_, c::ReflinkP> {
    /// What it maps, in the reflink btree's offsets.
    fn live(&self) -> Range<u64> {
        let idx = self.v().idx();
        idx..idx + self.k.size as u64
    }

    /// What it holds references on: live(), and its pads.
    fn refd(&self) -> Range<u64> {
        let (live, v) = (self.live(), self.v());
        live.start - v.front_pad.get() as u64..live.end + v.back_pad.get() as u64
    }
}

fn reflink_p_validate(v: &BkeyValidate<'_, '_>) -> Result<(), BchError> {
    let p = v.k.arm::<c::ReflinkP>().expect("a reflink_p").v();

    bkey_fsck_err_on!(v, p.idx() < p.front_pad.get() as u64, id::reflink_p_front_pad_bad,
                      "idx < front_pad ({} < {})", p.idx(), p.front_pad.get())
}

#[c_export]
pub fn bch2_reflink_p_validate(c: &Opaque<c::bch_fs>, k: BkeySC<'_>, from: &c::bkey_validate_context)
    -> c_int
{
    ret_to_c(reflink_p_validate(&BkeyValidate { fs: Fs::from_c(c), k, from }))
}

#[cold]
#[c_export]
pub fn bch2_reflink_p_to_text(out: &mut Printbuf, _c: &Opaque<c::bch_fs>, k: BkeySC<'_>) {
    let p = k.arm::<c::ReflinkP>().expect("a reflink_p").v();

    write!(out, "idx {} front_pad {} back_pad {}", p.idx(), p.front_pad.get(), p.back_pad.get());

    if p.error() {
        write!(out, " error");
    }

    if p.may_update_options() {
        write!(out, " may_update_opts");
    }
}

#[allow(unreachable_code, unused_variables, unused_mut)]
#[c_export]
pub fn bch2_reflink_p_merge(_c: &Opaque<c::bch_fs>, mut l: BkeyS<'_>, r: BkeySC<'_>) -> bool {
    /*
     * Disabled for now, the triggers code needs to be reworked for merging
     * of reflink pointers to work:
     */
    return false;

    let (l_idx, l_error) = {
        let l = l.arm::<c::ReflinkP>().expect("a reflink_p");
        (l.v().idx(), l.v().error())
    };
    let r_p = r.arm::<c::ReflinkP>().expect("a reflink_p").v();

    if l_idx + l.k.size as u64 != r_p.idx() {
        return false;
    }

    if l_error != r_p.error() {
        return false;
    }

    l.k.resize(l.k.size + r.k.size);
    true
}

// ── indirect extents ─────────────────────────────────────────────────────

fn reflink_v_validate(v: &BkeyValidate<'_, '_>) -> Result<(), BchError> {
    bkey_fsck_err_on!(v, bkey_gt(v.k.k.p, pos(0, c::ReflinkP::IDX_MAX)), id::reflink_v_pos_bad,
                      "indirect extent above maximum position 0:{}", c::ReflinkP::IDX_MAX)?;

    crate::data::ptrs::bkey_ptrs_validate(v)
}

#[c_export]
pub fn bch2_reflink_v_validate(c: &Opaque<c::bch_fs>, k: BkeySC<'_>, from: &c::bkey_validate_context)
    -> c_int
{
    ret_to_c(reflink_v_validate(&BkeyValidate { fs: Fs::from_c(c), k, from }))
}

#[cold]
#[c_export]
pub fn bch2_reflink_v_to_text(out: &mut Printbuf, c: &Opaque<c::bch_fs>, k: BkeySC<'_>) {
    write!(out, "refcount: {} ", k.arm::<c::ReflinkV>().expect("a reflink_v").refcount().get());

    crate::data::ptrs::bkey_ptrs_to_text(out, Some(Fs::from_c(c)), k);
}

/*
 * Currently disabled, needs to be debugged:
 */
#[cfg(any())]
#[c_export]
pub fn bch2_reflink_v_merge(c: &Opaque<c::bch_fs>, l: BkeyS<'_>, r: BkeySC<'_>) -> bool {
    l.as_sc().arm::<c::ReflinkV>().expect("a reflink_v").refcount() ==
        r.arm::<c::ReflinkV>().expect("a reflink_v").refcount() &&
        crate::data::extents::extent_merge(Fs::from_c(c), l, r)
}

// ── indirect inline data ─────────────────────────────────────────────────

#[c_export]
pub fn bch2_indirect_inline_data_validate(_c: &Opaque<c::bch_fs>, _k: BkeySC<'_>,
                                          _from: &c::bkey_validate_context) -> c_int {
    0
}

#[cold]
#[c_export]
pub fn bch2_indirect_inline_data_to_text(out: &mut Printbuf, _c: &Opaque<c::bch_fs>, k: BkeySC<'_>) {
    let data = &k.val_bytes()[size_of::<c::IndirectInlineData>()..];

    write!(out, "refcount {} datalen {}: ", k.arm::<c::IndirectInlineData>().expect("indirect inline data").refcount().get(), data.len());
    for b in &data[..data.len().min(32)] {
        write!(out, "{b:02x}");
    }
}

// ── lookup ───────────────────────────────────────────────────────────────

/// A reflink_p to an indirect extent that's there after all: its error flag
/// cleared.
fn indirect_extent_not_missing(t: &TransAttempt<'_, '_>, p: BkeySC<'_, c::ReflinkP>, should_commit: bool)
    -> Result<(), BchError>
{
    let mut new = t.bkey_make_mut_noupdate(p)?;

    new.v_mut().set_error(false);
    t.insert(c::btree_id::extents, new, UpdateTriggerFlags::NORUN)?;

    if !should_commit {
        return Ok(());
    }

    t.commit(None, CommitFlags::NO_ENOSPC)?;
    Err(t.restart(bch_errcode::BCH_ERR_transaction_restart_nested))
}

/// @p refers to @missing, which the reflink btree has nothing in: an fsck
/// error. Fixed by narrowing @p's pads, if it's only those that are missing,
/// or else by flagging @p's missing part REFLINK_P_ERROR.
fn indirect_extent_missing_error(
    t:             &TransAttempt<'_, '_>,
    p:             BkeySC<'_, c::ReflinkP>,
    missing:       Range<u64>,
    should_commit: bool,
) -> Result<(), BchError> {
    let fs = t.fs();

    if p.v().error() {
        return Ok(());
    }

    let (live, refd) = (p.live(), p.refd());

    assert!(refd.start <= missing.start && missing.end <= refd.end,
            "missing range {missing:?} outside the reflink_p's references {refd:?}");

    /* Wrapping: a missing range in the front pad starts before live.start */
    let mut missing_pos = p.k.start_pos();
    missing_pos.offset = missing_pos.offset.wrapping_add(missing.start.wrapping_sub(live.start));

    let mut buf = Printbuf::new();
    write!(buf, "pointer to missing indirect extent in ");
    t.result(unsafe { c::bch2_inum_offset_err_msg_trans_norestart(t.raw(), buf.as_raw(), 0, missing_pos) })?;

    write!(buf, "-{}\n", missing_pos.offset.wrapping_add(missing.end - missing.start) << 9);
    prt!(&mut buf, "{}", p.to_text(fs));
    write!(buf, "\nmissing reflink btree range {}-{}", missing.start, missing.end);

    if inode_fsck_err!(t, p.k.p, id::reflink_p_to_missing_reflink_v, "{}", buf.as_str())? {
        let mut new = t.bkey_make_mut_noupdate(p)?;

        /*
         * Is the missing range not actually needed?
         *
         * p.v->idx refers to the data that we actually want, but if the
         * indirect extent we point to was bigger, front_pad and back_pad
         * indicate the range we took a reference on.
         */

        if missing.end <= live.start {
            new.v_mut().front_pad = ((live.start - missing.end) as u32).into();
        } else if missing.start >= live.end {
            new.v_mut().back_pad = ((missing.start - live.end) as u32).into();
        } else {
            let mut new_start = new.k().start_pos();
            let mut new_end   = new.k().p;

            new_start.offset += missing.start.saturating_sub(live.start);
            new_end.offset   -= live.end.saturating_sub(missing.end);

            let overlap = "the missing range overlaps the live range";
            new.as_mut().arm::<Extents>().expect("a reflink_p is an extent")
                .cut_front(fs, new_start).expect(overlap)
                .cut_back(new_end).expect(overlap);

            new.v_mut().set_error(true);
        }

        t.insert(c::btree_id::extents, new, UpdateTriggerFlags::NORUN)?;

        if should_commit {
            t.commit_lazy(CommitFlags::NO_ENOSPC)?;
        }
    }

    Ok(())
}

/// The indirect extent @p points to, at @offset_into_extent into @p - which
/// becomes the offset into what's found. For the read path, which doesn't
/// expect to have to do a transaction commit, and for triggers, which should
/// not be doing one: only a repair, with @should_commit, commits.
pub fn lookup_indirect_extent<'a, 'k>(
    t:                  &'k TransAttempt<'a, '_>,
    iter:               &'k mut BtreeIter<'a>,
    offset_into_extent: &mut i64,
    p:                  BkeySC<'_, c::ReflinkP>,
    should_commit:      bool,
    iter_flags:         BtreeIterFlags,
) -> Result<BkeySC<'k>, BchError> {
    let refd = p.refd();
    let reflink_offset = p.v().idx().wrapping_add_signed(*offset_into_extent);

    assert!(refd.contains(&reflink_offset),
            "offset {} into the reflink_p, outside its references {refd:?}", *offset_into_extent);

    *iter = BtreeIter::new(t, c::btree_id::reflink, pos(0, reflink_offset), iter_flags);
    let k = iter.peek_slot(t)?.expect("peek_slot() returns a key");

    if k.arm::<Indirect>().is_none() {
        let missing_end = k.k.p.offset.min(refd.end);
        assert!(reflink_offset != missing_end);

        indirect_extent_missing_error(t, p, reflink_offset..missing_end, should_commit)?;
    } else if p.v().error() {
        indirect_extent_not_missing(t, p, should_commit)?;
    }

    *offset_into_extent = reflink_offset.wrapping_sub(k.k.start_offset()) as i64;
    Ok(k)
}

/// For C's read path: lookup_indirect_extent(), an error as an ERR_PTR().
#[c_export]
pub fn bch2_lookup_indirect_extent<'t>(
    trans:              &'t BtreeTrans<'t>,
    iter:               &mut BtreeIter<'t>,
    offset_into_extent: &mut i64,
    p:                  BkeySC<'_>,
    should_commit:      bool,
    iter_flags:         c_uint,
) -> c::bkey_s_c {
    // SAFETY: C calls this in an attempt
    let t = unsafe { trans.attempt_in_progress() };
    let p = p.arm::<c::ReflinkP>().expect("a reflink_p");

    match lookup_indirect_extent(&t, iter, offset_into_extent, p, should_commit,
                                 BtreeIterFlags::from_bits_retain(iter_flags)) {
        Ok(k)  => k.into(),
        Err(e) => c::bkey_s_c {
            k: err_ptr(e),
            v: core::ptr::null_mut(),
        },
    }
}

// ── reflink pointer trigger ──────────────────────────────────────────────

/// The indirect extent of @p's range at @idx: its refcount taken, or dropped,
/// and on insert, @p's pads widened to cover it. @idx moves to its end.
fn trans_trigger_reflink_p_segment(
    t:     &TransAttempt<'_, '_>,
    p:     &mut BkeyI<c::ReflinkP>,
    idx:   &mut u64,
    flags: UpdateTriggerFlags,
) -> Result<(), BchError> {
    let fs = t.fs();
    let overwrite = flags.contains(UpdateTriggerFlags::OVERWRITE);
    let mut offset_into_extent = idx.wrapping_sub(p.v.idx()) as i64;
    let mut iter = BtreeIter::uninit();
    let k = lookup_indirect_extent(t, &mut iter, &mut offset_into_extent, BkeySC::from(&*p), false,
                                   BtreeIterFlags::INTENT)?;
    *idx = k.k.p.offset;

    let Some(k) = k.arm::<Indirect>() else {
        return if overwrite { Ok(()) } else { fs.throw(bch_errcode::BCH_ERR_missing_indirect_extent) };
    };
    let refcount = k.refcount();

    if refcount.get() == 0 && overwrite {
        fsck_err_flags!(t, POS_MIN, c::bch_fsck_flags::FSCK_CAN_IGNORE, id::reflink_refcount_underflow,
                        "indirect extent refcount underflow while marking\n{}\n{}",
                        p.to_text(fs), k.to_text(fs))?;
        return Ok(());
    }

    let mut new = t.bkey_make_mut_noupdate(k)?;

    if flags.contains(UpdateTriggerFlags::INSERT) {
        let front_pad = p.v.idx().saturating_sub(new.k().start_offset())
            .max(p.v.front_pad.get().into());
        let back_pad  = new.k().p.offset.saturating_sub(BkeySC::from(&*p).live().end)
            .max(p.v.back_pad.get().into());

        assert!(front_pad.max(back_pad) <= u32::MAX.into(), "reflink_p pads {front_pad}, {back_pad} past u32");
        p.v.front_pad = (front_pad as u32).into();
        p.v.back_pad  = (back_pad as u32).into();
    }

    let refcount = new.refcount_mut();
    *refcount = if overwrite { refcount.get() - 1 } else { refcount.get() + 1 }.into();

    iter.set_pos_to_extent_start();
    t.update(&iter, &new, UpdateTriggerFlags::empty())
}

/// @p's references, taken or dropped - transactionally, a segment at a time,
/// and for gc, counted in its table.
fn trigger_reflink_p_key(t: &TransAttempt<'_, '_>, p: &mut BkeyI<c::ReflinkP>, flags: UpdateTriggerFlags)
    -> Result<(), BchError>
{
    let refd = BkeySC::from(&*p).refd();
    let mut idx = refd.start;

    if flags.contains(UpdateTriggerFlags::TRANSACTIONAL) {
        while idx < refd.end {
            trans_trigger_reflink_p_segment(t, p, &mut idx, flags)?;
        }
    }

    if flags.contains(UpdateTriggerFlags::GC) {
        gc_idx(t, BkeySC::from(&*p), idx..refd.end, flags, false)?;
    }
    Ok(())
}

fn set_pads(k: &mut BkeyS<'_>, front_pad: Le32, back_pad: Le32) {
    let mut p = k.as_s().arm::<c::ReflinkP>().expect("a reflink_p");
    let v = p.v_mut();
    v.front_pad = front_pad;
    v.back_pad  = back_pad;
}

/*
 * Each key is walked as a copy. Inserting, the segments widen its pads as they
 * go, and check offsets against them - C writes them into @op.new itself, cast
 * from the const key it walks - and the pads the walk ends with are written
 * back to @op.new.
 */
fn trigger_reflink_p(t: &TransAttempt<'_, '_>, op: &mut TriggerOp<'_>) -> Result<(), BchError> {
    if op.flags.contains(UpdateTriggerFlags::TRANSACTIONAL | UpdateTriggerFlags::INSERT) {
        set_pads(&mut op.new, 0.into(), 0.into());
    }

    let mut new_pads = None;
    op.run_overwrite_then_insert(|k, flags| {
        let mut p = BkeyI::<c::ReflinkP>::from_key(k).expect("a reflink_p");

        trigger_reflink_p_key(t, &mut p, flags)?;
        if flags.contains(UpdateTriggerFlags::INSERT) {
            new_pads = Some((p.v.front_pad, p.v.back_pad));
        }
        Ok(())
    })?;

    if let Some((front_pad, back_pad)) = new_pads {
        set_pads(&mut op.new, front_pad, back_pad);
    }
    Ok(())
}

crate::key_trigger!(bch2_trigger_reflink_p => trigger_reflink_p);

#[c_export]
pub fn bch2_reflink_p_check_repair<'t>(
    trans:  &'t BtreeTrans<'t>,
    _iter:  &mut BtreeIter<'t>,
    _btree: c::btree_id,
    _level: c_uint,
    k:      BkeySC<'_>,
) -> c_int {
    let p = k.arm::<c::ReflinkP>().expect("a reflink_p");

    // SAFETY: C calls check_repair in an attempt
    let t = unsafe { trans.attempt_in_progress() };
    ret_to_c(gc_idx(&t, p, p.refd(), UpdateTriggerFlags::empty(), true))
}

// ── indirect extent trigger ──────────────────────────────────────────────

/// An indirect extent whose refcount reaches zero is deleted, not inserted.
fn check_indirect_extent_deleting(op: &mut TriggerOp<'_>) {
    if op.flags.contains(UpdateTriggerFlags::INSERT) &&
       BkeySC::from(&op.new).arm::<Indirect>().expect("an indirect extent").refcount().get() == 0 {
        op.new.k.type_ = c::bch_bkey_type::KEY_TYPE_deleted.0 as u8;
        op.new.k.size  = 0;
        op.new.k.set_val_bytes(0);
        op.flags -= UpdateTriggerFlags::INSERT;
    }
}

fn trigger_reflink_v(t: &TransAttempt<'_, '_>, op: &mut TriggerOp<'_>) -> Result<(), BchError> {
    if op.flags.contains(UpdateTriggerFlags::TRANSACTIONAL | UpdateTriggerFlags::INSERT) {
        check_indirect_extent_deleting(op);
    }

    t.result(unsafe { c::bch2_trigger_extent(t.raw(), op.to_raw()) })
}

crate::key_trigger!(bch2_trigger_reflink_v => trigger_reflink_v);

fn trigger_indirect_inline_data(_t: &TransAttempt<'_, '_>, op: &mut TriggerOp<'_>) -> Result<(), BchError> {
    check_indirect_extent_deleting(op);
    Ok(())
}

crate::key_trigger!(bch2_trigger_indirect_inline_data => trigger_indirect_inline_data);

// ── create ───────────────────────────────────────────────────────────────

/// Make @orig, at @extent_iter, indirect: its data moves to a new indirect
/// extent in the reflink btree, and @orig becomes a reflink_p to it, in
/// place.
///
/// # Safety
/// @orig is in transaction memory - its update points at it - with room for
/// a reflink_p.
unsafe fn __make_extent_indirect(
    t:                               &TransAttempt<'_, '_>,
    extent_iter:                     &BtreeIter<'_>,
    orig:                            *mut c::bkey_i,
    reflink_p_may_update_opts_field: bool,
) -> Result<(), BchError> {
    let fs = t.fs();
    // SAFETY: the caller's
    let orig = unsafe { &mut *BkeyI::from_raw_mut(orig) };

    if orig.k.key_type() == c::bch_bkey_type::KEY_TYPE_inline_data {
        fs.check_set_feature(c::bch_sb_feature::BCH_FEATURE_reflink_inline_data);
    }

    /*
     * A non-degraded extent carries no bch_extent_reconcile entry - for a
     * data extent that's fine, reconcile re-derives the io options from the
     * inode each pass. But once it's indirect there's no inode to derive
     * from, so snapshot the inode's options onto the reflink_v now;
     * otherwise reconcile would reconcile it against the filesystem
     * defaults instead. (If the source extent already carries the entry,
     * or the inode's options match the defaults, this is a no-op.) The
     * allocation below reserves room for the entry + invalid-device padding.
     */
    let mut opts = core::mem::MaybeUninit::<c::bch_inode_opts>::uninit();
    t.result(unsafe {
        c::bch2_bkey_get_io_opts(t.raw(), core::ptr::null_mut(), BkeySC::from(&*orig).into(), opts.as_mut_ptr())
    })?;

    let mut reflink_iter = BtreeIter::new(t, c::btree_id::reflink, POS_MAX, BtreeIterFlags::INTENT);
    reflink_iter.peek_prev(t)?;

    /*
     * XXX: we're assuming that 56 bits will be enough for the life of the
     * filesystem: we need to implement wraparound, with a cursor in the
     * logged ops btree:
     */
    if bkey_ge(reflink_iter.pos(), pos(0, c::ReflinkP::IDX_MAX - orig.k.size as u64)) {
        return Err(c::ENOSPC.into());
    }

    let r_v_buf_u64s = orig.k.u64s as u32 + 2 + c::BCH_REPLICAS_MAX;
    let mut r_v = t.bkey_alloc(r_v_buf_u64s)?;

    let k = r_v.k_mut();
    *k = c::bkey::new();
    k.type_    = orig.k.indirect_type().0 as u8;
    k.p        = reflink_iter.pos();
    k.resize(orig.k.size);
    k.bversion = orig.k.bversion;
    k.set_val_bytes(size_of::<Le64>() + orig.k.val_bytes());

    /* The refcount, zero, then the extent's value */
    let (refcount, val) = r_v.val_bytes_mut().split_at_mut(size_of::<Le64>());
    refcount.fill(0);
    val.copy_from_slice(BkeySC::from(&*orig).val_bytes());

    // SAFETY: written by get_io_opts(), which succeeded
    let opts = unsafe { opts.assume_init_ref() };
    r_v.set_needs_reconcile(t, None, opts, c::set_needs_reconcile_ctx::SET_NEEDS_RECONCILE_other, 0)?;

    t.update(&reflink_iter, &r_v, UpdateTriggerFlags::empty())?;

    /* orig, rewritten in place as the reflink_p - in the room the caller gave it */
    let r_v_start = r_v.k().start_offset();

    orig.k.type_ = c::bch_bkey_type::KEY_TYPE_reflink_p.0 as u8;
    orig.k.set_val_bytes(size_of::<c::ReflinkP>());
    // SAFETY: the room the caller vouched for
    let orig = unsafe { &mut *BkeyI::from_raw_mut(orig.as_ffi_mut()) };
    (&mut *orig).val_bytes_mut().fill(0);

    let r_p = orig.arm::<c::ReflinkP>().expect("a reflink_p, as just made");
    r_p.v.set_idx(r_v_start);

    if reflink_p_may_update_opts_field {
        r_p.v.set_may_update_options(true);
    }

    // SAFETY: the caller's - @orig is in transaction memory
    unsafe { t.update_key(extent_iter, orig, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE) }
}

/// As __make_extent_indirect(), on a key with the room.
pub fn make_extent_indirect<'a>(
    t:                               &TransAttempt<'a, '_>,
    extent_iter:                     &BtreeIter<'a>,
    orig:                            &mut TransBkey<'a, '_>,
    reflink_p_may_update_opts_field: bool,
) -> Result<(), BchError> {
    assert!(orig.as_u64s().len() * size_of::<u64>() >= size_of::<BkeyI<c::ReflinkP>>(),
            "no room in {} for a reflink_p", orig.k());

    // SAFETY: in transaction memory, as a TransBkey is, with the room
    unsafe { __make_extent_indirect(t, extent_iter, orig.as_ptr(), reflink_p_may_update_opts_field) }
}

/// For C's backpointers: __make_extent_indirect().
///
/// # Safety
/// As __make_extent_indirect()'s.
#[c_export]
pub unsafe extern "C" fn bch2_make_extent_indirect<'t>(
    trans:                           &'t BtreeTrans<'t>,
    extent_iter:                     &mut BtreeIter<'t>,
    orig:                            *mut c::bkey_i,
    reflink_p_may_update_opts_field: bool,
) -> c_int {
    // SAFETY: C calls this in an attempt
    let t = unsafe { trans.attempt_in_progress() };

    ret_to_c(unsafe { __make_extent_indirect(&t, extent_iter, orig, reflink_p_may_update_opts_field) })
}

// ── remap ────────────────────────────────────────────────────────────────

/// Point @dst_inum's range at @src_inum's data, as remap_file_range: each
/// source extent made indirect, if it isn't already, and a reflink_p to it put
/// in the destination - and what the source has no data for, punched there.
/// How much was remapped - or, if none was, the error.
#[allow(clippy::too_many_arguments)]
pub fn remap_range(
    fs:                          &Fs,
    dst_inum:                    c::subvol_inum,
    dst_offset:                  u64,
    src_inum:                    c::subvol_inum,
    src_offset:                  u64,
    remap_sectors:               u64,
    mut new_i_size:              u64,
    i_sectors_delta:             &mut i64,
    may_change_src_io_path_opts: bool,
) -> Result<u64, BchError> {
    let dst_start = pos(dst_inum.inum, dst_offset);
    let src_start = pos(src_inum.inum, src_offset);
    let dst_end   = pos(dst_inum.inum, dst_offset + remap_sectors);
    let src_end   = pos(src_inum.inum, src_offset + remap_sectors);
    let reflink_p_may_update_opts_field = fs.request_incompat_feature(
        c::bcachefs_metadata_version::bcachefs_metadata_version_reflink_p_may_update_opts).is_ok();

    let Some(_write_ref) = fs.write_ref_tryget(c::bch_write_ref::BCH_WRITE_REF_reflink) else {
        return fs.throw(bch_errcode::BCH_ERR_erofs_no_writes);
    };

    fs.check_set_feature(c::bch_sb_feature::BCH_FEATURE_reflink);

    let trans = crate::btree_trans!(fs);
    let mut src_iter = BtreeIter::new(&trans, c::btree_id::extents, src_start, BtreeIterFlags::INTENT);
    let mut dst_iter = BtreeIter::new(&trans, c::btree_id::extents, dst_start, BtreeIterFlags::INTENT);
    let mut ret = Ok(());

    while (ret.is_ok() || ret.as_ref().is_err_and(is_restart)) && bkey_lt(dst_iter.pos(), dst_end) {
        let t = trans.begin();

        ret = (|| -> Result<(), BchError> {
            if fatal_signal_pending() {
                return Err(c::EINTR.into());
            }

            src_iter.set_snapshot(subvolume::get_snapshot(&t, src_inum.subvol as u32)?);
            dst_iter.set_snapshot(subvolume::get_snapshot(&t, dst_inum.subvol as u32)?);

            if dst_inum.inum < src_inum.inum {
                /* Avoid some lock cycle transaction restarts */
                dst_iter.traverse(&t)?;
            }

            let dst_done = dst_iter.pos().offset - dst_start.offset;
            let src_want = pos(src_start.inode, src_start.offset + dst_done);
            src_iter.set_pos(src_want);

            /*
             * The next source extent with data, as a copy: with room to be
             * made a reflink_p in place, and not borrowing src_iter, which
             * moves before we're done with it.
             */
            let src = src_iter.find_max_norestart(&t, src_end, |_, k| {
                Ok(!k.extent_is_unwritten(fs) && k.k.extent_is_data())
            })?
            .map(|k| t.bkey_reassemble_room(k, size_of::<BkeyI<c::ReflinkP>>()))
            .transpose()?;
            if src.is_none() && bkey_ge(src_iter.pos(), src_end) {
                src_iter.set_pos(src_end);
            }

            if bkey_lt(src_want, src_iter.pos()) {
                let end = dst_end.offset.min(dst_iter.pos().offset + src_iter.pos().offset - src_want.offset);
                return fpunch_at(&trans, &mut dst_iter, dst_inum, end, Some(&mut *i_sectors_delta));
            }

            let mut src = src.expect("source data: the source iterator didn't move past it");

            if src.k().key_type() != c::bch_bkey_type::KEY_TYPE_reflink_p {
                src_iter.set_pos_to_extent_start();

                make_extent_indirect(&t, &src_iter, &mut src, reflink_p_may_update_opts_field)?;

                assert!(src.k().key_type() == c::bch_bkey_type::KEY_TYPE_reflink_p);
            }

            let src_p = src.as_ref().arm::<c::ReflinkP>().expect("a reflink_p");
            let mut dst_p = BkeyI::<c::ReflinkP>::new();

            dst_p.v.set_idx(src_p.v.idx() + (src_want.offset - src_p.k.start_offset()));

            if reflink_p_may_update_opts_field &&
               may_change_src_io_path_opts &&
               src_p.v.may_update_options() {
                dst_p.v.set_may_update_options(true);
            }

            dst_p.k.p = dst_iter.pos();
            dst_p.k.resize((src_p.k.p.offset - src_want.offset)
                           .min(dst_end.offset - dst_iter.pos().offset) as u32);

            let res = DiskReservation::new(fs);
            t.result(unsafe {
                c::bch2_extent_update(t.raw(), dst_inum, dst_iter.raw_mut(), dst_p.as_ffi_mut(),
                                      dst_p.k.u64s as u32, res.as_mut_ptr(), new_i_size, i_sectors_delta,
                                      true, 0, core::ptr::null_mut())
            })
        })();
    }

    assert!(ret.is_err() || bkey_eq(dst_iter.pos(), dst_end),
            "remap ended at {}, not {}", dst_iter.pos(), dst_end);
    assert!(!bkey_gt(dst_iter.pos(), dst_end), "remap ended at {}, past {}", dst_iter.pos(), dst_end);

    let dst_done = dst_iter.pos().offset - dst_start.offset;
    new_i_size = new_i_size.min(dst_iter.pos().offset << 9);

    let ret2 = lockrestart_do(&trans, |t| {
        let mut inode_iter = BtreeIter::uninit();
        let mut inode_u = inode::__peek(t, &mut inode_iter, dst_inum, BtreeIterFlags::INTENT,
                                        Some(c"bch2_remap_range"))?;

        if inode_u.bi_size < new_i_size {
            inode_u.bi_size = new_i_size;
            inode::write(t, &mut inode_iter, &mut inode_u)?;
            t.commit(None, CommitFlags::NO_ENOSPC)?;
        }
        Ok(())
    });

    match dst_done {
        0 => ret.and(ret2).map(|()| 0),
        n => Ok(n),
    }
}

#[c_export]
#[allow(clippy::too_many_arguments)]
pub fn bch2_remap_range(
    c:                           &Opaque<c::bch_fs>,
    dst_inum:                    c::subvol_inum,
    dst_offset:                  u64,
    src_inum:                    c::subvol_inum,
    src_offset:                  u64,
    remap_sectors:               u64,
    new_i_size:                  u64,
    i_sectors_delta:             &mut i64,
    may_change_src_io_path_opts: bool,
) -> i64 {
    match remap_range(Fs::from_c(c), dst_inum, dst_offset, src_inum, src_offset, remap_sectors,
                      new_i_size, i_sectors_delta, may_change_src_io_path_opts) {
        Ok(done) => done as i64,
        Err(e)   => -(e.raw() as i64),
    }
}
