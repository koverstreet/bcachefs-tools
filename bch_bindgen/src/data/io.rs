// SPDX-License-Identifier: GPL-2.0
//
// IO operations on a bcachefs filesystem.
//
// ReadOp runs the read path's read() on a task and waits for it: ready on
// its first poll.
//
// WriteOp is still synchronous pending the same treatment.

use std::cell::UnsafeCell;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Waker};

use std::ptr::NonNull;

use crate::c;
use bcachefs_kernel::btree::bkey::spos;
use bcachefs_kernel::data::read::{self, Rbio};
use bcachefs_kernel::data::write::bch2_write;
use bcachefs_kernel::errcode::BchError;
use bcachefs_kernel::fs::Fs;
use bcachefs_kernel::inode_opts;
use bcachefs_kernel::util::bio::{Bio, BioOwner};
use bcachefs_kernel::util::closure;
use bcachefs_kernel::util::sleep::Sleep;
use bcachefs_kernel::util::task::{waiter, CompletionReturn, Spawn, TaskSlot};
use bcachefs_kernel::os::workqueue::Queue;

/// Maximum single IO size.
pub const MAX_IO_SIZE: usize = 1 << 20;
const PAGE_SIZE: usize = 4096;
const MAX_BVECS: usize = MAX_IO_SIZE / PAGE_SIZE;

/// Result of a write operation.
pub struct WriteResult {
    pub sectors_delta: i64,
}

// Heap-allocated state for an in-flight write.
// bch_write_op is at offset 0 so end_io can cast directly to WriteState.
#[repr(C)]
struct WriteState {
    op:         c::bch_write_op,
    bvecs:      [c::bio_vec; MAX_BVECS],
    completed:  AtomicBool,
    waker:      UnsafeCell<Option<Waker>>,
}

unsafe impl Send for WriteState {}
unsafe impl Sync for WriteState {}

/// end_io callback for bch_write_op — signals completion and wakes
/// the Rust future.
unsafe fn write_endio(op: *mut c::bch_write_op) {
    // WriteState has op at offset 0
    let state = op as *mut WriteState;

    let waker = unsafe { (*(*state).waker.get()).take() };
    unsafe { (*state).completed.store(true, Ordering::Release) };
    if let Some(w) = waker {
        w.wake();
    }
}

/// Async write operation on a bcachefs filesystem.
///
/// IO is submitted in new(); poll checks for completion.
pub struct WriteOp {
    state: Pin<Box<WriteState>>,
    /// Set if the disk reservation failed, and nothing was submitted.
    submit_err: Option<BchError>,
}

impl WriteOp {
    pub fn new(
        fs: &Fs,
        inum: u64,
        offset: u64,
        subvol: u32,
        replicas: u32,
        data: &[u8],
        new_i_size: u64,
    ) -> Self {
        // Made in place: the op's bio first, which the op's init() leaves be
        let mut state = Box::new(std::mem::MaybeUninit::<WriteState>::zeroed());
        let p = state.as_mut_ptr();
        let mut state = unsafe {
            let bio = &raw mut (*p).op.wbio.bio;
            bcachefs_kernel::c::bio_init(bio, std::ptr::null_mut(), (&raw mut (*p).bvecs).cast(),
                                         MAX_BVECS as u16, 0);
            Bio::from_raw_mut(bio).map(data.as_ptr().cast_mut(), data.len());
            c::bch_write_op::init_at(&raw mut (*p).op, fs, inode_opts::get_fs(fs, false));
            (&raw mut (*p).waker).write(UnsafeCell::new(None));
            state.assume_init()
        };
        let WriteState { op, .. } = &mut *state;
        op.end_io      = Some(write_endio);
        op.write_point = c::write_point_specifier::hashed(0);
        op.bits.set_nr_replicas(replicas as u8);
        op.subvol      = subvol;
        op.pos         = spos(inum, offset >> 9, u32::MAX);
        op.new_i_size  = new_i_size;
        op.set(c::WriteFlags::sync);

        let submit_err = op.res.add((data.len() >> 9) as u64, replicas, c::ReservationFlags::empty()).err();
        if submit_err.is_none() {
            // BCH_WRITE_sync: done when this returns, so @data outlives it
            unsafe { closure::call(NonNull::from(&mut *op), bch2_write, None) };
        }

        Self { state: Box::into_pin(state), submit_err }
    }
}

impl Future for WriteOp {
    type Output = Result<WriteResult, BchError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        // Submit-time error (e.g. disk reservation failure)
        if let Some(e) = this.submit_err {
            return Poll::Ready(Err(e));
        }

        if this.state.completed.load(Ordering::Acquire) {
            let error = this.state.op.error as i32;
            let sectors_delta = this.state.op.i_sectors_delta;
            if error != 0 {
                Poll::Ready(Err(BchError::from_raw(-error)))
            } else {
                Poll::Ready(Ok(WriteResult { sectors_delta }))
            }
        } else {
            unsafe {
                *this.state.waker.get() = Some(cx.waker().clone());
            }
            if this.state.completed.load(Ordering::Acquire) {
                let error = this.state.op.error as i32;
                let sectors_delta = this.state.op.i_sectors_delta;
                if error != 0 {
                    Poll::Ready(Err(BchError::from_raw(-error)))
                } else {
                    Poll::Ready(Ok(WriteResult { sectors_delta }))
                }
            } else {
                Poll::Pending
            }
        }
    }
}

/// A read on a bcachefs filesystem: read by a task, which new() waits for.
pub struct ReadOp {
    ret: Result<(), BchError>,
}

impl ReadOp {
    pub fn new(
        fs: &Fs,
        inum: c::subvol_inum,
        offset: u64,
        inode: &c::bch_inode_unpacked,
        buf: &mut [u8],
    ) -> Self {
        type F = c::ReadFlags;
        let opts = bcachefs_kernel::inode_opts::get_inode(fs, inode);

        let mut b = fs.bio_read.alloc(std::ptr::null_mut(), 1, c::req_op_REQ_OP_READ,
                                      |b| Rbio::new(fs, opts, inum.subvol as u32, None, b))
            .expect("a bio_read bio of one bvec: from its mempool");
        b.bio_mut().set_opf(c::req_op_REQ_OP_READ | c::REQ_SYNC);
        b.bio_mut().set_sector(offset >> 9);
        unsafe { b.bio_mut().map(buf.as_mut_ptr(), buf.len()) };

        let flags = F::retry_if_stale | F::may_promote | F::user_mapped;
        let slot = match TaskSlot::alloc(()) {
            Ok(slot) => slot,
            Err(e) => return Self { ret: Err(e) },
        };
        let mut done = None;
        let (wait, r) = CompletionReturn::new(&mut done, waiter::Sync::current());
        let task = slot.init(Spawn::Queue(Queue::system_dfl()), async move |_: &()| {
            r.complete(read::read(&b, inum, flags, ()).await);
        }, None).expect("a queue's task: nothing in it fails");
        unsafe { task.start_unchecked() };
        Self { ret: wait.wait(Sleep::UNINTERRUPTIBLE).expect("a read's task, ended with what it read") }
    }
}

impl Future for ReadOp {
    type Output = Result<(), BchError>;

    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        Poll::Ready(self.ret)
    }
}

/// Simple executor for futures — polls in a loop until Ready.
///
/// Handles both synchronous completion (ready on first poll) and
/// async completion (endio wakes from another thread).
pub fn block_on<F: Future>(fut: F) -> F::Output {
    let mut fut = std::pin::pin!(fut);
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(val) => return val,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

fn noop_waker() -> Waker {
    use std::task::{RawWaker, RawWakerVTable};

    const VTABLE: RawWakerVTable = RawWakerVTable::new(
        |p| RawWaker::new(p, &VTABLE),
        |_| {},
        |_| {},
        |_| {},
    );

    unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
}
