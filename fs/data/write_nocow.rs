// SPDX-License-Identifier: GPL-2.0

//! The write path's in place writes, over extents that allow them: nocow.

use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{bkey_ge, pos, spos, BkeyI, BkeySC, Typed, BKEY_U64S};
use crate::btree::iter::{is_restart, BtreeIter, BtreeIterFlags, CommitFlags, TransAttempt, UpdateTriggerFlags};
use crate::c;
use crate::data::nocow_locking::{bkey_lock_cas, bkey_unlock_cas, NocowKind};
use crate::data::ptrs::{crc_is_encoded, extents_match, Ptrs, PtrsMut};
use crate::data::write::{index_update_wq, to_wbio, wbio_init, submit_wbio_replicas, write_done,
                         write_endio, write_op_error};
use crate::errcode::{bch_errcode, BchError};
use crate::fs::{Dev, Fs};
use crate::format::{Btree, DataType};
use crate::snapshots::subvolume;
use crate::util::closure::Running;
use crate::util::kernel::{unlikely, WRITE};
use crate::util::rcu::read_lock;
use crate::{btree_trans, fs_inconsistent, inode};
use c::bch_write_flags as w;
use core::ops::ControlFlow;

type Op = c::bch_write_op;

fn extent_is_writeable(op: &Op, k: BkeySC<'_>) -> bool {
    let fs = op.fs();

    if k.k.key_type() != c::bch_bkey_type::KEY_TYPE_extent {
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
    new.set_needs_reconcile(t, None, &opts, c::set_needs_reconcile_ctx::SET_NEEDS_RECONCILE_foreground,
                            op.opts.change_cookie)?;
    t.update(iter, &new, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE | UpdateTriggerFlags::SET_NEEDS_RECONCILE_DONE)
}

fn nocow_write_convert_unwritten(op: &mut Op) {
    let fs = op.fs();

    let ret = {
        let trans = btree_trans!(fs);
        let op = &*op;
        let nr_u64s = op.insert_keys.u64s();
        let mut done_u64s = 0;

        op.insert_keys.iter().try_for_each(|orig| {
            done_u64s += usize::from(orig.k().u64s);
            let is_last = done_u64s == nr_u64s;

            let mut iter = BtreeIter::new(&trans, Btree::BTREE_ID_extents, orig.k().start_pos(),
                                          BtreeIterFlags::INTENT | BtreeIterFlags::TRIM);
            iter.for_each_max(&trans, orig.k().p, |t, iter, k| {
                let flush = op.has(w::BCH_WRITE_flush) && is_last && bkey_ge(k.k.p, orig.k().p);

                nocow_write_convert_one_unwritten(t, iter, op, orig, k)?;
                t.commit_flush(Some(&op.res), flush.then_some(&op.cl), CommitFlags::NO_ENOSPC)?;
                Ok(ControlFlow::<()>::Continue(()))
            }).map(|_| ())
        })
    };

    if let Err(e) = ret {
        if !e.matches(c::EROFS) {
            let insert = op.insert_keys.front().expect("keys left by an index update's error");
            write_op_error(op, insert.k().start_offset(), format_args!("btree update error: {e}"));
        }
        op.set_error(e);
    }
}

fn __nocow_write_done(op: &mut Op) {
    if unlikely(op.io_error != 0) {
        op.set_error(op.fs().err(bch_errcode::BCH_ERR_data_write_io));
    } else if unlikely(op.has(w::BCH_WRITE_convert_unwritten)) {
        nocow_write_convert_unwritten(op);
    }
}

extern "C" fn nocow_write_done(mut op: Running<Op>) {
    __nocow_write_done(&mut op);
    write_done(op);
}

/*
 * Take an io_ref per ptr, populating @cas (parallel to @ptrs) and
 * returning the number of refs taken on success.  On partial failure,
 * walks back through cas[] to drop the refs taken so far — never
 * re-derives ca via c->devs[idx], which dev_remove may have cleared
 * while our io_ref still pins the dev.
 */
fn bkey_get_dev_iorefs<'f>(fs: &'f Fs, k: BkeySC<'_, Ptrs>, cas: &mut [Option<&'f Dev>]) -> Option<usize> {
    let mut i = 0;
    for ptr in k.ptrs(fs) {
        cas[i] = fs.dev_get_ioref_kind(ptr.dev().into(), WRITE, IO_WRITE);
        if cas[i].is_none() {
            unsafe { put_iorefs(&cas[..i]) };
            return None;
        }
        i += 1;
    }

    Some(i)
}

const IO_WRITE: u32 = c::bch_dev_write_ref::BCH_DEV_WRITE_REF_io_write as u32;

/// # Safety
/// Each of @cas is a device whose io ref, from bkey_get_dev_iorefs(), is the
/// caller's to give up.
unsafe fn put_iorefs(cas: &[Option<&Dev>]) {
    for ca in cas.iter().flatten() {
        unsafe { ca.io_ref[WRITE].put_kind(IO_WRITE) };
    }
}

fn inode_get_i_size(t: &TransAttempt<'_, '_>, inode_pos: crate::format::Bpos) -> Result<u64, BchError> {
    let mut iter = BtreeIter::new(t, Btree::BTREE_ID_inodes, inode_pos, BtreeIterFlags::CACHED);
    let k = iter.peek_slot(t)?.expect("a slot");

    Ok(match k.typed() {
        Typed::inode_v3(i) => i.v().bi_size.get(),
        _ => inode::unpack(t.fs(), k).bi_size,
    })
}

/// # Safety
/// @dst has room for @k.
unsafe fn bkey_reassemble(dst: *mut c::bkey_i, k: BkeySC<'_>) {
    let val_u64s = usize::from(k.k.u64s) - BKEY_U64S;
    unsafe {
        (*dst).k = *k.k;
        core::ptr::copy_nonoverlapping(k.v.cast::<u64>(), dst.cast::<u64>().add(BKEY_U64S), val_u64s);
    }
}

/// The op's bio written in place, as much as its extents allow, in @t: the
/// pointers overwritten written, each bio submitted, and the keys to convert
/// from unwritten on the keylist. Stops, falling back, at what can't be.
fn nocow_write_submit(t: &TransAttempt<'_, '_>, op: &mut Running<Op>) -> Result<(), BchError> {
    let fs = t.fs();

    let snapshot = subvolume::get_snapshot(t, op.subvol)?;

    let i_size = inode_get_i_size(t, spos(0, op.pos.inode, snapshot))?;

    if op.new_i_size > i_size {
        op.set(w::BCH_WRITE_convert_unwritten);
    }

    let mut iter = BtreeIter::new(t, Btree::BTREE_ID_extents, spos(op.pos.inode, op.pos.offset, snapshot),
                                  BtreeIterFlags::SLOTS | BtreeIterFlags::TRIM);
    loop {
        let sectors = u64::from(op.wbio.bio.bi_iter.bi_size >> 9);

        t.relock()?;

        iter.set_pos(spos(op.pos.inode, op.pos.offset, snapshot));
        let Some(k) = iter.peek_slot_max(t, pos(op.pos.inode, op.pos.offset + sectors))? else {
            return Ok(());
        };

        /* fall back to normal cow write path? */
        if unlikely(k.k.p.snapshot != snapshot || !extent_is_writeable(op, k)) {
            return Ok(());
        }

        /*
         * Extent end may not be block-aligned (e.g. after
         * truncate). Can't split a nocow bio at a non-aligned
         * point — fall back to COW which allocates new blocks.
         */
        if k.k.p.offset < op.pos.offset + sectors &&
           (k.k.p.offset - op.pos.offset) & (u64::from(fs.block_sectors()) - 1) != 0 {
            return Ok(());
        }

        if op.keys_realloc(k.k.u64s.into()).is_err() {
            return Ok(());
        }

        /* Get iorefs before dropping btree locks: */
        let mut cas = [None; c::BCH_BKEY_PTRS_MAX as usize];
        let Some(nr_cas) = bkey_get_dev_iorefs(fs, k.arm::<Ptrs>().expect("a writeable extent"), &mut cas) else {
            return Ok(());
        };
        let cas = &cas[..nr_cas];

        /* Unlock before taking nocow locks, doing IO: */
        let top = op.insert_keys.top();
        unsafe { bkey_reassemble(top, k) };
        let k = BkeySC::from(unsafe { &*BkeyI::from_raw(top) });

        t.unlock();

        bkey_lock_cas(fs, Some(t.trans()), k, cas, NocowKind::Update);

        /*
         * This could be handled better: If we're able to trylock the
         * nocow locks with btree locks held we know dirty pointers
         * can't be stale
         */
        {
            let rcu = read_lock();
            let ptrs = k.arm::<Ptrs>().expect("a writeable extent");
            for (ptr, ca) in ptrs.ptrs(fs).zip(cas.iter().flatten()) {
                let stale = ca.ptr_stale(ptr, &rcu);
                if unlikely(stale != Some(0)) {
                    let ret = if stale.is_none() {
                        fs_inconsistent!(fs, "pointer to invalid bucket in nocow path on device {}\n  {}",
                                         ptr.dev(), k.to_text(fs));
                        fs.err(bch_errcode::BCH_ERR_data_write_invalid_ptr)
                    } else {
                        /* We can retry this: */
                        fs.err(bch_errcode::BCH_ERR_transaction_restart)
                    };

                    bkey_unlock_cas(fs, k, cas, NocowKind::Update);
                    unsafe { put_iorefs(cas) };

                    /* Fall back to COW path: */
                    return Err(ret);
                }

                if ptr.unwritten() != 0 {
                    op.set(w::BCH_WRITE_convert_unwritten);
                }
            }
        }

        let bio: *mut c::bio = if k.k.p.offset < op.pos.offset + sectors {
            unsafe {
                let bio = c::bio_split(&raw mut op.wbio.bio, (k.k.p.offset - op.pos.offset) as i32,
                                       c::GFP_KERNEL, fs.bio_write.as_ptr());
                wbio_init(bio).wbio.bits.set_put_bio(1);
                (*bio).bi_opf = op.wbio.bio.bi_opf;
                bio
            }
        } else {
            op.set(w::BCH_WRITE_submitted);
            &raw mut op.wbio.bio
        };

        let bio = unsafe { &mut *bio };
        let bio_sectors = bio.bi_iter.bi_size >> 9;
        op.pos.offset += u64::from(bio_sectors);
        op.written += bio_sectors;

        bio.bi_end_io  = Some(write_endio);
        bio.bi_private = op.closure().into_ref().as_ptr().cast();
        bio.bi_opf    |= c::req_op_REQ_OP_WRITE;
        if op.has(w::BCH_WRITE_flush) {
            bio.bi_opf |= c::REQ_FUA;
        }

        op.closure().get().leak();

        unsafe { submit_wbio_replicas(to_wbio(bio), fs, DataType::user, &*BkeyI::from_raw(top), Some(cas)) };

        if op.has(w::BCH_WRITE_convert_unwritten) {
            unsafe { op.insert_keys.push() };
        }
        if op.has(w::BCH_WRITE_submitted) {
            return Ok(());
        }
    }
}

/// The op written in place - Ok, its closure's continued - or, where it
/// can't be, given back for the cow write path.
pub(crate) fn nocow_write(mut op: Running<Op>) -> Result<(), Running<Op>> {
    let fs = op.fs();

    if op.has(w::BCH_WRITE_move) {
        return Err(op);
    }

    op.clear(w::BCH_WRITE_convert_unwritten);

    let ret = {
        let trans = btree_trans!(fs);
        loop {
            let t = trans.begin();
            match nocow_write_submit(&t, &mut op) {
                Err(e) if is_restart(&e) => continue,
                r => break r,
            }
        }
    };

    if let Err(e) = ret {
        write_op_error(&op, op.pos.offset, format_args!("bch2_nocow_write(): btree lookup error: {e}"));
        op.set_error(e);
        op.set(w::BCH_WRITE_submitted);
    }

    if !op.has(w::BCH_WRITE_submitted) {
        /* fallback to cow write path */
        op.sync();
        __nocow_write_done(&mut op);
        op.insert_keys.top_p = op.insert_keys.keys_p;
        Err(op)
    } else if op.has(w::BCH_WRITE_sync) {
        op.sync();
        nocow_write_done(op);
        Ok(())
    } else {
        /*
         * XXX
         * needs to run out of process context because ei_quota_lock is
         * a mutex
         */
        let wq = index_update_wq(&op);
        op.continue_at(nocow_write_done, Some(wq));
        Ok(())
    }
}
