// SPDX-License-Identifier: GPL-2.0

//! O_DIRECT writes: direct.c's write half. A dio_write, from the dio write
//! bioset, is a write op over the user's pages, a bio's worth at a time: a
//! sync kiocb's loop runs in the call, an async one's continues from each
//! write's end_io.

use crate::btree::bkey::{pos, spos};
use crate::btree::iter::{lockrestart_do, BtreeIter, BtreeIterFlags};
use crate::btree_trans;
use crate::c;
use crate::data::durability::BkeyVDurability;
use crate::data::write::bch2_write;
use crate::errcode::{Errcode, BchError};
use crate::format::Btree;
use crate::fs::Fs;
use crate::inode_opts;
use crate::snapshots::subvolume;
use crate::util::alloc::kmalloc;
use crate::util::bio::Bio;
use crate::util::closure;
use crate::os::iov_iter::{IovIter, IovIterFields};
use crate::os::kernel::{current_id, likely, task_io_account_write, unlikely, Mm};
use crate::util::refcount::TryRefcount;
use crate::vfs::folio::Mapping;
use crate::vfs::inode::InodeInfo;
use crate::vfs::kiocb::Kiocb;
use c::WriteFlags as w;
use core::ops::ControlFlow;
use core::ptr::NonNull;

type Dio = c::dio_write;

fn check_range_allocated(fs: &Fs, inum: c::subvol_inum, mut offset: u64, size: u64,
                         nr_replicas: u32, compressed: bool) -> bool {
    let trans = btree_trans!(fs);
    let end = offset + size;
    let mut allocated = true;

    let ret = lockrestart_do(&trans, |t| {
        let snapshot = subvolume::get_snapshot(t, inum.subvol as u32)?;

        let mut iter = BtreeIter::new(t, Btree::extents, spos(inum.inum, offset, snapshot),
                                      BtreeIterFlags::SLOTS | BtreeIterFlags::TRIM);
        iter.for_each_max_norestart(t, pos(inum.inum, end), |iter, k| {
            offset = iter.pos().offset;

            let d = k.durability_safe(fs);
            if k.k.p.snapshot != snapshot ||
               nr_replicas > u32::from(d.total) ||
               (!compressed && d.sectors_compressed != 0) {
                allocated = false;
                return Ok(ControlFlow::Break(()));
            }
            Ok(ControlFlow::Continue(()))
        })
    });

    ret.is_ok() && allocated
}

impl c::dio_write {
    /// 'static for the dio: its DioStarted, the i_dio_count, is its ref on
    /// the inode, and thus on the filesystem.
    fn inode(&self) -> &'static InodeInfo {
        unsafe { InodeInfo::from_inode(self.inode.cast()) }
    }

    fn bio_mut(&mut self) -> &mut c::bio {
        &mut self.op.wbio.bio
    }

    fn bio_sectors(&self) -> u64 {
        u64::from(self.op.wbio.bio.bi_iter.bi_size >> 9)
    }
}

/// The dio of a write op's end_io.
///
/// # Safety
/// @op is a dio_write's.
unsafe fn of_op(op: *mut c::bch_write_op) -> *mut Dio {
    op.wrapping_byte_sub(core::mem::offset_of!(Dio, op)).cast()
}

#[inline(never)]
fn dio_write_check_allocated(dio: &Dio) -> bool {
    check_range_allocated(dio.op.fs(), dio.inode().inum(), dio.op.pos.offset, dio.bio_sectors(),
                          dio.op.opts.data_replicas.into(), dio.op.opts.compression != 0)
}

/*
 * We're going to return -EIOCBQUEUED, but we haven't finished consuming the
 * iov_iter yet, so we need to stash a copy of the iovec: it might be on the
 * caller's stack, we're not guaranteed that it will live for the duration of
 * the IO:
 */
#[inline(never)]
fn dio_write_copy_iov(dio: &mut Dio) -> Result<(), ()> {
    /*
     * iov_iter has a single embedded iovec - nothing to do:
     */
    if dio.iter.is_ubuf() {
        return Ok(());
    }

    /*
     * We don't currently handle non-iovec iov_iters here - return an error,
     * and we'll fall back to doing the IO synchronously:
     */
    if !dio.iter.is_iovec() {
        return Err(());
    }

    let nr_segs = dio.iter.nr_segs();
    let mut iov = dio.inline_vecs.as_mut_ptr();

    if nr_segs > dio.inline_vecs.len() {
        iov = kmalloc(nr_segs * size_of::<c::iovec>()).map_err(|_| ())?.as_ptr().cast();
        dio.iov = iov;
    }

    unsafe {
        core::ptr::copy_nonoverlapping(dio.iter.iov().cast(), iov, nr_segs);
        dio.iter.set_iov(iov.cast());
    }
    Ok(())
}

/// The dio done, and freed: what the write wrote, or its error - for an async
/// kiocb, completed, and -EIOCBQUEUED.
///
/// # Safety
/// @dio is ours, and nothing uses it after.
#[inline(always)]
unsafe fn dio_write_done(dio: *mut Dio) -> isize {
    let d = unsafe { &mut *dio };
    let fs = d.op.fs();
    let req = d.req;
    let sync = d.bits.sync() != 0;

    unsafe { c::rust_kfree(d.iov.cast()) };

    let mut ret = if d.op.error != 0 { isize::from(d.op.error) } else { (d.written << 9) as isize };
    let (write_ref, started) = (d.write_ref.take(), d.started.take());
    unsafe { c::bio_put(d.bio_mut()) };

    drop(write_ref);

    /* inode->i_dio_count is our ref on inode and thus bch_fs */
    drop(started);

    if ret < 0 {
        ret = BchError::from_raw(ret as i32).class() as isize;
    }

    if !sync {
        unsafe { ((*req).ki_complete.expect("an async kiocb's completion"))(req, ret) };
        ret = -(c::EIOCBQUEUED as isize);
    }
    ret
}

#[inline(always)]
fn dio_write_end(dio: &mut Dio) {
    let req = unsafe { &mut *dio.req };
    let inode = dio.inode();

    req.ki_pos += (u64::from(dio.op.written) << 9) as i64;
    dio.written += u64::from(dio.op.written);

    if dio.bits.extending() != 0 {
        inode.v.size_extend(req.ki_pos as u64);
    }

    if dio.op.i_sectors_delta != 0 || dio.quota_res.sectors != 0 {
        inode.i_sectors_acct_put(&mut dio.quota_res, dio.op.i_sectors_delta);
    }

    unsafe { c::bio_release_pages(dio.bio_mut(), false) };

    if unlikely(dio.op.error != 0) {
        inode.ei_flags.set(c::InodeInfoFlag::EI_INODE_ERROR, true);
    }
}

unsafe fn dio_write_sync_done(op: *mut c::bch_write_op) {
    let dio = unsafe { &mut *of_op(op) };

    dio.bits.set_sync_done(1);
}

/// # Safety
/// @dio is ours: for an async kiocb, until it's queued its write.
#[inline(always)]
unsafe fn dio_write_loop(dio: *mut Dio) -> isize {
    let d = unsafe { &mut *dio };
    let req = d.req;
    let mapping = d.mapping;
    let inode = d.inode();
    let fs = inode.fs();
    let opts = inode_opts::get_inode(fs, &inode.inode());
    let mut sync = d.bits.sync() != 0;

    let ret: Result<(), BchError> = loop {
        let d = unsafe { &mut *dio };
        let iter_count = d.iter.count();

        debug_assert!(fs.fdm_table.get().is_none(), "a DIO write with page faults disabled already");
        let fdm = fs.fdm_table.set(mapping);

        let mut ret = unsafe { Bio::from_raw_mut(d.bio_mut()) }
            .pin_user_pages(&mut unsafe { IovIter::from_raw(&mut d.iter) });

        let dropped_locks = fdm.dropped_locks();
        drop(fdm);

        /*
         * If the fault handler returned an error but also signalled
         * that it dropped & retook ei_pagecache_lock, we just need to
         * re-shoot down the page cache and retry:
         */
        if dropped_locks && ret.is_err() {
            ret = Ok(());
        }

        if let Err(e) = ret {
            break Err(e);
        }

        let ki_pos = unsafe { (*req).ki_pos } as u64;
        if unlikely(dropped_locks) {
            let r = mapping.write_invalidate_range(ki_pos, ki_pos + iter_count as u64 - 1);
            if let Err(e) = r {
                break Err(e);
            }

            if d.op.wbio.bio.bi_iter.bi_size == 0 {
                continue;
            }
        }

        let unaligned = d.op.wbio.bio.bi_iter.bi_size & (fs.block_bytes() as u32 - 1);
        d.op.wbio.bio.bi_iter.bi_size -= unaligned;
        d.iter.revert(unaligned as usize);

        if d.op.wbio.bio.bi_iter.bi_size == 0 {
            /*
             * bio_iov_iter_get_pages was only able to get <
             * blocksize worth of pages:
             */
            break Err(c::EFAULT.into());
        }

        unsafe { c::bch_write_op::init_at(&raw mut d.op, fs, opts) };
        let op = &mut d.op;
        op.end_io      = Some(if sync { dio_write_sync_done } else { dio_write_loop_async });
        op.target      = op.opts.foreground_target;
        op.write_point = c::write_point_specifier::hashed(current_id());
        op.bits.set_nr_replicas(op.opts.data_replicas);
        op.subvol      = inode.inum().subvol as u32;
        op.pos         = pos(inode.inum().inum, ki_pos >> 9);
        op.devs_need_flush = &inode.ei_devs_need_flush;

        if sync {
            op.set(w::sync);
        }
        if d.bits.flush() != 0 {
            d.op.set(w::flush);
        }
        d.op.set(w::check_enospc);

        let sectors = d.bio_sectors();
        if let Err(e) = inode.quota_reservation_add(&mut d.quota_res, sectors, true) {
            break Err(e);
        }

        match d.op.res.add(sectors, d.op.opts.data_replicas.into(), c::ReservationFlags::empty()) {
            Ok(()) => {
                let nr_replicas = d.op.res.nr_replicas.get();
                d.op.bits.set_nr_replicas(nr_replicas as u8);
            }
            Err(e) => {
                if !dio_write_check_allocated(d) {
                    break Err(e);
                }
            }
        }

        task_io_account_write(d.op.wbio.bio.bi_iter.bi_size as usize);

        if unlikely(d.iter.count() != 0) &&
           d.bits.sync() == 0 &&
           d.bits.loop_() == 0 &&
           dio_write_copy_iov(d).is_err() {
            d.bits.set_sync(1);
            sync = true;
        }

        d.bits.set_loop_(1);
        unsafe { closure::call(NonNull::from(&mut d.op), bch2_write, None) };

        if !sync {
            return -(c::EIOCBQUEUED as isize);
        }

        let d = unsafe { &mut *dio };
        assert!(d.bits.sync_done() != 0, "a sync DIO write's op returned before it was done");
        d.bits.set_sync_done(0);

        dio_write_end(d);

        if likely(d.iter.count() == 0) || d.op.error != 0 {
            break Ok(());
        }

        unsafe { c::bio_reset(d.bio_mut(), core::ptr::null_mut(),
                              c::req_op_REQ_OP_WRITE | c::REQ_SYNC | c::REQ_IDLE) };
    };

    if let Err(e) = ret {
        let d = unsafe { &mut *dio };
        d.op.set_error(e);

        unsafe { c::bio_release_pages(d.bio_mut(), false) };

        inode.quota_reservation_put(&mut d.quota_res);
    }

    unsafe { dio_write_done(dio) }
}

/// # Safety
/// As dio_write_loop()'s.
#[cold]
#[inline(never)]
unsafe fn dio_write_continue(dio: *mut Dio) {
    let mm = unsafe { (*dio).mm };

    unsafe { c::bio_reset((*dio).bio_mut(), core::ptr::null_mut(), c::req_op_REQ_OP_WRITE) };

    let _mm = mm.map(|mm| unsafe { mm.use_in_kthread() });
    unsafe { dio_write_loop(dio) };
}

unsafe fn dio_write_loop_async(op: *mut c::bch_write_op) {
    let dio = unsafe { of_op(op) };
    let d = unsafe { &mut *dio };

    dio_write_end(d);

    if likely(d.iter.count() == 0) || d.op.error != 0 {
        unsafe { dio_write_done(dio) };
    } else {
        unsafe { dio_write_continue(dio) };
    }
}

pub fn direct_write(k: &mut Kiocb<'_>, it: &mut IovIter<'_>) -> isize {
    let mapping = k.file().mapping();
    let inode = k.inode();
    let fs = inode.fs();

    let Some(write_ref) = fs.writes.tryget_live() else {
        return -(c::EROFS as isize);
    };

    let mut locked = Some(inode.v.lock());

    let ret = 'out: {
        match k.write_checks(it) {
            Ok(0) => break 'out 0,
            Err(e) => break 'out e.class() as isize,
            Ok(_) => {}
        }

        if let Err(e) = k.file().remove_privs().and_then(|()| k.file().update_time()) {
            break 'out e.class() as isize;
        }

        let (ki_pos, count) = (k.pos(), it.count() as u64);
        if unlikely((ki_pos | count) & (fs.block_bytes() - 1) != 0) {
            break 'out fs.err(Errcode::BCH_ERR_EINVAL_unaligned_io).class() as isize;
        }

        let started = inode.dio_start();

        let extending = ki_pos + count > inode.v.size();
        if !extending {
            locked = None;
        }

        let bio = unsafe {
            c::bio_alloc_bioset(core::ptr::null_mut(),
                                c::bio_iov_vecs_to_alloc(it.as_ptr(), c::BIO_MAX_VECS as i32) as u16,
                                c::req_op_REQ_OP_WRITE | c::REQ_SYNC | c::REQ_IDLE,
                                c::GFP_KERNEL, fs.vfs.dio_write_bioset.as_ptr())
        };
        let dio = unsafe { crate::util::bio::of_bio::<Dio>(bio) };
        let d = unsafe { &mut *dio };
        d.req           = k.as_ptr();
        d.mapping       = mapping;
        d.inode         = inode.raw();
        d.mm            = Mm::current();
        d.iov           = core::ptr::null();
        d.bits.set_loop_(0);
        d.bits.set_extending(extending.into());
        d.bits.set_sync((k.is_sync() || extending).into());
        d.bits.set_flush((k.is_dsync() && fs.opts().journal_flush_disabled.get() == 0).into());
        d.quota_res.sectors = 0;
        d.written       = 0;
        d.iter          = unsafe { core::ptr::read(it.as_ptr()) };

        if unlikely(!mapping.is_empty()) {
            if let Err(e) = mapping.write_invalidate_range(ki_pos, ki_pos + count - 1) {
                unsafe { c::bio_put(bio) };
                drop(started);
                break 'out e.class() as isize;
            }
        }

        // the dio's memory, a bio's front pad: not a dio's yet, nothing to drop
        unsafe {
            core::ptr::write(&mut d.started, Some(started));
            core::ptr::write(&mut d.write_ref, Some(write_ref));
        }

        unsafe { dio_write_loop(dio) }
    };

    drop(locked);
    ret
}
