// SPDX-License-Identifier: GPL-2.0

//! An extent update cut to what one transaction can do: extent_trim_atomic().
//!
//! A transaction has a fixed number of btree iterators, and an extent update
//! needs more than its own: one for each key it overwrites, and, through the
//! triggers, for each of their pointers' alloc keys and the freespace or
//! discard key each alloc update may move - two a pointer, or a stripe
//! pointer - and a cached pointer's LRU entry. A reflink pointer's indirect
//! extents count too: their own update, and their pointers'. The update is cut
//! back where the count reaches EXTENT_ITERS_MAX, to the end of the key - or
//! the indirect extent - that reached it; the caller updates the rest in a
//! transaction of its own.
//!
//! Whiteouts it overwrites count one each - unless they're its own whiteout
//! type, which the update leaves as they are. An extent_whiteout past the
//! update's end ends the walk: key start positions, extent_whiteouts included,
//! only go up (btree/update.rs's extent_whiteout_type()).

use crate::btree::bkey::{bkey_ge, bkey_gt, bpos_ge, bpos_gt, bpos_min, pos, BkeySC, BkeyV, BkeyVMut};
use crate::btree::iter::{BtreeIter, BtreeIterFlags, BtreeTrans, TransAttempt, TransBkey};
use crate::btree::update::extent_whiteout_type;
use crate::c;
use crate::data::extents::{BkeyVExtents, Extents};
use crate::data::ptrs::{BkeyVPtrs, ExtentEntry, Ptrs};
use crate::errcode::{ret_to_c, BchError};
use crate::fs::Fs;
use crate::prt;
use crate::sb::counters::events;
use core::ffi::c_int;
use core::ops::ControlFlow;
use cstruct_macros::c_export;

/// The most btree iterators an extent update's transaction may need.
const EXTENT_ITERS_MAX: u32 = 64;

/// The iterators the alloc updates for @k's pointers take - @k an extent or
/// an indirect extent.
fn nr_alloc_ptrs(fs: &Fs, k: BkeySC<'_>) -> u32 {
    let Some(k) = k.arm::<Ptrs>() else { return 0 };
    let (mut ptrs, mut lru) = (0, 0);

    for e in k.extent_entries(fs) {
        match e {
            ExtentEntry::ptr(p) => {
                ptrs += 1;
                lru += (p.cached() != 0) as u32;
            }
            ExtentEntry::stripe_ptr(_) => ptrs += 1,
            _ => {}
        }
    }
    lru + ptrs * 2
}

/// The update's iterators so far, and where it ends.
struct Count {
    nr:  u32,
    end: c::bpos,
}

impl Count {
    /// At the limit, the update stops at @at: whether it's there.
    fn stop_at(&mut self, at: c::bpos) -> bool {
        let full = self.nr >= EXTENT_ITERS_MAX;
        if full {
            self.end = bpos_min(self.end, at);
        }
        full
    }
}

impl<'t> TransAttempt<'_, 't> {
    /// The iterators overwriting @k takes, @offset into it - besides its
    /// own, which the caller's counted: whether the count's full.
    fn count_iters_for_insert(&self, k: BkeySC<'_>, offset: u64, n: &mut Count) -> Result<bool, BchError> {
        use c::bch_bkey_type as t;

        n.stop_at(k.k.p);

        match k.k.key_type() {
            t::KEY_TYPE_extent | t::KEY_TYPE_reflink_v => {
                n.nr += nr_alloc_ptrs(self.fs(), k);
                n.stop_at(k.k.p);
            }
            t::KEY_TYPE_reflink_p => {
                let idx = k.arm::<c::ReflinkP>().expect("a reflink_p").v().idx();
                let sectors = bpos_min(n.end, k.k.p).offset - k.k.start_offset();
                let mut iter = BtreeIter::new(self, c::btree_id::BTREE_ID_reflink, pos(0, idx + offset),
                                              BtreeIterFlags::SLOTS);

                iter.for_each_norestart(self, |_, r_k| {
                    if bkey_ge(r_k.k.start_pos(), pos(0, idx + sectors)) {
                        return Ok(ControlFlow::Break(()));
                    }
                    // two for the indirect extent - one its update, through
                    // extent_update_to_keys() - then its pointers'
                    n.nr += 2 + nr_alloc_ptrs(self.fs(), r_k);

                    let mut at = k.k.start_pos();
                    at.offset += (k.k.size as u64).min(r_k.k.p.offset - idx);
                    Ok(if n.stop_at(at) { ControlFlow::Break(()) } else { ControlFlow::Continue(()) })
                })?;
            }
            _ => {}
        }

        Ok(n.nr >= EXTENT_ITERS_MAX)
    }

    /// Cut @insert back, if it must be, so that its transaction needs no more
    /// than EXTENT_ITERS_MAX iterators: it's to go in at @iter, which only
    /// gives where - the walk is a copy's.
    pub fn extent_trim_atomic<K>(&self, iter: &BtreeIter<'t>, insert: &mut K) -> Result<(), BchError>
    where K: BkeyVExtents + BkeyVMut<V = Extents>
    {
        let fs = self.fs();
        let ins = *insert.k();
        let mut n = Count { nr: 0, end: ins.p };
        let mut whiteout_type = None;

        let mut copy = iter.copy();
        copy.set_flags(BtreeIterFlags::NOFILTER_WHITEOUTS);
        // whole keys: what's overwritten is counted from where they start
        copy.clear_flags(BtreeIterFlags::TRIM);
        copy.traverse(self)?;

        // Whiteouts aren't filtered, but the walk still needs an end, for
        // bch2_snapshot_is_ancestor()'s assert. A key at a time, with none of
        // the iterator's peek flags - its slots, say - as C's walk is
        let end = pos(ins.p.inode, u64::MAX);
        while let Some(k) = copy.peek_max(self, end)? {
            if k.k.is_extent_whiteout() {
                let whiteout_type = *whiteout_type.get_or_insert_with(||
                    extent_whiteout_type(fs, iter.btree(), &ins));

                if bpos_gt(k.k.p, ins.p) {
                    if k.k.key_type() == c::bch_bkey_type::KEY_TYPE_extent_whiteout {
                        break;
                    }
                } else if k.k.key_type() != whiteout_type {
                    n.nr += 1;
                    if n.stop_at(k.k.p) {
                        break;
                    }
                }
            } else {
                if bpos_ge(k.k.start_pos(), n.end) {
                    break;
                }
                let offset = if bkey_gt(iter.pos(), k.k.start_pos()) {
                    iter.pos().offset - k.k.start_offset()
                } else {
                    0
                };
                n.nr += 1;
                if self.count_iters_for_insert(k, offset, &mut n)? {
                    break;
                }
            }

            if !copy.advance() {
                break;
            }
        }

        if bpos_gt(ins.p, n.end) {
            fs.event_trace(events::extent_trim_atomic, |out| {
                prt!(out, "{}\n{}", n.end, insert.as_sc().to_text(fs));
            });
            insert.cut_back_in_place(n.end);
        }
        Ok(())
    }
}

/// extent_trim_atomic(), for C: data/write.c's bch2_extent_update().
///
/// # Safety
/// C has an attempt in progress on @trans, and @k is a key in its memory, in
/// an extents btree.
#[c_export]
pub unsafe fn bch2_extent_trim_atomic(trans: &BtreeTrans<'_>, iter: &mut BtreeIter<'_>, k: *mut c::bkey_i) -> c_int {
    // SAFETY: the caller's
    let t = unsafe { trans.attempt_in_progress() };
    // SAFETY: the caller's - and a key it cuts back only shrinks
    let mut k = unsafe { TransBkey::from_raw_room(&t, k, (*k).k.u64s.into()) }
        .arm::<Extents>().expect("trimming a key an extents btree can't hold");
    ret_to_c(t.extent_trim_atomic(iter, &mut k))
}
