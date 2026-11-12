// SPDX-License-Identifier: GPL-2.0

//! O_DIRECT reads: into the user's buffer, a bio of its pages at a time, each
//! through the read path. Each bio is the home of a task, in one allocation
//! with the task and its bvecs, from a pool of the module's: a box is freed
//! when its task is done, after the dio's ended - when the filesystem may be
//! gone. The first bio's task is the dio's: it sets up the bios in order, and
//! reads the first: started, then the rest started - each a split, that puts
//! what it read in the dio's Gather - then the splits awaited, if there are
//! any, then the first; and the dio's ended: the kiocb's completion, or the
//! caller waiting for it.

use super::kiocb::{self, Kiocb};
use crate::c;
use crate::data::read;
use crate::errcode::{bch_errcode, BchError};
use crate::fs::Fs;
use crate::inode_opts::get_inode;
use crate::util::alloc::Kmalloc;
use crate::util::bio::{inline_vecs, plugged, Bio, HasBio};
use crate::util::gather::{Gather, GatherRef};
use crate::util::iov_iter::IovIter;
use crate::util::mempool::Mempool;
use crate::util::ops::IntoRaw;
use crate::util::sleep::Sleep;
use crate::util::task::{exclusive_size, poll_once, waiter, CompletionReturn, Resume, Return, TaskSlot};
use crate::util::workqueue::Queue;
use core::pin::{pin, Pin};
use core::ptr::{self, NonNull};
use core::task::Poll;
use cstruct_macros::in_place;
use pin_init::{pin_data, pin_init, pinned_drop, stack_pin_init};

type F = c::bch_read_flags;
type Splits = Gather<Result<(), BchError>, waiter::Async>;
type SplitRef = GatherRef<Result<(), BchError>, waiter::Async>;
type Pool = &'static Mempool<Kmalloc>;

static DIOS: Mempool<Kmalloc> = ::pin_init::zeroed();
static SPLITS: Mempool<Kmalloc> = ::pin_init::zeroed();

/// At module init: elements for the most bvecs a bio has.
pub fn pools_init() -> Result<(), BchError> {
    let vecs = inline_vecs(c::BIO_MAX_VECS as u16);
    unsafe {
        Mempool::init_static(&DIOS, Mempool::kmalloc(4, exclusive_size::<DioBio, DioFut<'static>, Pool>(vecs)))?;
        Mempool::init_static(&SPLITS, Mempool::kmalloc(1, exclusive_size::<DioBio, SplitFut<'static>, Pool>(vecs)))
    }
}

/// # Safety
/// At module exit, after the last box's grace period.
pub unsafe fn pools_exit() {
    unsafe {
        SPLITS.exit();
        DIOS.exit();
    }
}

enum End {
    Async(kiocb::Completion),
    Sync(Return<Result<usize, BchError>, waiter::Sync>),
}

impl End {
    fn complete(self, ret: Result<usize, BchError>) {
        match self {
            End::Async(done) => done.complete(ret.into_raw()),
            End::Sync(r) => r.complete(ret),
        }
    }
}

#[pin_data(PinnedDrop)]
#[repr(C)]
struct DioBio {
    #[pin]
    rbio: c::bch_read_bio,
}
crate::has_bio!(DioBio, rbio.bio);

#[pinned_drop]
impl PinnedDrop for DioBio {
    fn drop(self: Pin<&mut Self>) {
        unsafe { self.bio_mut().uninit() }
    }
}

struct CallersIter(NonNull<IovIter<'static>>);

unsafe impl Send for CallersIter {}

impl CallersIter {
    unsafe fn new(iter: &mut IovIter<'_>) -> Self {
        CallersIter(NonNull::from(iter).cast())
    }

    fn get(&mut self) -> &mut IovIter<'_> {
        unsafe { self.0.cast().as_mut() }
    }
}

fn set_up(mut b: Pin<&mut Bio>, iter: &mut IovIter<'_>, offset: &mut u64, should_dirty: bool) -> Result<(), BchError> {
    b.as_mut().set_opf(c::req_op_REQ_OP_READ | c::REQ_SYNC);
    b.as_mut().set_sector(*offset >> 9);
    b.as_mut().pin_user_pages(iter)?;

    *offset += u64::from(b.raw().bi_iter.bi_size);

    if should_dirty {
        b.set_pages_dirty();
    }
    Ok(())
}

#[in_place(SplitFut)]
async fn split_task<'h>(mut split: Pin<&'h mut DioBio>, r: SplitRef, inum: c::subvol_inum, flags: F,
                        should_dirty: bool) {
    r.put(read::read(&mut split.rbio, inum, flags).await);
    split.as_mut().bio_mut().release_user_pages(should_dirty).await;
}

#[in_place(DioFut)]
async fn dio_task<'h>(mut dio: Pin<&'h mut DioBio>, end: End, mut iter: CallersIter, mut offset: u64,
                      inum: c::subvol_inum, flags: F, count: usize) {
    let should_dirty = iter.get().user_backed();

    let ret = match set_up(dio.as_mut().bio_mut(), iter.get(), &mut offset, should_dirty) {
        Err(e) => Err(e),
        Ok(()) => {
            let (fs, opts, subvol) = (unsafe { Fs::borrow_raw(dio.rbio.c) }, dio.rbio.opts, dio.rbio.subvol);
            let err_report = unsafe { dio.rbio.err_report.as_ref() };
            stack_pin_init!(let splits = Splits::new());

            let mut own = pin!(read::read(&mut dio.rbio, inum, flags));
            let started = poll_once(own.as_mut()).await;

            let failed = loop {
                if iter.get().count() == 0 {
                    break None;
                }
                let nr_vecs = iter.get().bio_vecs();
                let mut split = TaskSlot::exclusive_bio_in(&SPLITS, ptr::null_mut(), nr_vecs, c::req_op_REQ_OP_READ, |b| {
                    pin_init!(DioBio { rbio <- c::bch_read_bio::new(fs, opts, subvol, err_report, b) }? BchError)
                }).expect("a split's box: its pool waits for an element, and an Rbio's init can't fail");

                if let Err(e) = set_up(split.home().bio_mut(), iter.get(), &mut offset, should_dirty) {
                    break Some((split, e));
                }
                let r = splits.get();
                unsafe {
                    split.start_inline(Queue::system_dfl(), move |s| split_task(s, r, inum, flags, should_dirty))
                };
            };

            let ret = match failed {
                None => Ok(()),
                Some((mut split, e)) => {
                    split.home().bio_mut().release_user_pages(should_dirty).await;
                    Err(e)
                }
            };
            let splits = splits.wait(Resume::Inline).await;
            let own = match started {
                Poll::Ready(ret) => ret,
                Poll::Pending => own.await,
            };
            own.and(splits).and(ret)
        }
    };

    /*
     * no_poison_check asks for what is physically there. The read still
     * failed, and said so - err_report carries which errors - but the bytes
     * it got are in the caller's buffer, and replacing the count with the
     * error is what throws them away.
     */
    end.complete(match ret {
        Err(e) if !flags.contains(F::no_poison_check) => Err(e),
        _ => Ok(count),
    });
    dio.as_mut().bio_mut().release_user_pages(should_dirty).await;
}

fn read(req: &mut Kiocb<'_>, iter: &mut IovIter<'_>, flags: F, err_report: Option<&c::bch_read_err_report>)
    -> Result<usize, BchError>
{
    let inode = req.inode();
    let fs = inode.fs();
    let offset = req.pos();
    let opts = get_inode(fs, &inode.inode());

    /* bios must be 512 byte aligned: */
    if (offset | iter.count() as u64) & (c::SECTOR_SIZE as u64 - 1) != 0 {
        return Err(fs.err(bch_errcode::BCH_ERR_EINVAL_unaligned_io));
    }

    let count = iter.count() as u64;
    let ret = count.min(inode.vfs().size().saturating_sub(offset));
    if ret == 0 {
        return Ok(0);
    }

    let rounded = ret.next_multiple_of(u64::from(fs.opts().block_size.get()));
    let mut iter = iter.hold_back(if rounded < count { (count - rounded) as usize } else { 0 });

    let inum = inode.inum();
    let subvol = inum.subvol as u32;

    let dio = TaskSlot::exclusive_bio_in(&DIOS, ptr::null_mut(), iter.bio_vecs(), c::req_op_REQ_OP_READ, |b| {
        pin_init!(DioBio { rbio <- c::bch_read_bio::new(fs, opts, subvol, err_report, b) }? BchError)
    })?;
    let flags = F::retry_if_stale | F::may_promote | F::user_mapped | flags;

    let mut slot = None;
    let (end, wait) = match unsafe { req.queued() } {
        Some(done) => (End::Async(done), None),
        None => {
            let (wait, r) = CompletionReturn::new(&mut slot, waiter::Sync::current());
            (End::Sync(r), Some(wait))
        }
    };

    unsafe {
        let callers = CallersIter::new(&mut iter);
        dio.start_inline(Queue::system_dfl(), move |d| dio_task(d, end, callers, offset, inum, flags, ret as usize))
    };
    drop(iter);

    match wait {
        None => Err(c::EIOCBQUEUED.into()),
        Some(wait) => wait.wait(Sleep::UNINTERRUPTIBLE).expect("a dio's task, ended with what it read"),
    }
}

pub fn direct_io_read(req: &mut Kiocb<'_>, iter: &mut IovIter<'_>, flags: F,
                      err_report: Option<&c::bch_read_err_report>) -> Result<usize, BchError> {
    let file = req.file();
    if file.has_pagecache() {
        let pos = req.pos();
        if let Some(last) = (pos + iter.count() as u64).checked_sub(1) {
            file.write_and_wait_range(pos, last)?;
        }
    }

    file.accessed();

    let ret = plugged(|| read(req, iter, flags, err_report))?;
    req.advance(ret as u64);
    Ok(ret)
}

pub unsafe extern "C" fn bch2_read_iter(iocb: *mut c::kiocb, iter: *mut c::iov_iter) -> isize {
    let (mut req, mut iter) = unsafe { (Kiocb::from_raw(iocb), IovIter::from_raw(iter)) };
    if iter.count() == 0 {
        return 0; /* skip atime */
    }

    if req.direct() {
        direct_io_read(&mut req, &mut iter, F::empty(), None).into_raw()
    } else {
        let _add = req.inode().ei_pagecache_lock.lock::<c::PagecacheAdd>();
        req.filemap_read(&mut iter)
    }
}
