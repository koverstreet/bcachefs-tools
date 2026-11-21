// SPDX-License-Identifier: GPL-2.0

//! O_DIRECT writes: direct.c's write half. A dio write is a task, its home a
//! Dio - a write op over the user's pages - in one allocation with the
//! task and the op's bvecs, from a pool of the module's: freed when its task
//! is done, after the dio's ended, when the filesystem may be gone. It writes
//! a bio's worth of pages at a time, in order, awaiting each write: the
//! first in its caller's context, started inline, the rest in a worker, in
//! the caller's address space. Then it's ended, as a read dio is: the
//! kiocb's completion, or the caller waiting for it.

use crate::btree::bkey::{pos, spos};
use crate::btree::iter::{lockrestart_do, BtreeIter, BtreeIterFlags};
use crate::btree_trans;
use crate::c;
use crate::data::durability::BkeyVDurability;
use crate::data::write::{write, WriteFor};
use crate::errcode::{Errcode, BchError};
use crate::format::Btree;
use crate::fs::Fs;
use crate::inode_opts;
use crate::snapshots::subvolume;
use crate::util::alloc::Kmalloc;
use crate::os::bio::{Bio, BvecMempool};
use crate::util::mempool::Mempool;
use crate::util::sleep::Sleep;
use crate::util::task::{exclusive_size, waiter, CompletionReturn, DoneWith, Resume, Spawn, TaskSlot, WaitSync};
use crate::os::iov_iter::{HeldIovIter, IovIter};
use crate::os::kernel::{current_id, likely, task_io_account_write, unlikely, Mm};
use crate::os::module::{Quiesced, ThisModule};
use crate::os::ops::IntoRaw;
use crate::os::workqueue::Queue;
use crate::util::refcount::{PercpuRef, RefGuard, TryRefcount};
use super::direct::End;
use crate::vfs::folio::Mapping;
use crate::vfs::inode::DioStarted;
use crate::os::kiocb::Kiocb;
use super::file::InodeFile;
use c::WriteFlags as w;
use core::ops::ControlFlow;
use core::pin::Pin;
use core::ptr;
use cstruct_macros::in_place;
use pin_init::{pin_data, pin_init};

#[pin_data]
struct Dio {
    /* where its next bio's written: the kiocb's, moved on as it's done */
    pos:       u64,
    mapping:   Mapping,
    mm:        Option<Mm>,
    extending: bool,
    sync:      bool,
    flush:     bool,
    quota_res: c::quota_res,
    written:   u64,

    iter:      HeldIovIter,

    /* the dio count and the page cache blocked, and c->writes: till it's
     * done */
    started:   Option<DioStarted>,
    write_ref: Option<RefGuard<PercpuRef>>,

    #[pin]
    op:        c::bch_write_op,
    #[pin]
    wbio:      Bio,
}
crate::has_bio!(Dio, wbio);

type Pool = &'static Mempool<Kmalloc>;

static DIO_WRITES: Mempool<Kmalloc> = ::pin_init::zeroed();
static DIO_WRITE_BVECS: BvecMempool = ::pin_init::zeroed();

pub fn pools_init(m: &ThisModule) -> Result<(), BchError> {
    Mempool::init_static(&DIO_WRITES, Mempool::kmalloc(4, exclusive_size::<Dio, DioWriteFut<'static>, Pool>()), m)?;
    BvecMempool::init_static(&DIO_WRITE_BVECS, 4, m)
}

pub fn pools_exit(q: &Quiesced<'_>) {
    DIO_WRITE_BVECS.exit(q);
    DIO_WRITES.exit(q);
}

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

impl Dio {
    fn bio_sectors(&self) -> u64 {
        u64::from(self.wbio.range().len() >> 9)
    }

    /// Its pages given back: a write only read them.
    fn release_pages(&mut self) {
        self.wbio.release_pinned_pages();
    }
}

#[inline(never)]
fn dio_write_check_allocated(dio: &Dio) -> bool {
    check_range_allocated(dio.op.fs(), dio.mapping.inode().inum(), dio.op.pos.offset, dio.bio_sectors(),
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

    dio.iter.keep_iovecs().map_err(|_| ())
}

/// What the dio wrote, or its error, what it held given back.
fn dio_write_done(d: &mut Dio) -> Result<usize, BchError> {
    let ret = match d.op.error {
        0 => Ok((d.written << 9) as usize),
        e => Err(BchError::from_raw(e.into())),
    };
    drop(d.write_ref.take());

    /* inode->i_dio_count is our ref on inode and thus bch_fs */
    drop(d.started.take());
    ret
}

#[inline(always)]
fn dio_write_end(dio: &mut Dio) {
    let mapping = dio.mapping;
    let inode = mapping.inode();

    dio.pos += u64::from(dio.op.written) << 9;
    dio.written += u64::from(dio.op.written);

    if dio.extending {
        inode.v.size_extend(dio.pos);
    }

    if dio.op.i_sectors_delta != 0 || dio.quota_res.sectors != 0 {
        inode.i_sectors_acct_put(&mut dio.quota_res, dio.op.i_sectors_delta);
    }

    dio.release_pages();

    if unlikely(dio.op.error != 0) {
        inode.ei_flags.set(c::InodeInfoFlag::EI_INODE_ERROR, true);
    }
}

/// The dio's next bio's worth of pages pinned, whole blocks of them: in its
/// caller's context - or, @in_caller false, a worker, in its caller's address
/// space.
fn dio_write_pin(d: &mut Dio, in_caller: bool) -> Result<(), BchError> {
    let fs = d.op.fs();

    loop {
        let iter_count = d.iter.get().count();

        let (mut ret, dropped_locks) = {
            let _mm = (!in_caller).then_some(d.mm).flatten().map(|mm| unsafe { mm.use_in_kthread() });

            debug_assert!(fs.fdm_table.get().is_none(), "a DIO write with page faults disabled already");
            let fdm = fs.fdm_table.set(d.mapping);

            let ret = d.wbio.pin_user_pages(&mut d.iter.get());
            (ret, fdm.dropped_locks())
        };

        /*
         * If the fault handler returned an error but also signalled
         * that it dropped & retook ei_pagecache_lock, we just need to
         * re-shoot down the page cache and retry:
         */
        if dropped_locks && ret.is_err() {
            ret = Ok(());
        }
        ret?;

        if unlikely(dropped_locks) {
            d.mapping.write_invalidate_range(d.pos, d.pos + iter_count as u64 - 1)?;

            if d.wbio.range().is_empty() {
                continue;
            }
        }

        let len = d.wbio.range().len();
        let unaligned = len & (fs.block_bytes() as u32 - 1);
        d.wbio.shrink(len - unaligned);
        d.iter.get().revert(unaligned as usize);

        if d.wbio.range().is_empty() {
            /*
             * bio_iov_iter_get_pages was only able to get <
             * blocksize worth of pages:
             */
            return Err(c::EFAULT.into());
        }
        return Ok(());
    }
}

/// The dio's writes, a bio's worth of its pages at a time, the first's
/// pinned by its caller.
async fn dio_write_loop(d: &mut Dio) -> Result<(), BchError> {
    let mapping = d.mapping;
    let inode = mapping.inode();
    let fs = inode.fs();
    let opts = inode_opts::get_inode(fs, &inode.inode());
    // its caller's, not a worker's: every bio of the dio to the same one
    let write_point = c::write_point_specifier::hashed(current_id());

    loop {
        unsafe { c::bch_write_op::init_at(&raw mut d.op, fs, opts) };
        let op = &mut d.op;
        op.target      = op.opts.foreground_target;
        op.write_point = write_point;
        op.bits.set_nr_replicas(op.opts.data_replicas);
        op.subvol      = inode.inum().subvol as u32;
        op.pos         = pos(inode.inum().inum, d.pos >> 9);

        if d.flush {
            d.op.set(w::flush);
        }
        d.op.set(w::check_enospc);

        let sectors = d.bio_sectors();
        inode.quota_reservation_add(&mut d.quota_res, sectors, true)?;

        match d.op.res.add(sectors, d.op.opts.data_replicas.into(), c::ReservationFlags::empty()) {
            Ok(()) => {
                let nr_replicas = d.op.res.nr_replicas.get();
                d.op.bits.set_nr_replicas(nr_replicas as u8);
            }
            Err(e) => {
                if !dio_write_check_allocated(d) {
                    return Err(e);
                }
            }
        }

        task_io_account_write(d.wbio.range().len() as usize);

        write(&mut d.op, &mut d.wbio, WriteFor::Inode(&inode.ei_devs_need_flush), None, Resume::queue(Queue::system_dfl()))
            .await;

        dio_write_end(d);

        if likely(d.iter.get().count() == 0) || d.op.error != 0 {
            return Ok(());
        }

        let opf = match d.sync {
            true  => c::req_op_REQ_OP_WRITE | c::REQ_SYNC | c::REQ_IDLE,
            false => c::req_op_REQ_OP_WRITE,
        };
        d.wbio.clear(opf);
        dio_write_pin(d, false)?;
    }
}

#[in_place(DioWriteFut)]
async fn dio_write_task<'h>(dio: Pin<&'h mut Dio>, mut end: End) {
    let d = dio.get_mut();

    if let Err(e) = dio_write_loop(d).await {
        d.op.set_error(e);
        d.release_pages();
        let mapping = d.mapping;
        mapping.inode().quota_reservation_put(&mut d.quota_res);
    }

    // an async kiocb's position moved on before it's completed; a sync
    // one's caller moves its own
    if let End::Async(done) = &mut end {
        done.advance(d.written << 9);
    }
    end.complete(dio_write_done(d));
}

pub fn direct_write(k: &mut Kiocb<'_, InodeFile>, it: &mut IovIter<'_>) -> isize {
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
            break 'out fs.err(Errcode::EINVAL_unaligned_io).class() as isize;
        }

        let started = inode.dio_start();

        let extending = ki_pos + count > inode.v.size();
        if !extending {
            locked = None;
        }

        if unlikely(!mapping.is_empty()) {
            if let Err(e) = mapping.write_invalidate_range(ki_pos, ki_pos + count - 1) {
                break 'out e.class() as isize;
            }
        }

        let mut sync = k.is_sync() || extending;
        let flush = k.is_dsync() && fs.opts().journal_flush_disabled.get() == 0;
        let opts = inode_opts::get_inode(fs, &inode.inode());
        let nr_vecs = it.bio_vecs();

        let mut dio = match TaskSlot::exclusive_bio_in(&DIO_WRITES, &DIO_WRITE_BVECS, nr_vecs,
                                                       c::req_op_REQ_OP_WRITE | c::REQ_SYNC | c::REQ_IDLE, |b| {
            pin_init!(Dio {
                pos:       ki_pos,
                mapping,
                mm:        Mm::current(),
                extending,
                sync:      false,
                flush,
                quota_res: c::quota_res { sectors: 0 },
                written:   0,
                iter:      unsafe { HeldIovIter::new(it) },
                started:   Some(started),
                write_ref: Some(write_ref),
                op         <- c::bch_write_op::init(fs, opts),
                wbio       <- b.init::<BchError, _>(),
            }? BchError)
        }) {
            Ok(dio) => dio,
            Err(e) => break 'out e.class() as isize,
        };

        // the first bio's pages pinned here, in the caller: if there's more,
        // an async kiocb's iovec - which may be on its caller's stack - is
        // copied, or the dio is waited for here
        let d = dio.home().get_mut();
        if let Err(e) = dio_write_pin(d, true) {
            d.release_pages();
            break 'out e.class() as isize;
        }
        if unlikely(d.iter.get().count() != 0) && !sync && dio_write_copy_iov(d).is_err() {
            sync = true;
        }
        d.sync = sync;

        let mut slot = DoneWith::new();
        let (end, wait) = match sync {
            false => (End::Async(unsafe { k.queued() }.expect("an async kiocb")), None),
            true  => {
                let (wait, r) = CompletionReturn::new(&mut slot, waiter::Sync::current());
                (End::Sync(r), Some(wait))
            }
        };

        unsafe { dio.start(Spawn::Inline, fs.tasks(), move |d| dio_write_task(d, end)) };

        let Some(wait) = wait else {
            break 'out -(c::EIOCBQUEUED as isize);
        };
        let ret = wait.wait_sync(Sleep::UNINTERRUPTIBLE).expect("a dio write's task, ended with what it wrote");
        if let Ok(written) = ret {
            k.advance(written as u64);
        }
        ret.into_raw()
    };

    drop(locked);
    ret
}
