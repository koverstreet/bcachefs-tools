// SPDX-License-Identifier: GPL-2.0
//
// IO operations on a bcachefs filesystem.
//
// ReadOp is a genuine async Future: the C shim submits the read and
// returns immediately; the bio endio callback wakes the Rust waker
// when IO completes (from the libaio completion thread in userspace).
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
use bcachefs_kernel::data::write::bch2_write;
use bcachefs_kernel::errcode::{self, BchError};
use bcachefs_kernel::fs::{writepoint_hashed, Fs};
use bcachefs_kernel::inode_opts;
use bcachefs_kernel::util::backref::BackRef;
use bcachefs_kernel::util::bio::Bio;
use bcachefs_kernel::util::closure;

/// Maximum single IO size (must match RUST_IO_MAX in rust_shims.h).
pub const MAX_IO_SIZE: usize = 1 << 20;
const PAGE_SIZE: usize = 4096;
const MAX_BVECS: usize = MAX_IO_SIZE / PAGE_SIZE;

unsafe extern "C" {
    fn rust_read_submit(
        c: *mut Fs,
        rbio: *mut c::bch_read_bio,
        bvecs: *mut c::bio_vec,
        nr_bvecs: u32,
        buf: *mut std::ffi::c_void,
        len: usize,
        offset: u64,
        opts: c::bch_inode_opts,
        inum: c::subvol_inum,
        endio: c::bio_end_io_t,
    );
}

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
unsafe extern "C" fn write_endio(op: *mut c::bch_write_op) {
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
        // The op zeroed but for its references to the filesystem, which
        // can't be null, then made by init()
        let mut state = Box::new(std::mem::MaybeUninit::<WriteState>::zeroed());
        let p = state.as_mut_ptr();
        let mut state = unsafe {
            let c = BackRef::new(fs);
            (&raw mut (*p).op.res).write(c::DiskReservation::new(c.get()));
            (&raw mut (*p).op.c).write(c);
            (&raw mut (*p).waker).write(UnsafeCell::new(None));
            state.assume_init()
        };
        let WriteState { op, bvecs, .. } = &mut *state;

        unsafe {
            bcachefs_kernel::c::bio_init(&mut op.wbio.bio, std::ptr::null_mut(), bvecs.as_mut_ptr(), MAX_BVECS as u16, 0);
            Bio::from_raw_mut(&mut op.wbio.bio).map(data.as_ptr().cast_mut(), data.len());
            op.init(fs, inode_opts::get_fs(fs, false));
        }
        op.end_io      = Some(write_endio);
        op.write_point = writepoint_hashed(0);
        op.bits.set_nr_replicas(replicas as u8);
        op.subvol      = subvol;
        op.pos         = spos(inum, offset >> 9, u32::MAX);
        op.new_i_size  = new_i_size;
        op.set(c::bch_write_flags::BCH_WRITE_sync);

        let submit_err = op.res.add((data.len() >> 9) as u64, replicas, c::bch_reservation_flags::empty()).err();
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

// Heap-allocated state for an in-flight read. The bch_read_bio is at
// offset 0 so the endio callback can cast bio → bch_read_bio → ReadState.
#[repr(C)]
struct ReadState {
    rbio:       c::bch_read_bio,
    bvecs:      [c::bio_vec; MAX_BVECS],
    completed:  AtomicBool,
    waker:      UnsafeCell<Option<Waker>>,
}

// Safety: the waker is only written from poll (single-threaded) and
// read from the endio callback. AtomicBool provides the ordering
// guarantee: the endio stores completed=true with Release, and poll
// loads with Acquire, so the waker write is visible before we read
// completed=true.
unsafe impl Send for ReadState {}
unsafe impl Sync for ReadState {}

/// Endio callback — called from the IO completion path (libaio thread).
/// Gets the bio pointer, walks up to ReadState via container_of, signals
/// completion and wakes the Rust future.
unsafe extern "C" fn read_endio(bio: *mut c::bio) {
    // container_of(bio, bch_read_bio, bio) — bio is the last field
    let rbio = unsafe { (bio as *mut u8).sub(std::mem::offset_of!(c::bch_read_bio, bio)) }
        as *mut c::bch_read_bio;
    // ReadState has rbio at offset 0
    let state = rbio as *mut ReadState;

    // Take and wake before setting completed, so the waker is consumed
    // before any future poll sees completed=true.
    let waker = unsafe { (*(*state).waker.get()).take() };
    unsafe { (*state).completed.store(true, Ordering::Release) };
    if let Some(w) = waker {
        w.wake();
    }
}

/// Async read operation on a bcachefs filesystem.
///
/// IO is submitted in new(); poll checks for completion.
pub struct ReadOp {
    state: Pin<Box<ReadState>>,
}

impl ReadOp {
    pub fn new(
        fs: &Fs,
        inum: c::subvol_inum,
        offset: u64,
        inode: &c::bch_inode_unpacked,
        buf: &mut [u8],
    ) -> Self {
        let opts = bcachefs_kernel::inode_opts::get_inode(fs, inode);

        let state = Box::pin(ReadState {
            rbio:       unsafe { std::mem::zeroed() },
            bvecs:      unsafe { std::mem::zeroed() },
            completed:  AtomicBool::new(false),
            waker:      UnsafeCell::new(None),
        });

        unsafe {
            let state_ptr = &*state as *const ReadState as *mut ReadState;
            rust_read_submit(
                fs.as_ptr(),
                &raw mut (*state_ptr).rbio,
                (*state_ptr).bvecs.as_mut_ptr(),
                MAX_BVECS as u32,
                buf.as_mut_ptr() as *mut _,
                buf.len(),
                offset,
                opts,
                inum,
                Some(read_endio),
            );
        }

        Self { state }
    }
}

impl Future for ReadOp {
    type Output = Result<(), BchError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        if this.state.completed.load(Ordering::Acquire) {
            let ret = this.state.rbio.ret as i32;
            Poll::Ready(errcode::ret_to_result(ret).map(|_| ()))
        } else {
            unsafe {
                *this.state.waker.get() = Some(cx.waker().clone());
            }
            // Re-check after storing waker to avoid missed wakeup
            if this.state.completed.load(Ordering::Acquire) {
                let ret = this.state.rbio.ret as i32;
                Poll::Ready(errcode::ret_to_result(ret).map(|_| ()))
            } else {
                Poll::Pending
            }
        }
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
