// SPDX-License-Identifier: GPL-2.0

//! An extent update - extent_update(), with its inode's - cut to what one
//! transaction can do: extent_trim_atomic().
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

use crate::alloc::capacity::DiskReservation;
use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{bkey_ge, bkey_gt, pos, spos, BkeyI, BkeyS, BkeySC, BkeyV, BkeyVMut, Typed};
use crate::btree::iter::{BtreeIter, BtreeIterFlags, CommitFlags, TransAttempt, TransBkey, UpdateTriggerFlags};
use crate::btree::update::extent_whiteout_type;
use crate::c;
use crate::data::extents::{BkeyVExtents, Extents};
use crate::data::ptrs::{BkeyVPtrs, ExtentEntry, Ptrs};
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::init::error::count_fsck_err;
use crate::debug::trace::events;
use crate::sb::errors::SbErrorId;
use crate::util::ffi::Opaque;
use crate::util::kernel::{likely, unlikely};
use crate::util::log::LogMsg;
use crate::{inode, inode_opts, prt};
use core::ops::ControlFlow;
use crate::format::{Bpos, Btree, BkeyType};

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
    end: Bpos,
}

impl Count {
    /// At the limit, the update stops at @at: whether it's there.
    fn stop_at(&mut self, at: Bpos) -> bool {
        let full = self.nr >= EXTENT_ITERS_MAX;
        if full {
            self.end = self.end.min(at);
        }
        full
    }
}

impl<'t> TransAttempt<'_, 't> {
    /// The iterators overwriting @k takes, @offset into it - besides its
    /// own, which the caller's counted: whether the count's full.
    fn count_iters_for_insert(&self, k: BkeySC<'_>, offset: u64, n: &mut Count) -> Result<bool, BchError> {

        n.stop_at(k.k.p);

        match k.k.key_type() {
            BkeyType::extent | BkeyType::reflink_v => {
                n.nr += nr_alloc_ptrs(self.fs(), k);
                n.stop_at(k.k.p);
            }
            BkeyType::reflink_p => {
                let idx = k.arm::<c::ReflinkP>().expect("a reflink_p").v().idx();
                let sectors = n.end.min(k.k.p).offset - k.k.start_offset();
                let mut iter = BtreeIter::new(self, Btree::reflink, pos(0, idx + offset),
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
            if k.k.key_type().is_extent_whiteout() {
                let whiteout_type = *whiteout_type.get_or_insert_with(||
                    extent_whiteout_type(fs, iter.btree(), &ins));

                if k.k.p > ins.p {
                    if k.k.key_type() == BkeyType::extent_whiteout {
                        break;
                    }
                } else if k.k.key_type() != whiteout_type {
                    n.nr += 1;
                    if n.stop_at(k.k.p) {
                        break;
                    }
                }
            } else {
                if k.k.start_pos() >= n.end {
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

        if ins.p > n.end {
            fs.event_trace(events::extent_trim_atomic, |out| {
                prt!(out, "{}\n{}", n.end, insert.as_sc().to_text(fs));
            });
            insert.cut_back_in_place(n.end);
        }
        Ok(())
    }
}

/// What writing @new over what it overwrites does to the inode and the disk:
/// bch2_sum_sector_overwrites().
pub struct SectorOverwrites {
    /// Whether it takes space the overwritten keys don't give back: a
    /// snapshot's overwrite, more replicas, or uncompressed over compressed.
    /// An enospc check is due.
    pub usage_increasing:   bool,
    pub i_sectors_delta:    i64,
    pub disk_sectors_delta: i64,
    /// What disk_sectors_delta is weighted by: what topping a reservation up
    /// to cover it charges at.
    pub new_nr_replicas:    u32,
}

impl<'t> TransAttempt<'_, 't> {
    /// @new's SectorOverwrites, from @extent_iter at @new's start.
    pub fn sum_sector_overwrites(&self, extent_iter: &BtreeIter<'t>, new: &BkeyI) -> Result<SectorOverwrites, BchError> {
        let fs = self.fs();
        let new_k = *new.k();
        let new_d = BkeySC::from(new).durability_safe(fs);

        let mut s = SectorOverwrites {
            usage_increasing:   false,
            i_sectors_delta:    0,
            disk_sectors_delta: 0,
            /*
             * @disk_sectors_delta is weighted by this below, so a caller topping up
             * a reservation to cover it needs to know the count it's charging at:
             */
            new_nr_replicas:    new_d.nr_replicas.into(),
        };

        /* @extent_iter is at @new's start: each slot, trimmed, is what @new overwrites */
        let mut iter = extent_iter.copy();
        iter.set_flags(BtreeIterFlags::TRIM);

        while let Some(old) = iter.peek_max_type(self, new_k.p, BtreeIterFlags::SLOTS)? {
            let sectors = i64::from(old.k.size);

            let old_d = old.durability_safe(fs);

            s.i_sectors_delta += sectors *
                (i64::from(new_k.extent_is_allocation()) -
                 i64::from(old.k.extent_is_allocation()));

            /*
             * What the new key will occupy, less what overwriting the old
             * one gives back. Only the old key's uncompressed replicas
             * count against us: a compressed extent holds fewer sectors
             * than it covers, so crediting its full extent would hand back
             * space that was never taken.
             *
             * nr_replicas, not total: this is space accounting, and it must
             * not depend on mi.durability. Durability is OPT_RUNTIME while
             * accounting is persistent, so weighting by it would let a
             * device setting change retroactively invalidate space already
             * accounted for.
             */
            s.disk_sectors_delta += sectors * i64::from(new_d.nr_replicas);
            s.disk_sectors_delta -= if new_k.p.snapshot == old.k.p.snapshot {
                sectors * i64::from(old_d.nr_overwritable)
            } else {
                0
            };

            if !s.usage_increasing &&
               (new_k.p.snapshot != old.k.p.snapshot ||
                new_d.replicas > old_d.replicas ||
                (new_d.sectors_compressed == 0 && old_d.sectors_compressed != 0)) {
                s.usage_increasing = true;
            }

            if bkey_ge(old.k.p, new_k.p) {
                break;
            }
            iter.advance();
        }

        Ok(s)
    }

    #[cold]
    fn bi_sectors_underflow(&self, inode: Bpos, bi_sectors: i64, i_sectors_delta: &mut i64) {
        let fs = self.fs();
        let (inum, snapshot) = (inode.offset, inode.snapshot);

        let mut msg = LogMsg::new(fs);
        prt!(&mut *msg, "inode {inum} snapshot {snapshot} i_sectors underflow: {} + {} < 0",
             bi_sectors, *i_sectors_delta);

        let counted = count_fsck_err(fs, SbErrorId::inode_i_sectors_underflow, &mut msg);
        msg.set_suppressed(!counted);

        *i_sectors_delta = if *i_sectors_delta < 0 { -bi_sectors } else { 0 };
    }

    /// The update of the inode @extent_iter's extents are in: i_size raised
    /// to @new_i_size, i_sectors by @i_sectors_delta, at @extent_iter's
    /// snapshot. Its io options in @opts.
    pub(crate) fn extent_update_i_size_sectors(&self, extent_iter: &BtreeIter<'t>, new_i_size: u64,
                                    mut i_sectors_delta: i64, opts: &mut c::bch_inode_opts)
        -> Result<(), BchError>
    {
        let fs = self.fs();

        let mut iter = BtreeIter::new(self, Btree::inodes,
                                      spos(0, extent_iter.pos().inode, extent_iter.snapshot()),
                                      BtreeIterFlags::INTENT | BtreeIterFlags::CACHED);
        let k = iter.peek_slot(self)?.expect("a slot");

        /* Slack for varint::decode_fast(), which reads up to 7 bytes past the end: */
        let mut k_mut: TransBkey<'_, 't> = self.bkey_reassemble_room(k, 0)?;

        if unlikely(k_mut.k().key_type() != BkeyType::inode_v3) {
            k_mut = inode::to_v3(self, k_mut.as_ref())?;
        }

        let mut inode_update_flags = UpdateTriggerFlags::NOJOURNAL;
        {
            let mut ks = BkeyS::from(&mut k_mut);
            let Typed::inode_v3(mut inode) = ks.as_s().typed() else {
                unreachable!("an inode_v3, made so above")
            };

            /*
             * Fast path: if the inode has no per-inode io opts (the common case),
             * skip the full unpack and use fs defaults. has_inode_opts is set on
             * existing inodes by the upgrade's scheduled passes; version upgrades
             * always run, so by the time we're doing io the flags are trustworthy:
             */
            *opts = if likely(inode.v().bi_flags.get() & u64::from(c::bch_inode_flags::has_inode_opts.bits()) == 0) {
                inode_opts::get_fs(fs, false)
            } else {
                inode_opts::get_inode(fs, &inode::unpack(fs, k))
            };

            /*
             * Crazy performance optimization:
             * Every extent update needs to also update the inode: the inode trigger
             * will set bi->journal_seq to the journal sequence number of this
             * transaction - for fsync.
             *
             * But if that's the only reason we're updating the inode (we're not
             * updating bi_size or bi_sectors), then we don't need the inode update
             * to be journalled - if we crash, the bi_journal_seq update will be
             * lost, but that's fine.
             */
            if new_i_size > inode.v().bi_size.get() {
                inode.v_mut().bi_size = new_i_size.into();
                inode_update_flags = UpdateTriggerFlags::empty();
            }

            if i_sectors_delta != 0 {
                let bi_sectors = inode.v().bi_sectors.get() as i64;
                if unlikely(bi_sectors + i_sectors_delta < 0) {
                    self.bi_sectors_underflow(inode.k.p, bi_sectors, &mut i_sectors_delta);
                }

                inode.v_mut().bi_sectors = ((bi_sectors + i_sectors_delta) as u64).into();
                inode_update_flags = UpdateTriggerFlags::empty();
            }
        }

        /*
         * extents, dirents and xattrs updates require that an inode update also
         * happens - to ensure that if a key exists in one of those btrees with
         * a given snapshot ID an inode is also present - so we may have to skip
         * the nojournal optimization:
         */
        if k_mut.k().p.snapshot != iter.snapshot() {
            k_mut.k_mut().p.snapshot = iter.snapshot();
            inode_update_flags = UpdateTriggerFlags::empty();
        }

        self.update(&iter, &k_mut, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE | inode_update_flags)
    }

    /// @k written at @iter and committed, with the inode's update: i_size
    /// raised to @new_i_size - no further than @k reaches - and i_sectors by
    /// what @k adds, which @i_sectors_delta_total gets too. @k is cut to what
    /// one commit takes, @iter moved to its end, and it may grow into its room
    /// for a reconcile entry. @disk_res is topped up for the space @k takes;
    /// that fails for want of space only if @check_enospc and @k takes space
    /// what it overwrites doesn't give back. @flush, if given, waits on the
    /// commit's journal flush - unless @k was cut, the commit not the last.
    #[allow(clippy::too_many_arguments)]
    pub fn extent_update<V: ?Sized>(&self, _inum: c::subvol_inum, iter: &mut BtreeIter<'t>,
                                    k: &mut TransBkey<'_, 't, V>, disk_res: &DiskReservation<'_>,
                                    new_i_size: u64, i_sectors_delta_total: Option<&mut i64>,
                                    check_enospc: bool, change_cookie: u32,
                                    mut flush: Option<&Opaque<c::closure>>) -> Result<(), BchError>
    {
        let mut k: TransBkey<'_, 't> = unsafe { k.reborrow().cast() };

        /*
         * This traverses us the iterator without changing iter->path->pos to
         * search_key() (which is pos + 1 for extents): we want there to be a
         * path already traversed at iter->pos because
         * bch2_trans_extent_update() will use it to attempt extent merging
         */
        iter.traverse_at_pos(self)?;

        let mut next_pos = k.k().p;

        self.extent_trim_atomic(iter, &mut k.reborrow().arm::<Extents>()
                                .expect("an update of a key an extents btree can't hold"))?;

        if next_pos != k.k().p {
            next_pos = k.k().p;
            /* trim split us: only the actually-last commit gets the flush */
            flush = None;
        }

        let s = self.sum_sector_overwrites(iter, k.as_ref())?;

        if s.disk_sectors_delta > disk_res.sectors() as i64 {
            disk_res.add_physical((s.disk_sectors_delta - disk_res.sectors() as i64) as u64, s.new_nr_replicas,
                                  if !check_enospc || !s.usage_increasing {
                                      c::bch_reservation_flags::BCH_DISK_RESERVATION_NOFAIL
                                  } else {
                                      c::bch_reservation_flags::empty()
                                  })?;
        }

        let mut opts = c::bch_inode_opts::default();
        self.extent_update_i_size_sectors(iter, (k.k().p.offset << 9).min(new_i_size),
                                          s.i_sectors_delta, &mut opts)?;

        k.set_needs_reconcile(self, None, &opts, c::set_needs_reconcile_ctx::SET_NEEDS_RECONCILE_foreground,
                              change_cookie)?;
        self.update(iter, &k, UpdateTriggerFlags::SET_NEEDS_RECONCILE_DONE)?;

        self.commit_flush(Some(disk_res), flush, CommitFlags::NO_CHECK_RW | CommitFlags::NO_ENOSPC)?;

        if let Some(total) = i_sectors_delta_total {
            *total += s.i_sectors_delta;
        }
        iter.set_pos(next_pos);
        Ok(())
    }
}
