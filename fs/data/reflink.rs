// SPDX-License-Identifier: GPL-2.0

//! Reflink: indirect extents - reflink_v, indirect_inline_data - in
//! BTREE_ID_reflink, refcounted, and the reflink_p keys in the extents btree
//! that point to them. data/reflink.c, as it's converted.

use crate::btree::bkey::{BkeyI, BkeyITypedMut, BkeySC, BkeySCTyped, POS_MIN};
use crate::btree::iter::{BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags, TransAttempt, UpdateTriggerFlags};
use crate::c;
use crate::errcode::{bch_errcode, ret_to_c, BchError};
use crate::fs::Fs;
use crate::init::error::id;
use crate::types::{GenRadix, Le64};
use crate::util::ffi::Opaque;
use crate::{bch_err, bch_err_fn, fsck_err_on};
use core::cell::UnsafeCell;
use core::ffi::c_int;
use core::sync::atomic::{AtomicI32, AtomicUsize, Ordering::Relaxed};
use cstruct_macros::c_export;

impl c::bkey {
    /// Whether it's an indirect extent - what a reflink_p points to:
    /// bkey_is_indirect(), and reflink.c's bkey_extent_is_reflink_data().
    pub fn is_indirect(&self) -> bool {
        matches!(self.key_type(),
                 c::bch_bkey_type::KEY_TYPE_reflink_v | c::bch_bkey_type::KEY_TYPE_indirect_inline_data)
    }

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

impl<'a> BkeySC<'a> {
    /// An indirect extent's refcount: bkey_refcount_c(). None for any other
    /// key.
    pub fn refcount(self) -> Option<&'a Le64> {
        match self.typed() {
            BkeySCTyped::reflink_v(r)            => Some(&r.v().refcount),
            BkeySCTyped::indirect_inline_data(d) => Some(&d.v().refcount),
            _                                    => None,
        }
    }
}

impl BkeyI {
    /// refcount(), to edit: bkey_refcount().
    pub fn refcount_mut(&mut self) -> Option<&mut Le64> {
        match self.typed_mut() {
            BkeyITypedMut::reflink_v(r)            => Some(&mut r.v_mut().refcount),
            BkeyITypedMut::indirect_inline_data(d) => Some(&mut d.v_mut().refcount),
            _                                      => None,
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
    p:            BkeySC<'_>,
    idx:          &mut u64,
    flags:        UpdateTriggerFlags,
    r_idx:        usize,
    check_repair: bool,
) -> Result<(), BchError> {
    let gc = &t.fs().reflink_gc;
    let pv = p.arm::<c::ReflinkP>().expect("a reflink_p").v();
    let add = if flags.contains(UpdateTriggerFlags::OVERWRITE) { -1 } else { 1 };
    let mut next_idx = pv.idx() + p.k.size as u64 + pv.back_pad.get() as u64;

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
        t.result(unsafe {
            c::bch2_indirect_extent_missing_error(t.raw(), p.to_raw(), *idx, next_idx, false)
        })?;
    }

    *idx = next_idx;
    Ok(())
}

/// Count @p's references from @idx to @end into gc's table: as
/// reflink_p_gc_idx(). With @check_repair, what isn't there is the missing
/// indirect extent error.
fn gc_idx(
    t:            &TransAttempt<'_, '_>,
    p:            BkeySC<'_>,
    mut idx:      u64,
    end:          u64,
    flags:        UpdateTriggerFlags,
    check_repair: bool,
) -> Result<(), BchError> {
    let gc = &t.fs().reflink_gc;

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

    while idx < end {
        gc_segment(t, p, &mut idx, flags, l, check_repair)?;
        l += 1;
    }
    Ok(())
}

/// For C's reflink_p trigger and check_repair: gc_idx().
#[c_export]
pub fn bch2_reflink_p_gc_idx(
    trans:        &BtreeTrans<'_>,
    p:            BkeySC<'_>,
    idx:          u64,
    end:          u64,
    flags:        c::btree_iter_update_trigger_flags,
    check_repair: bool,
) -> c_int {
    // SAFETY: C calls this in a trigger or check_repair, in an attempt
    let t = unsafe { trans.attempt_in_progress() };
    ret_to_c(gc_idx(&t, p, idx, end, UpdateTriggerFlags::from_bits_retain(flags.bits()), check_repair))
}

/// Check @k's refcount against gc's count: as bch2_gc_write_reflink_key().
fn gc_write_reflink_key(
    t:    &TransAttempt<'_, '_>,
    iter: &BtreeIter<'_>,
    k:    BkeySC<'_>,
    idx:  &mut usize,
) -> Result<(), BchError> {
    let fs = t.fs();
    let Some(refcount) = k.refcount() else { return Ok(()) };
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
            *new.k_i_mut().refcount_mut().expect("an indirect extent") = (r_refcount as u64).into();
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
        if k.refcount().is_none() {
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
