// SPDX-License-Identifier: GPL-2.0

//! The write path's in place writes, over extents that allow them: nocow.

use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{bkey_ge, pos, spos, BkeyI, BkeySC};
use crate::btree::iter::{lockrestart_do, BtreeIter, BtreeIterFlags, CommitFlags, TransAttempt, UpdateTriggerFlags};
use crate::c;
use crate::journal::flush_wait::Flush;
use crate::data_structures::dst_room::DstRoom;
use crate::data::nocow_locking::{bkey_lock_cas, bkey_unlock_cas, NocowKind};
use crate::data::ptrs::{crc_is_encoded, extents_match, Ptrs, PtrsMut};
use crate::data::write::{index_update_wq, write_op_error, DevWriteRef, WriteFor};
use crate::data::write_replicas::{InFlight, ReplicaErrs};
use crate::data_structures::array_vec::ArrayVec;
use crate::os::bio::BioRange;
use crate::errcode::{Errcode, BchError};
use crate::fs::{Dev, Fs};
use crate::format::{Btree, DataType, BkeyType};
use crate::snapshots::subvolume;
use crate::os::kernel::unlikely;
use crate::util::rcu::read_lock;
use crate::util::task::Resume;
use crate::{btree_trans, fs_inconsistent, inode};
use c::WriteFlags as w;
use core::ops::ControlFlow;

type Op = c::bch_write_op;

fn extent_is_writeable(op: &Op, k: BkeySC<'_>) -> bool {
    let fs = op.fs();

    if k.k.key_type() != BkeyType::extent {
        return false;
    }

    let Some(e) = k.arm::<Ptrs>() else { return false };

    let rcu = read_lock();
    let mut replicas = 0;
    for p in e.ptrs_decoded(fs) {
        if crc_is_encoded(&p.p.crc) || p.p.has_ec {
            return false;
        }

        replicas += if p.ptr.cached() == 0 { fs.dev_durability(p.ptr.dev().into(), &rcu) } else { 0 };
    }

    replicas >= op.opts.data_replicas.into()
}

fn nocow_write_convert_one_unwritten<'t>(t: &TransAttempt<'_, 't>, iter: &BtreeIter<'t>, op: &Op,
                                         orig: &BkeyI, k: BkeySC<'_>)
    -> Result<(), BchError>
{
    let fs = t.fs();

    if !extents_match(fs, BkeySC::from(orig), k) {
        /* trace this */
        return Ok(());
    }

    /*
     * If we did a degraded write, bch2_bkey_set_needs_reconcile() will add
     * pointers to BCH_SB_MEMBER_INVALID so the extent is accounted as
     * degraded
     */
    let new_buf_u64s = usize::from(k.k.u64s) + 1 + c::BCH_REPLICAS_MAX as usize;
    let mut new = t.bkey_reassemble_room(k, new_buf_u64s * size_of::<u64>())?;

    if let Some(mut ptrs) = new.reborrow().arm::<PtrsMut>() {
        ptrs.ptrs_mut(fs).for_each(|ptr| ptr.set_unwritten(0));
    }

    /*
     * Note that we're not calling bch2_subvol_get_snapshot() in this path -
     * that was done when we kicked off the write, and here it's important
     * that we update the extent that we wrote to - even if a snapshot has
     * since been created. The write is still outstanding, so we're ok
     * w.r.t. snapshot atomicity:
     */
    let mut opts = c::bch_inode_opts::default();

    t.extent_update_i_size_sectors(iter, (new.k().p.offset << 9).min(op.new_i_size), 0, &mut opts)?;
    new.set_needs_reconcile(t, None, &opts, c::SetNeedsReconcileCtx::SET_NEEDS_RECONCILE_foreground,
                            op.opts.change_cookie)?;
    t.update(iter, new, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE | UpdateTriggerFlags::SET_NEEDS_RECONCILE_DONE)
}

fn nocow_write_convert_unwritten(op: &mut Op, mut flush: Option<&mut Option<Flush<'_>>>) {
    let fs = op.fs();

    let ret = {
        let trans = btree_trans!(fs);
        let op = &*op;
        let nr_u64s = op.insert_keys.u64s();
        let mut done_u64s = 0;

        op.insert_keys.iter().try_for_each(|orig| {
            done_u64s += usize::from(orig.k().u64s);
            let is_last = done_u64s == nr_u64s;

            let mut iter = BtreeIter::new(&trans, Btree::extents, orig.k().start_pos(),
                                          BtreeIterFlags::INTENT | BtreeIterFlags::TRIM);
            iter.for_each_max(&trans, orig.k().p, |t, iter, k| {
                let flush = flush.as_deref_mut().filter(|_| is_last && bkey_ge(k.k.p, orig.k().p));

                nocow_write_convert_one_unwritten(t, iter, op, orig, k)?;
                t.commit_flush(Some(&op.res), flush, CommitFlags::NO_ENOSPC)?;
                Ok(ControlFlow::<()>::Continue(()))
            }).map(|_| ())
        })
    };

    if let Err(e) = ret {
        if !e.matches(c::EROFS) {
            let insert = op.insert_keys.front().expect("keys left by an index update's error");
            write_op_error(op, None, insert.k().start_offset(), format_args!("btree update error: {e}"));
        }
        op.set_error(e);
    }
}

/// With w::flush, the last asking for the write's journal flush.
fn nocow_write_done(op: &mut Op, flush: Option<&mut Option<Flush<'_>>>) {
    if unlikely(op.io_error != 0) {
        op.set_error(op.fs().err(Errcode::data_write_io));
    } else if unlikely(op.has(w::convert_unwritten)) {
        nocow_write_convert_unwritten(op, flush.filter(|_| op.has(w::submitted)));
    }
}

/*
 * Take an io_ref per ptr, populating @cas (parallel to @ptrs) and
 * returning the number of refs taken on success.  On partial failure,
 * the refs taken so far go with cas[] — never re-derived via
 * c->devs[idx], which dev_remove may have cleared while our io_ref
 * still pins the dev.
 */
fn bkey_get_dev_iorefs(fs: &Fs, k: BkeySC<'_, Ptrs>, cas: &mut [Option<DevWriteRef>]) -> Option<usize> {
    let mut i = 0;
    for ptr in k.ptrs(fs) {
        cas[i] = Some(fs.dev_get_ioref::<c::DevWriteRefKind>(ptr.dev().into())?);
        i += 1;
    }

    Some(i)
}

fn inode_get_i_size(t: &TransAttempt<'_, '_>, inode_pos: crate::format::Bpos) -> Result<u64, BchError> {
    let mut iter = BtreeIter::new(t, Btree::inodes, inode_pos, BtreeIterFlags::CACHED);
    let Some(k) = iter.peek_slot(t)?.expect("a slot").arm::<inode::Inodes>() else {
        return Err(t.fs().err(Errcode::ENOENT_inode));
    };

    Ok(match k.typed() {
        inode::InodesTyped::inode_v3(i) => i.v().bi_size.get(),
        _ => inode::unpack(t.fs(), k).bi_size,
    })
}

/// An extent written in place: its part of the op's data, the key it
/// overwrites, and an io ref for each of its pointers, its buckets locked.
struct NocowExtent<'a> {
    data: BioRange<'a>,
    k:    DstRoom<BkeyI, [u64; c::BKEY_EXTENT_U64s_MAX as usize]>,
    cas:  [Option<DevWriteRef>; c::BCH_BKEY_PTRS_MAX as usize],
    nr:   usize,
}

type NocowExtents<'a> = ArrayVec<NocowExtent<'a>, { c::BCH_BKEY_PTRS_MAX as usize }>;

/// The write's data - @data, a position in its bio, advanced past what's
/// done - written in place, as much as its extents allow, in @t: each extent
/// to write in @extents, its buckets locked, and the keys to convert from
/// unwritten on the keylist. Stops, falling back, at what can't be - or,
/// true, for another pass, with as many as @extents holds.
fn nocow_write_submit<'a>(t: &TransAttempt<'_, '_>, op: &mut Op, data: &mut BioRange<'a>,
                          extents: &mut NocowExtents<'a>) -> Result<bool, BchError>
{
    let fs = t.fs();

    let snapshot = subvolume::get_snapshot(t, op.subvol)?;

    let i_size = inode_get_i_size(t, spos(0, op.pos.inode, snapshot))?;

    if op.new_i_size > i_size {
        op.set(w::convert_unwritten);
    }

    let mut iter = BtreeIter::new(t, Btree::extents, spos(op.pos.inode, op.pos.offset, snapshot),
                                  BtreeIterFlags::SLOTS | BtreeIterFlags::TRIM);
    loop {
        let sectors = u64::from(data.len() >> 9);

        t.relock()?;

        iter.set_pos(spos(op.pos.inode, op.pos.offset, snapshot));
        let Some(k) = iter.peek_slot_max(t, pos(op.pos.inode, op.pos.offset + sectors))? else {
            return Ok(false);
        };

        /* fall back to normal cow write path? */
        if unlikely(k.k.p.snapshot != snapshot || !extent_is_writeable(op, k)) {
            return Ok(false);
        }

        /*
         * Extent end may not be block-aligned (e.g. after
         * truncate). Can't split a nocow bio at a non-aligned
         * point — fall back to COW which allocates new blocks.
         */
        if k.k.p.offset < op.pos.offset + sectors &&
           (k.k.p.offset - op.pos.offset) & (u64::from(fs.block_sectors()) - 1) != 0 {
            return Ok(false);
        }

        if op.insert_keys.reserve(k.k.u64s.into()).is_err() {
            return Ok(false);
        }

        /* Get iorefs before dropping btree locks: */
        let mut refs = [const { None }; c::BCH_BKEY_PTRS_MAX as usize];
        let Some(nr_cas) = bkey_get_dev_iorefs(fs, k.arm::<Ptrs>().expect("a writeable extent"), &mut refs) else {
            return Ok(false);
        };
        let cas = &refs[..nr_cas];
        let devs: [Option<&Dev>; c::BCH_BKEY_PTRS_MAX as usize] =
            core::array::from_fn(|i| cas.get(i).and_then(Option::as_deref));
        let devs = &devs[..nr_cas];

        /* Unlock before taking nocow locks, doing IO: */
        let mut copy = DstRoom::<BkeyI, [u64; c::BKEY_EXTENT_U64s_MAX as usize]>::default();
        if copy.reassemble(k).is_err() {
            return Ok(false);
        }
        let k = BkeySC::from(&*copy);

        t.unlock();

        bkey_lock_cas(fs, Some(t), k, devs, NocowKind::Update);

        /*
         * This could be handled better: If we're able to trylock the
         * nocow locks with btree locks held we know dirty pointers
         * can't be stale
         */
        {
            let rcu = read_lock();
            let ptrs = k.arm::<Ptrs>().expect("a writeable extent");
            for (ptr, ca) in ptrs.ptrs(fs).zip(devs.iter().flatten()) {
                let stale = ca.ptr_stale(ptr, &rcu);
                if unlikely(stale != Some(0)) {
                    let ret = if stale.is_none() {
                        fs_inconsistent!(fs, "pointer to invalid bucket in nocow path on device {}\n  {}",
                                         ptr.dev(), k.to_text(fs));
                        fs.err(Errcode::data_write_invalid_ptr)
                    } else {
                        /* We can retry this: */
                        fs.err(Errcode::transaction_restart)
                    };

                    bkey_unlock_cas(fs, k, devs, NocowKind::Update);

                    /* Fall back to COW path: */
                    return Err(ret);
                }

                if ptr.unwritten() != 0 {
                    op.set(w::convert_unwritten);
                }
            }
        }

        let bytes = if k.k.p.offset < op.pos.offset + sectors {
            ((k.k.p.offset - op.pos.offset) as u32) << 9
        } else {
            op.set(w::submitted);
            data.len()
        };
        let range = data.take(bytes);
        data.advance(bytes);

        op.pos.offset += u64::from(bytes >> 9);
        op.written += bytes >> 9;

        if op.has(w::convert_unwritten) {
            op.insert_keys.push_key(&copy);
        }

        extents.push(NocowExtent { data: range, k: copy, cas: refs, nr: nr_cas });

        if op.has(w::submitted) {
            return Ok(false);
        }
        if extents.is_full() {
            return Ok(true);
        }
    }
}

/// A pass of the op's in-place write, as write_pass() is of a cow write's:
/// its extents, from @data, a position in its bio, written - true if it
/// stopped with @extents full, for the next pass to go on from. What waiting
/// it does is resumed @at.
async fn nocow_write_pass<'a>(op: &mut Op, data: &mut BioRange<'a>, opf: u32, at: Resume<'_>)
    -> (bool, ReplicaErrs)
{
    let mut extents = NocowExtents::new();
    let ret = {
        let trans = btree_trans!(op.fs());
        lockrestart_do(&trans, |t| nocow_write_submit(t, op, data, &mut extents))
    };
    // its transaction gone first: the error's message has one of its own
    let more = ret.unwrap_or_else(|e| {
        write_op_error(op, None, op.pos.offset, format_args!("bch2_nocow_write(): btree lookup error: {e}"));
        op.set_error(e);
        op.set(w::submitted);
        false
    });

    /* what was taken before an error is written still: its buckets are locked */
    let mut opf = opf | c::req_op_REQ_OP_WRITE;
    if op.has(w::flush) {
        opf |= c::REQ_FUA;
    }
    let mut io = InFlight::new(op.fs());
    for e in extents.iter_mut() {
        io.submit::<c::DevWriteRefKind>(e.data, opf, DataType::user, &e.k, Some(&mut e.cas[..e.nr]), at).await;
    }
    (more, io.wait(at).await)
}

/// The op written in place, a pass at a time: true - or, where it can't be,
/// false, for the cow write path to write the rest. @in_caller cleared once
/// it's waited for IO.
pub(crate) async fn nocow_write(op: &mut Op, wbio: &mut c::bch_write_bio, of: WriteFor<'_>,
                                mut flush: Option<&mut Option<Flush<'_>>>, in_caller: &mut bool) -> bool
{
    if op.has(w::update) {
        return false;
    }

    op.clear(w::convert_unwritten);

    loop {
        /*
         * XXX
         * needs to run out of process context because ei_quota_lock is
         * a mutex
         */
        let at = Resume::queue(index_update_wq(op));
        let (more, errs, pos) = {
            let (mut data, opf) = (wbio.bio.range(), wbio.bio.opf());
            let (more, errs) = nocow_write_pass(op, &mut data, opf, at).await;
            (more, errs, data.pos())
        };
        wbio.bio_mut().set_iter(pos);
        errs.record(op, of);
        if op.written != 0 {
            *in_caller = false;
        }

        nocow_write_done(op, flush.as_deref_mut());

        if op.has(w::submitted) {
            return true;
        }

        op.insert_keys.clear();
        if !more {
            /* fallback to cow write path */
            return false;
        }
    }
}
