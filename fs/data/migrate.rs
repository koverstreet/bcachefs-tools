// SPDX-License-Identifier: GPL-2.0

//! Dropping a device's data, for its removal: its pointers, from every key
//! that has one.
//!
//! Two ways to find them. dev_data_drop() walks every btree once - the keys
//! that point to data, then the btree node pointers, level by level, the
//! root's included. dev_data_drop_by_backpointers() scans the device's
//! backpointers, which name what points to it; but what points to it changes
//! under the scan - reconcile updates backpointers on read-only devices, and
//! stripe creates the removal doesn't fence move data off it, as open stripes
//! the scan skips. So it scans until the device has no data left, and fails
//! only on scans that stop making progress: ten in a row without a new low in
//! its data buckets. Before each scan, in-flight interior updates are flushed
//! - their nodes have no backpointers yet, and the buckets they replace are
//! still counted (tools#921) - and the write buffer, for the backpointers;
//! after one that skipped open stripes, those are waited for.
//!
//! What a key has left without the device must be what @flags allows: with
//! nothing, BCH_FORCE_IF_{DATA,METADATA}_LOST; with fewer replicas than the
//! filesystem wants, BCH_FORCE_IF_*_DEGRADED - otherwise
//! remove_would_lose_data, the key printed into @err. What's left readable is
//! marked for reconcile, to replicate again; user data with nothing readable
//! becomes an error key, its loss recorded. A btree node pointer is updated
//! through the node it points to.
//!
//! Stripes are erasure coding's, still C: bch2_invalidate_stripe_to_dev() in
//! the backpointer scan, and bch2_dev_remove_stripes(), the caller's, after
//! dev_data_drop().

use crate::accounting::{data_type_is_empty, data_type_is_hidden};
use crate::alloc::backpointers::{backpointer_get_key, backpointer_get_node, BpScanIter};
use crate::alloc::buckets::DiskReservation;
use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{pos, BkeySC, POS_MIN, SPOS_MAX};
use crate::btree::iter::{BtreeIter, BtreeIterFlags, BtreeLevels, BtreeNodeIter, BtreeNodeRef, CommitFlags,
                         TransAttempt, TransBkey, UpdateTriggerFlags};
use crate::btree::write_buffer::{WbFlush, WbMaybeFlush};
use crate::c;
use crate::c::bch_errcode;
use crate::data::ptrs::{BkeyVPtrs, Ptrs};
use crate::errcode::{ret_to_c, ret_to_result_void, BchError};
use crate::fs::Fs;
use crate::init::damage;
use crate::init::error::id;
use crate::init::progress::Progress;
use crate::inode_opts::get_io_opts;
use crate::util::ffi::Opaque;
use crate::util::Printbuf;
use crate::prt;
use core::ffi::{c_int, c_uint};
use core::mem::size_of;
use cstruct_macros::c_export;

// data/ec/init.c's, until it converts
unsafe extern "C" {
    fn bch2_invalidate_stripe_to_dev(trans: *mut c::btree_trans, iter: *mut c::btree_iter, k: c::bkey_s_c,
                                     dev_idx: c_uint, flags: c_uint, err: *mut c::printbuf,
                                     had_open_stripe: *mut bool, recorded_stripe: *mut u64) -> c_int;
    fn bch2_fs_ec_flush_outstanding(c: *mut c::bch_fs);
}

/// A backpointer to a node being replaced: its replacement isn't reachable
/// yet, and has backpointers of its own for the next scan.
fn unless_overwritten<T>(r: Result<Option<T>, BchError>) -> Result<Option<T>, BchError> {
    match r {
        Err(e) if e.matches(bch_errcode::BCH_ERR_backpointer_to_overwritten_btree_node) => Ok(None),
        r => r,
    }
}

struct DevDrop<'e> {
    dev:   u32,
    flags: u32,
    err:   &'e mut Printbuf,
}

impl DevDrop<'_> {
    fn drop_ptrs<'a, 't>(&mut self, t: &TransAttempt<'a, 't>, k: BkeySC<'_, Ptrs>)
        -> Result<Option<TransBkey<'a, 't>>, BchError>
    {
        let fs = t.fs();
        let metadata = k.k.is_btree_ptr();
        let (replicas, lost, degraded, room) = if metadata {
            (fs.opts.metadata_replicas, c::BCH_FORCE_IF_METADATA_LOST, c::BCH_FORCE_IF_METADATA_DEGRADED,
             c::BKEY_BTREE_PTR_U64s_MAX)
        } else {
            (fs.opts.data_replicas, c::BCH_FORCE_IF_DATA_LOST, c::BCH_FORCE_IF_DATA_DEGRADED,
             c::BKEY_EXTENT_U64s_MAX)
        };

        if k.has_device(fs, self.dev).is_none() {
            return Ok(None);
        }

        let mut n = t.bkey_reassemble_room(k, room * size_of::<u64>())?;
        n.drop_device(fs, self.dev);
        let mut n: TransBkey<'a, 't> = n.into();

        let durability = n.as_sc().durability(t)?.total;
        if (durability == 0 && self.flags & lost == 0) || (durability < replicas && self.flags & degraded == 0) {
            prt!(self.err, "cannot drop device without degrading/losing data\n  {}\n", k.to_text(fs));
            return fs.throw(bch_errcode::BCH_ERR_remove_would_lose_data);
        }

        if n.as_sc().can_read(fs) {
            let opts = get_io_opts(t, None, k.into())?;
            n.set_needs_reconcile(t, None, &opts, c::set_needs_reconcile_ctx::SET_NEEDS_RECONCILE_opt_change, 0)?;
        } else if !metadata {
            n.set_error(fs, c::bch_key_type_errors::KEY_TYPE_ERROR_device_removed);
        }
        Ok(Some(n))
    }

    fn btree_node<'a>(&mut self, t: &TransAttempt<'a, '_>, iter: &mut BtreeIter<'a>, b: BtreeNodeRef)
        -> Result<(), BchError>
    {
        let Some(n) = self.drop_ptrs(t, b.key_sc().into())? else { return Ok(()) };
        t.btree_node_update_key(iter, b, n, CommitFlags::empty(), false)
    }

    fn usrdata_key<'a>(&mut self, t: &TransAttempt<'a, '_>, iter: &BtreeIter<'a>, k: BkeySC<'_, Ptrs>)
        -> Result<(), BchError>
    {
        let Some(mut n) = self.drop_ptrs(t, k)? else { return Ok(()) };

        if n.k().key_type() == c::bch_bkey_type::KEY_TYPE_error {
            damage::record_data_loss(t, iter.btree(), k.k.p, id::data_lost_device_removed)?;
        }

        // Not through an extent iterator - an all_snapshots one isn't - so a
        // deletion is a plain key's, not an extent overwrite's
        if n.k().is_deleted() {
            n.k_mut().size = 0;
        }
        t.update(iter, &n, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)
    }

    fn usrdata(&mut self, fs: &Fs, progress: &Progress<'_>) -> Result<(), BchError> {
        let trans = crate::btree_trans!(fs);
        let res = DiskReservation::new(fs);

        // FIXME: this does not handle unknown btrees with data pointers
        for btree in (0..c::btree_id::BTREE_ID_NR.0).map(c::btree_id) {
            if !btree.has_data_ptrs() || btree == c::btree_id::stripes {
                continue;
            }

            BtreeIter::new(&trans, btree, POS_MIN, BtreeIterFlags::PREFETCH | BtreeIterFlags::ALL_SNAPSHOTS)
                .for_each_commit(&trans, Some(&res), CommitFlags::NO_ENOSPC, |t, iter, k| {
                    res.put();
                    progress.update(t, iter)?;
                    let Some(k) = k.arm::<Ptrs>() else { return Ok(()) };
                    self.usrdata_key(t, iter, k)
                })?;
        }
        Ok(())
    }

    fn metadata(&mut self, fs: &Fs, progress: &Progress<'_>) -> Result<(), BchError> {
        if self.flags & c::BCH_FORCE_IF_METADATA_LOST != 0 {
            return fs.throw(bch_errcode::BCH_ERR_remove_with_metadata_missing_unimplemented);
        }

        let trans = crate::btree_trans!(fs);

        for btree in (0..fs.btree_id_nr_alive()).map(c::btree_id) {
            BtreeLevels { btree, levels: 1..c::BTREE_MAX_DEPTH + 1, start: POS_MIN, end: SPOS_MAX }
                .for_each_btree_ptr_commit(&trans, None, CommitFlags::empty(), |t, iter, level, k| {
                    progress.update(t, iter)?;

                    let mut node_iter = BtreeNodeIter::new(t, btree, k.k.p, 0, level - 1, BtreeIterFlags::empty());
                    let b = node_iter.peek_ref()?.expect("the node a btree node pointer points to");
                    self.btree_node(t, node_iter.iter(), b)
                })?;
        }

        trans.unlock();
        unsafe { c::bch2_btree_interior_updates_flush(fs.as_ptr()) };
        Ok(())
    }

    fn bp(&mut self, t: &TransAttempt<'_, '_>, bp: BkeySC<'_, c::Backpointer>, mut last_flushed: WbFlush<'_>,
          had_open_stripe: &mut bool, recorded_stripe: &mut u64) -> Result<(), BchError>
    {
        let fs = t.fs();
        let mut iter = BtreeIter::uninit();
        let r = backpointer_get_key(t, bp, &mut iter, BtreeIterFlags::INTENT, last_flushed.reborrow());
        let Some((iter, k)) = unless_overwritten(r)? else { return Ok(()) };
        let Some(k) = k.arm::<Ptrs>().filter(|k| k.has_device(fs, self.dev).is_some()) else {
            return Ok(());
        };

        // For a node pointer, @iter may be at the node or above it: the update
        // needs an iterator holding the node
        if k.k.is_btree_ptr() {
            let mut iter = BtreeIter::uninit();
            let Some(b) = unless_overwritten(backpointer_get_node(t, bp, &mut iter, last_flushed))? else {
                return Ok(());
            };
            let b = BtreeNodeRef::from(b);
            self.btree_node(t, &mut iter, b)
        } else if k.k.key_type() == c::bch_bkey_type::KEY_TYPE_stripe {
            // XXX: pass flags arg to invalidate_stripe_to_dev and handle it properly
            ret_to_result_void(unsafe {
                bch2_invalidate_stripe_to_dev(t.raw(), iter.raw(), k.into(), self.dev, self.flags,
                                              self.err.as_raw(), had_open_stripe, recorded_stripe)
            })
        } else {
            self.usrdata_key(t, iter, k)
        }
    }
}

fn dev_data_drop(fs: &Fs, mut d: DevDrop<'_>) -> Result<(), BchError> {
    let p = c::progress_indicator::default();

    let user = crate::BTREE_HAS_DATA_PTRS_MASK & !(1 << c::btree_id::stripes.0);
    d.usrdata(fs, &Progress::new(&p, fs, c"dropping user data", user, 0))?;
    d.metadata(fs, &Progress::new(&p, fs, c"dropping metadata", !0, !0))
}

/// Not its journal's or superblock's: removal deals with the journal after,
/// and the superblock goes with the device.
fn dev_data_buckets(ca: &c::bch_dev) -> u64 {
    ca.usage_read_fast().buckets.iter().enumerate()
        .filter(|&(i, _)| {
            let t = c::bch_data_type(i as u32);
            !data_type_is_empty(t) && !data_type_is_hidden(t)
        })
        .map(|(_, &nr)| nr)
        .sum()
}

fn dev_data_drop_by_backpointers(fs: &Fs, ca: &c::bch_dev, mut d: DevDrop<'_>) -> Result<(), BchError> {
    const MAX_STALLED: u32 = 10;

    let trans = crate::btree_trans!(fs);
    let mut last_flushed = WbMaybeFlush::new()?;
    let p = c::progress_indicator::default();
    let progress = Progress::new(&p, fs, c"dropping device data", 1 << c::btree_id::backpointers.0, 0);

    let mut min_data_buckets = u64::MAX;
    let mut nr_stalled = 0;
    let mut recorded_stripe = 0;

    loop {
        let mut had_open_stripe = false;

        // The flushes wait on other threads' updates, which may need our locks
        trans.unlock();
        unsafe { c::bch2_btree_interior_updates_flush(fs.as_ptr()) };
        trans.write_buffer_flush_sync()?;

        BpScanIter::new(c::btree_id::backpointers, pos(d.dev.into(), 0), Some(&progress))
            .for_each(&trans, pos(d.dev.into(), u64::MAX), last_flushed.borrow(), |t, bp, mut last_flushed| {
                // What maybe_flush() rate limits its flushes against
                last_flushed.inc();
                let res = DiskReservation::new(fs);
                d.bp(t, bp, last_flushed, &mut had_open_stripe, &mut recorded_stripe)?;
                t.commit(Some(&res), CommitFlags::NO_ENOSPC)
            })?;

        let data_buckets = dev_data_buckets(ca);
        if data_buckets == 0 {
            return Ok(());
        }

        if data_buckets < min_data_buckets {
            min_data_buckets = data_buckets;
            nr_stalled = 0;
        } else {
            nr_stalled += 1;
            if nr_stalled >= MAX_STALLED {
                prt!(d.err, "bch2_dev_data_drop_by_backpointers(): no progress after {nr_stalled} scans, \
                             {data_buckets} data buckets remain\n");
                return fs.throw(bch_errcode::BCH_ERR_remove_by_backpointer_did_not_terminate);
            }
        }

        if had_open_stripe {
            trans.unlock();
            unsafe { bch2_fs_ec_flush_outstanding(fs.as_ptr()) };
        }
    }
}

#[c_export]
pub extern "C" fn bch2_dev_data_drop(c: &Opaque<c::bch_fs>, dev_idx: c_uint, flags: c_uint, err: &mut Printbuf)
    -> c_int
{
    ret_to_c(dev_data_drop(Fs::from_c(c), DevDrop { dev: dev_idx, flags, err }))
}

#[c_export]
pub extern "C" fn bch2_dev_data_drop_by_backpointers(c: &Opaque<c::bch_fs>, ca: &c::bch_dev, flags: c_uint,
                                                     err: &mut Printbuf) -> c_int {
    let d = DevDrop { dev: ca.dev_idx.into(), flags, err };
    ret_to_c(dev_data_drop_by_backpointers(Fs::from_c(c), ca, d))
}
