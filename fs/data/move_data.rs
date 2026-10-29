// SPDX-License-Identifier: GPL-2.0

//! The mover's walks, which find what to move: move_data_btree() walks a
//! btree's levels, and the phys walk - the evacuates, scrub - a device's
//! backpointers. Each key a predicate picks is moved by move_extent(): an
//! extent's data update (moving_update.rs), a btree node's rewrite, or a
//! scrub's read of one replica.
//!
//! A predicate (Pred) is given the key and its io options, fills in the
//! move's options and says whether to move it; with its name, for the
//! data_update_pred trace. Before it, the key's reconcile options are brought
//! up to date and committed, lazily: a commit restarts, and the key is peeked
//! again with them.
//!
//! The walks run their own attempts and don't commit: a move unlocks the
//! transaction to issue its read. They ratelimit() between keys, skip a key
//! whose update failed (data_update_fail), and stop on what going on won't
//! help: the filesystem going read-only, an IO error, the device gone.
//!
//! The phys walk resolves every backpointer it passes, moving its key or not:
//! resolving is what repairs a dangling one (backpointer_to_missing_ptr,
//! autofix). And the audit of each bucket's backpointers against its alloc
//! key, check_one_bucket_backpointer_mismatch(), run as the walk passes the
//! bucket, can only see missing backpointers once the dangling ones are gone:
//! a dangling one still counts its data_type, gen and length in the sums,
//! hiding the mismatch forever - and copygc livelocks, its is_movable
//! approving the bucket, evacuation skipping every backpointer and moving
//! nothing. A walk that finishes audits the buckets past its last
//! backpointer; one that stopped doesn't - it didn't skip them, it stopped,
//! and the audit is slow enough to be what someone who hit ctrl-C waits on.
//!
//! A stripe isn't moved: the phys walk of a scrub checks its blocks against
//! their checksums instead (ec/repair.rs's scrub_stripe_block()).

use crate::alloc::backpointers::{backpointer_get_key, BackpointerPos};
use crate::alloc::capacity::DiskReservation;
use crate::alloc::check_backpointers::check_one_bucket_backpointer_mismatch;
use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{pos, BkeyI, BkeySC};
use crate::btree::bkey_buf::BkeyBuf;
use crate::btree::iter::{is_restart, BtreeIter, BtreeIterFlags, BtreeLevels, CommitFlags, CommitOpts, TransAttempt};
use crate::btree::write_buffer::WbMaybeFlush;
use crate::c;
use crate::c::bch_errcode as err;
use crate::data::ec::repair::scrub_stripe_block;
use crate::data::moving::{key_sectors, MoveStats, Moving, MovingCtxt};
use crate::data::moving_update;
use crate::data::ptrs::{BkeyVPtrs, Ptrs};
use crate::data::reconcile::needs_reconcile::update_reconcile_opts;
use crate::data::update::{fail_should_trace, is_scrub};
use crate::errcode::{ret_to_c, BchError};
use crate::fs::{writepoint_hashed, Fs};
use crate::inode_opts::{get_io_opts, PerSnapshotIoOpts};
use crate::sb::counters::events;
use crate::util::ffi::Opaque;
use crate::util::kernel::current_id;
use crate::util::Printbuf;
use crate::{bch_err_msg, warn_once};
use core::ffi::{c_int, c_uint};
use core::ops::Range;
use core::ptr::NonNull;
use cstruct_macros::c_export;

/// What to move: of @k, in @btree, with @io_opts, the move's options filled
/// in, and whether to. In the walk's attempt, so it can look things up - an
/// error, or a restart, is the walk's.
pub type Pred<'p> = dyn FnMut(&TransAttempt<'_, '_>, c::btree_id, BkeySC<'_>, &c::bch_inode_opts,
                              &mut c::data_update_opts) -> Result<bool, BchError> + 'p;

// ── Moving a key ─────────────────────────────────────────────────────────

/// A btree node rewrite has no data_update, whose exit traces an extent's
/// move: so a btree node's is traced here. @k is a copy - the rewrite frees
/// the node the key was in.
#[cold]
fn move_btree_node_trace(fs: &Fs, opts: &c::bch_inode_opts, data_opts: &c::data_update_opts, k: BkeySC<'_>,
                         ret: Result<(), BchError>) {
    let sectors = fs.btree_sectors().into();
    let msg = |out: &mut Printbuf| {
        write!(out, "{}\n", k.to_text(fs));
        fs.data_update_opts_to_text(out, opts, data_opts);
    };
    match ret {
        Ok(()) => fs.event_add_trace(events::data_update, sectors, msg),
        Err(e) if fail_should_trace(data_opts.type_, e) => fs.event_add_trace(events::data_update_fail, sectors, |out| {
            msg(out);
            write!(out, "\nret:\t{e}\n");
        }),
        Err(_) => {}
    }
}

impl<'a> Moving<'a> {
    /// Start moving @k, at @iter's position in the context's transaction, as
    /// @data_opts say - returning once the read is issued: an extent's data
    /// update, a btree node's rewrite, or a scrub's read. Past this the
    /// attempt has no commit left to make: an extent's move unlocks to issue
    /// its read, @k copied first. The read retries itself - restarts in its
    /// erasure coded fallback - which aren't the attempt's.
    ///
    /// The move holds a count on @bucket until it's done.
    pub fn move_extent(
        &self,
        t:         &TransAttempt<'_, 'a>,
        bucket:    Option<&c::move_bucket>,
        opts:      &c::bch_inode_opts,
        data_opts: &c::data_update_opts,
        iter:      &BtreeIter<'_>,
        level:     u32,
        k:         BkeySC<'_>,
    ) -> Result<(), BchError> {
        let fs = t.fs();

        if is_scrub(data_opts.type_) && !fs.dev_idx_is_online(data_opts.read_dev) {
            return fs.throw(err::BCH_ERR_device_offline);
        }

        let ret = if !k.k.is_btree_ptr() {
            moving_update::move_extent(self, t, bucket, iter, k, opts, data_opts)
        } else if !is_scrub(data_opts.type_) {
            self.move_btree_node(t, opts, data_opts, iter, level, k)
        } else if data_opts.type_ == c::bch_data_update_types::BCH_DATA_UPDATE_scrub {
            t.btree_node_scrub(iter.btree(), level, k, data_opts.read_dev)
        } else {
            // scrub_no_repair: node scrub repairs, and the journal scrub
            // checks nodes itself with a report callback - scrub_journal_node()
            // - so nothing should send one here
            warn_once!(true, fs, "move_extent(): a btree node with scrub_no_repair");
            fs.throw(err::BCH_ERR_invalid)
        };
        t.restarts_handled();

        match ret {
            // wait for some IO to finish, and retry
            Err(e) if e.matches(c::ENOMEM) => {
                self.wait_for_io();
                Err(t.restart(err::BCH_ERR_transaction_restart_nested))
            }
            r => r,
        }
    }

    fn move_btree_node(&self, t: &TransAttempt<'_, 'a>, opts: &c::bch_inode_opts, data_opts: &c::data_update_opts,
                       iter: &BtreeIter<'_>, level: u32, k: BkeySC<'_>) -> Result<(), BchError> {
        use c::bch_watermark as wm;
        let fs = t.fs();

        if data_opts.type_ != c::bch_data_update_types::BCH_DATA_UPDATE_copygc {
            if let Err(e) = t.can_do_data_update(opts, data_opts, k, None) {
                if !is_restart(&e) {
                    move_btree_node_trace(fs, opts, data_opts, k, Err(e));
                }
                return Err(e);
            }
        }

        let node_key = BkeyBuf::from_key(k);

        let mut flags = CommitOpts::from(data_opts.commit_flags);
        if data_opts.commit_flags.bits() & c::BCH_WATERMARK_MASK == wm::BCH_WATERMARK_copygc as u32 {
            flags = flags.watermark_hipri();
        }

        let ret = t.btree_node_rewrite_pos(iter.btree(), level, k.k.p, data_opts.target.into(), flags,
                                           data_opts.write_flags);

        // ENOMEM becomes a restart, retried: not an outcome
        if !matches!(&ret, Err(e) if is_restart(e) || e.matches(c::ENOMEM)) {
            move_btree_node_trace(fs, opts, data_opts, BkeySC::from(&node_key), ret);
        }
        ret
    }
}

/// For the journal scrub, until it's Rust - its walk's progress counted here,
/// as C's was.
///
/// # Safety
/// @ctxt's transaction is in an attempt, at @iter.
#[c_export]
pub unsafe fn bch2_move_extent(ctxt: &Opaque<c::moving_context>, bucket: Option<&c::move_bucket>,
                               opts: &c::bch_inode_opts, data_opts: &c::data_update_opts, iter: &BtreeIter<'_>,
                               level: c_uint, k: BkeySC<'_>) -> c_int {
    let moving = Moving::from_c(ctxt);
    // SAFETY: the caller's
    let t = unsafe { moving.trans().attempt_in_progress() };
    let sectors = key_sectors(t.fs(), k);
    let ret = moving.move_extent(&t, bucket, opts, data_opts, iter, level, k);
    if !matches!(&ret, Err(e) if is_restart(e)) {
        if let Some(stats) = moving.stats_opt() {
            stats.walked(iter, sectors);
        }
    }
    ret_to_c(ret)
}

/// What a walk moves with: the context, the bucket each move holds a count on,
/// and the predicate, named for the data_update_pred trace.
struct Mover<'a, 'p> {
    moving: &'a Moving<'a>,
    bucket: Option<&'a c::move_bucket>,
    name:   &'a str,
    pred:   &'a mut Pred<'p>,
}

impl Mover<'_, '_> {
    /// @k moved, if it's data the predicate picks: its io options - cached
    /// across a walk in key order by @s - and reconcile options, committed
    /// first.
    fn move_key(&mut self, t: &TransAttempt<'_, '_>, mut s: Option<&mut c::per_snapshot_io_opts>,
                iter: &BtreeIter<'_>, level: u32, k: BkeySC<'_>) -> Result<(), BchError> {
        if !k.k.extent_is_direct_data() {
            return Ok(());
        }

        let fs = t.fs();
        let opts = get_io_opts(t, s.as_deref_mut(), k)?;
        update_reconcile_opts(t, s, &opts, iter, level, k, c::set_needs_reconcile_ctx::SET_NEEDS_RECONCILE_other)?;
        t.commit_lazy(Some(&DiskReservation::new(fs)), CommitFlags::NO_ENOSPC)?;

        let mut data_opts = c::data_update_opts { read_dev: u32::MAX, ..Default::default() };
        let ret = (self.pred)(t, iter.btree(), k, &opts, &mut data_opts);

        fs.event_add_trace(events::data_update_pred, k.k.size.into(), |out| {
            match ret {
                Ok(m) => write!(out, "{}: {}\n", self.name, i32::from(m)),
                Err(e) => write!(out, "{}: {e}\n", self.name),
            };
            write!(out, "{}\n", k.to_text(fs));
            fs.data_update_opts_to_text(out, &opts, &data_opts);
        });

        if !ret? {
            return Ok(());
        }
        self.moving.move_extent(t, self.bucket, &opts, &data_opts, iter, level, k)
    }
}

// ── A btree's keys ───────────────────────────────────────────────────────

impl Moving<'_> {
    /// Start moving what @pred picks of @btree, at @levels, @start to @end -
    /// returning once the moves are issued.
    pub fn move_data_btree(&self, btree: c::btree_id, levels: Range<u32>, start: c::bpos, end: c::bpos,
                           name: &str,
                           mut pred: impl FnMut(&TransAttempt<'_, '_>, c::btree_id, BkeySC<'_>, &c::bch_inode_opts,
                                                &mut c::data_update_opts) -> Result<bool, BchError>)
        -> Result<(), BchError>
    {
        let trans = self.trans();
        let fs = trans.fs();
        let mut s = PerSnapshotIoOpts::new(fs);
        let mut mover = Mover { moving: self, bucket: None, name, pred: &mut pred };

        if let Some(stats) = self.stats_opt() {
            stats.at.data_type.set(c::bch_data_type::BCH_DATA_user);
            stats.set_pos(c::bbpos { btree, pos: start });
        }

        BtreeLevels { btree, levels, start, end }.for_each(trans, |t, iter, level, k| {
            let sectors = key_sectors(fs, k);

            match mover.move_key(t, Some(&mut s), iter, level, k) {
                Ok(()) => {}
                // failure for this key: go on
                Err(e) if e.matches(err::BCH_ERR_data_update_fail) => {}
                // a topology error, a btree node read error
                Err(e) if is_restart(&e) || e.matches(c::EROFS) || e.matches(c::EIO) => return Err(e),
                Err(e) => {
                    warn_once!(!e.matches(c::EAGAIN), fs, "unhandled error from move_extent: {e}");
                }
            }
            if let Some(stats) = self.stats_opt() {
                stats.walked(iter, sectors);
            }
            // between keys: it unlocks, and @k is done with
            self.ratelimit()
        })
    }
}

// ── A device's backpointers ──────────────────────────────────────────────

/// The backpointers a phys walk walks.
#[derive(Clone, Copy)]
enum BpWalk {
    /// A device's, in BTREE_ID_backpointers
    Dev(u32),
    /// Of block @block of stripe @idx, whose device was removed, in
    /// BTREE_ID_stripe_backpointers: their extents' pointers are
    /// BCH_SB_MEMBER_INVALID, still holding the stripe
    EcOrphan { idx: u64, block: u32 },
}

struct PhysWalk {
    bps:            BpWalk,
    /// Sectors @start..@end
    start:          u64,
    end:            u64,
    /// Bit per data type to move
    data_types:     u32,
    /// Scrub: check the device's stripe blocks against their checksums
    verify_stripes: bool,
    /// Copygc's: the audit schedules its repair ratelimited
    copygc:         bool,
}

impl PhysWalk {
    /// Every data type, no stripe scrub, no copygc.
    fn new(bps: BpWalk, start: u64, end: u64) -> Self {
        PhysWalk { bps, start, end, data_types: !0, verify_stripes: false, copygc: false }
    }
}

/// A phys walk under way.
struct PhysWalker<'a, 'p> {
    mover:        Mover<'a, 'p>,
    w:            &'a PhysWalk,
    /// The device, for a walk of a device's backpointers: it's audited
    ca:           Option<&'a c::bch_dev>,
    last_flushed: WbMaybeFlush,
    /// Buckets below it audited
    audited:      u64,
}

impl PhysWalker<'_, '_> {
    /// The buckets up to @bucket audited.
    fn audit_to(&mut self, bucket: u64) {
        let Some(ca) = self.ca else { return };
        while self.audited < bucket {
            let _ = check_one_bucket_backpointer_mismatch(self.mover.moving.trans(), ca, self.audited, self.w.copygc,
                                                          self.last_flushed.borrow());
            self.audited += 1;
        }
    }

    /// Backpointer @bp's key resolved - and moved, if it's of a data type
    /// moved and the predicate picks it. An error stops the walk.
    fn bp(&mut self, t: &TransAttempt<'_, '_>, bp: &BkeyI<c::Backpointer>) -> Result<(), BchError> {
        let fs = t.fs();
        let moving = self.mover.moving;
        let v = &bp.v;
        let moved = self.w.data_types & (1 << v.data_type) != 0;

        if let Some(stats) = moving.stats_opt() {
            stats.at.offset.set(bp.k.p.offset >> fs.sb.extent_bp_shift.get());
        }

        let mut iter = BtreeIter::uninit();
        let Some((iter, k)) = backpointer_get_key(t, BkeySC::from(bp), &mut iter, BtreeIterFlags::empty(),
                                                  self.last_flushed.borrow())? else { return Ok(()) };

        if v.level == 0 && c::btree_id(v.btree_id.into()) == c::btree_id::stripes {
            return match self.ca.filter(|_| self.w.verify_stripes && moved) {
                None => Ok(()),
                Some(ca) => scrub_stripe_block(moving, t, ca, BkeySC::from(bp), k),
            };
        }

        // not moving these: resolving them still verified them
        if !moved {
            return Ok(());
        }

        let bucket_len = { v.bucket_len };
        let ret = self.mover.move_key(t, None, iter, v.level.into(), k);
        if matches!(&ret, Err(e) if is_restart(e)) {
            return ret;
        }
        if let Some(stats) = moving.stats_opt() {
            stats.walked(iter, bucket_len.into());
        }
        match ret {
            Ok(()) => Ok(()),
            // failure for this key: go on
            Err(e) if e.matches(err::BCH_ERR_data_update_fail) => Ok(()),
            Err(e) if e.matches(c::EAGAIN) || e.matches(c::EROFS) || e.matches(c::EIO) ||
                      e.matches(err::BCH_ERR_device_offline) => Err(e),
            Err(e) => {
                warn_once!(true, fs, "unhandled error from move_extent: {e}");
                Ok(())
            }
        }
    }
}

/// The evacuates' predicate: the pointers @kill picks moved, to the
/// background target.
fn evacuate_ptrs(t: &TransAttempt<'_, '_>, k: BkeySC<'_>, io_opts: &c::bch_inode_opts,
                 data_opts: &mut c::data_update_opts, mut kill: impl FnMut(&c::extent_ptr_decoded) -> bool)
    -> Result<bool, BchError>
{
    data_opts.target = io_opts.background_target;
    if let Some(k) = k.arm::<Ptrs>() {
        for (i, d) in k.ptrs_decoded(t.fs()).enumerate() {
            if kill(&d.p) {
                data_opts.ptrs_kill |= 1 << i;
            }
        }
    }
    Ok(data_opts.ptrs_kill != 0)
}

impl Moving<'_> {
    fn move_data_phys(&self, bucket: Option<&c::move_bucket>, w: &PhysWalk, name: &str, pred: &mut Pred<'_>)
        -> Result<(), BchError>
    {
        let trans = self.trans();
        let fs = trans.fs();

        let (bp_btree, bp_dev, ca) = match w.bps {
            // userspace might have supplied @dev
            BpWalk::Dev(dev) => (c::btree_id::backpointers, dev.into(), fs.dev_get(dev)),
            BpWalk::EcOrphan { idx, block } =>
                (c::btree_id::stripe_backpointers, BackpointerPos::ec_removed_dev(idx, block), None),
        };

        let end = ca.as_ref().map_or(w.end, |ca| w.end.min(ca.bucket_to_sector(ca.mi.get().nbuckets)));
        let shift = fs.sb.extent_bp_shift.get();
        let bp_end = pos(bp_dev, end << shift);

        let mut walker = PhysWalker {
            mover:        Mover { moving: self, bucket, name, pred },
            w,
            ca:           ca.as_deref(),
            last_flushed: WbMaybeFlush::new().map_err(|_| BchError::from(c::ENOMEM))?,
            audited:      ca.as_ref().map_or(0, |ca| ca.sector_to_bucket(w.start)),
        };

        // not run where transaction restarts are handled
        trans.begin_raw();
        let mut bp_iter = BtreeIter::new(trans, bp_btree, pos(bp_dev, w.start << shift), BtreeIterFlags::empty());

        match trans.write_buffer_tryflush() {
            Err(e) if e.matches(c::EROFS) => return Err(e),
            r => bch_err_msg!(fs, r, "flushing btree write buffer")?,
        }

        bp_iter.for_each_max(trans, bp_end, |t, _, k| {
            // the buckets before this backpointer's, which the audit runs
            // transactions of its own for: then the same position again
            let bucket = walker.ca.map(|ca| BackpointerPos::from_bpos(fs, k.k.p).bucket(ca).offset);
            if let Some(bucket) = bucket.filter(|&b| walker.audited < b) {
                return Err(t.close(CommitFlags::empty(), |_| {
                    walker.audit_to(bucket);
                    Ok(())
                }));
            }

            if let Some(bp) = BkeyI::<c::Backpointer>::from_key(k) {
                walker.bp(t, &bp)?;
            }
            // between backpointers: it unlocks, and @k is done with
            self.ratelimit()
        })?;

        if let Some(ca) = walker.ca {
            walker.audit_to(ca.sector_to_bucket(end));
        }
        Ok(())
    }

    /// Start moving the data in sectors @start..@end of device @dev off it,
    /// found by backpointer - to the extents' background target - returning
    /// once the moves are issued.
    pub fn evacuate_data(&self, dev: u32, start: u64, end: u64) -> Result<(), BchError> {
        self.move_data_phys(None, &PhysWalk::new(BpWalk::Dev(dev), start, end), "evacuate_pred",
                            &mut |t, _, k, io_opts, d| {
            evacuate_ptrs(t, k, io_opts, d, |p| u32::from(p.ptr.dev()) == dev)
        })
    }

    /// evacuate_data(), for block @block of stripe @idx, on a device that's
    /// been removed: its extents found by their stripe_backpointers, the
    /// pointers moved those that point to the block.
    pub fn evacuate_ec_orphan(&self, idx: u64, block: u32, start: u64, end: u64) -> Result<(), BchError> {
        let w = PhysWalk::new(BpWalk::EcOrphan { idx, block }, start, end);
        // Every removed device's pointers are BCH_SB_MEMBER_INVALID: the
        // block's are the ones whose stripe pointer is to it
        self.move_data_phys(None, &w, "evacuate_ec_orphan_pred", &mut |t, _, k, io_opts, d| {
            evacuate_ptrs(t, k, io_opts, d, |p| {
                u32::from(p.ptr.dev()) == c::BCH_SB_MEMBER_INVALID && p.has_ec &&
                    p.ec.idx() == idx && u32::from(p.ec.block()) == block
            })
        })
    }

    /// Start moving everything off @key's bucket, while it's that
    /// generation, as @data_opts say: its backpointers walked, each extent's
    /// move issued - returning once the reads are. Each move holds a count on
    /// @bucket_in_flight until its update is done.
    ///
    /// # Safety
    /// @bucket_in_flight stays valid, where it is, until its count is back
    /// to what it was.
    pub unsafe fn evacuate_bucket(&self, bucket_in_flight: NonNull<c::move_bucket>, key: c::move_bucket_key,
                                  data_opts: c::data_update_opts) -> Result<(), BchError> {
        let fs = self.trans().fs();
        let bucket = key.bucket;
        let generation = key.generation as i32;

        // userspace might have supplied @dev
        let Some(ca) = fs.dev_get(bucket.inode as u32) else { return Ok(()) };

        let w = PhysWalk {
            copygc: true,
            ..PhysWalk::new(BpWalk::Dev(bucket.inode as u32), ca.bucket_to_sector(bucket.offset),
                            ca.bucket_to_sector(bucket.offset + 1))
        };
        let mut name = Printbuf::new();
        write!(name, "evacuate_bucket_pred gen={generation}");
        let mut sectors = 0u32;
        // SAFETY: the caller's
        let ret = self.move_data_phys(Some(unsafe { bucket_in_flight.as_ref() }), &w, name.as_str(),
                                      &mut |t, _, k, io_opts, d| {
            *d = data_opts;
            d.read_dev = u32::MAX;
            evacuate_ptrs(t, k, io_opts, d, |p| {
                let kill = u64::from(p.ptr.dev()) == bucket.inode &&
                    (generation < 0 || generation == i32::from(p.ptr.generation())) &&
                    p.ptr.cached() == 0;
                if kill {
                    sectors += p.crc.compressed_size;
                }
                kill
            })
        });

        fs.event_inc_trace(events::evacuate_bucket, |out| {
            write!(out, "bucket: {bucket} gen: {generation} ret {}\n", BchError::from_raw(ret_to_c(ret)));
            write!(out, "{sectors}/{} sectors\n", { ca.mi.get().bucket_size });
        });
        ret
    }
}

// ── Data jobs ────────────────────────────────────────────────────────────

/// Scrub: one replica on @arg's device read and checked - of a btree node,
/// any; of an extent, a checksummed one.
fn scrub(fs: &Fs, stats: &MoveStats, arg: c::bch_ioctl_data_scrub) -> Result<(), BchError> {
    let ctxt = MovingCtxt::new(fs, stats, writepoint_hashed(current_id()), false)
        .map_err(|_| BchError::from(c::ENOMEM))?;
    stats.at.data_type.set(c::bch_data_type(c::DATA_PROGRESS_DATA_TYPE_phys));

    let _ = ctxt.trans().write_buffer_flush_sync();

    let dev = arg.dev;
    let w = PhysWalk {
        data_types:     arg.data_types,
        verify_stripes: true,
        ..PhysWalk::new(BpWalk::Dev(dev), 0, u64::MAX)
    };
    ctxt.move_data_phys(None, &w, "scrub_pred", &mut |t, _, k, _, data_opts| {
        if k.k.key_type() != c::bch_bkey_type::KEY_TYPE_btree_ptr_v2 {
            if let Some(kp) = k.arm::<Ptrs>() {
                if let Some(d) = kp.ptrs_decoded(t.fs()).find(|d| u32::from(d.p.ptr.dev()) == dev) {
                    if d.p.crc.csum_type == 0 {
                        return Ok(false);
                    }
                }
            }
        }

        data_opts.type_ = c::bch_data_update_types::BCH_DATA_UPDATE_scrub;
        data_opts.read_dev = dev;
        data_opts.read_flags = c::bch_read_flags::BCH_READ_hard_require_read_device;
        Ok(true)
    })
}

macro_rules! data_op_names {
    ($(($name:tt, $n:tt)),* $(,)?) => { const DATA_OP_NAMES: &[&str] = &[$(stringify!($name),)*]; };
}
c::BCH_DATA_OPS!(data_op_names);

/// The ioctl's data job @op, counted into @stats. Scrub is the only op.
pub fn data_job(fs: &Fs, stats: &MoveStats, op: &c::bch_ioctl_data) -> Result<(), BchError> {
    let Some(name) = DATA_OP_NAMES.get(usize::from(op.op)) else {
        return fs.throw(err::BCH_ERR_EINVAL_data_job_bad_op);
    };
    stats.reset(name);

    match u32::from(op.op) {
        c::BCH_DATA_OP_scrub => {
            // so tests don't fail spuriously: every btree node that needs
            // repair is seen
            fs.btree_interior_updates_flush();
            scrub(fs, stats, op.scrub())
        }
        _ => Err(c::EINVAL.into()),
    }
}
