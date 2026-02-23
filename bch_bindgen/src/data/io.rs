// SPDX-License-Identifier: GPL-2.0
//
// Async IO operations on a bcachefs filesystem.
//
// WriteOp and ReadOp map the kernel's closure-based IO completion model
// to Rust's async/Future model:
//
//   closure_init / write_op_init  →  Future construction (builder pattern)
//   closure_call(bch2_write)      →  first poll (submit IO)
//   closure_sync                  →  poll returning Ready
//
// Initial implementation: synchronous C shims (complete on first poll).
// Target: native Rust where closure completion drives the Waker.

use crate::c;
use crate::errcode::{self, BchError};
use crate::fs::Fs;

/// Maximum single IO size (must match RUST_IO_MAX in rust_shims.h).
pub const MAX_IO_SIZE: usize = 1 << 20;

extern "C" {
    fn rust_write_data(
        c: *mut c::bch_fs,
        inum: u64,
        offset: u64,
        buf: *const std::ffi::c_void,
        len: usize,
        subvol: u32,
        replicas: u32,
        sectors_delta: *mut i64,
    ) -> i32;

    fn rust_read_data(
        c: *mut c::bch_fs,
        inum: u64,
        subvol: u32,
        offset: u64,
        buf: *mut std::ffi::c_void,
        len: usize,
    ) -> i32;
}

/// Result of a write operation.
pub struct WriteResult {
    /// Change in inode sector count (from bch_write_op.i_sectors_delta).
    pub sectors_delta: i64,
}

/// Builder for a bcachefs write operation.
///
/// Maps to the kernel's `bch_write_op` + closure submission.
///
/// ```ignore
/// let result = block_on(
///     fs.write()
///         .pos(inum, offset)
///         .submit(data)
/// )?;
/// inode.bi_sectors += result.sectors_delta;
/// ```
pub struct WriteOp<'a> {
    fs: &'a Fs,
    inum: u64,
    offset: u64,
    subvol: u32,
    replicas: u32,
}

impl<'a> WriteOp<'a> {
    pub fn new(fs: &'a Fs) -> Self {
        Self { fs, inum: 0, offset: 0, subvol: 1, replicas: 1 }
    }

    /// Set the target inode and byte offset.
    pub fn pos(mut self, inum: u64, byte_offset: u64) -> Self {
        self.inum = inum;
        self.offset = byte_offset;
        self
    }

    /// Set the subvolume (default: 1).
    pub fn subvol(mut self, s: u32) -> Self {
        self.subvol = s;
        self
    }

    /// Set the replication factor (default: 1).
    pub fn replicas(mut self, n: u32) -> Self {
        self.replicas = n;
        self
    }

    /// Submit the write. Data must be block-aligned and <= MAX_IO_SIZE.
    ///
    /// Currently completes synchronously (C shim). When the closure
    /// subsystem is ported to Rust, this becomes a genuine async
    /// operation where IO completion wakes the Future.
    pub async fn submit(self, data: &[u8]) -> Result<WriteResult, BchError> {
        let mut sectors_delta: i64 = 0;
        let ret = unsafe {
            rust_write_data(
                self.fs.raw,
                self.inum,
                self.offset,
                data.as_ptr() as *const _,
                data.len(),
                self.subvol,
                self.replicas,
                &mut sectors_delta,
            )
        };
        errcode::ret_to_result(ret)?;
        Ok(WriteResult { sectors_delta })
    }
}

/// Builder for a bcachefs read operation.
///
/// ```ignore
/// block_on(
///     fs.read()
///         .pos(inum, offset)
///         .submit(buf)
/// )?;
/// ```
pub struct ReadOp<'a> {
    fs: &'a Fs,
    inum: u64,
    offset: u64,
    subvol: u32,
}

impl<'a> ReadOp<'a> {
    pub fn new(fs: &'a Fs) -> Self {
        Self { fs, inum: 0, offset: 0, subvol: 1 }
    }

    /// Set the source inode and byte offset.
    pub fn pos(mut self, inum: u64, byte_offset: u64) -> Self {
        self.inum = inum;
        self.offset = byte_offset;
        self
    }

    /// Set the subvolume (default: 1).
    pub fn subvol(mut self, s: u32) -> Self {
        self.subvol = s;
        self
    }

    /// Submit the read. Buffer must be block-aligned and <= MAX_IO_SIZE.
    ///
    /// Currently completes synchronously (C shim). When the closure
    /// subsystem is ported to Rust, this becomes a genuine async
    /// operation where IO completion wakes the Future.
    pub async fn submit(self, buf: &mut [u8]) -> Result<(), BchError> {
        let ret = unsafe {
            rust_read_data(
                self.fs.raw,
                self.inum,
                self.subvol,
                self.offset,
                buf.as_mut_ptr() as *mut _,
                buf.len(),
            )
        };
        errcode::ret_to_result(ret)?;
        Ok(())
    }
}

/// Simple single-poll executor for futures that complete immediately.
///
/// Used during the transition from C closure-based IO to native Rust async.
/// All current IO operations complete on first poll (synchronous C shims),
/// so this just polls once and returns the result.
///
/// When the closure subsystem moves to Rust async, this will be replaced
/// by a real executor (or callers will be in async contexts already).
pub fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    let mut fut = std::pin::pin!(fut);
    let waker = noop_waker();
    let mut cx = std::task::Context::from_waker(&waker);
    match fut.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(val) => val,
        std::task::Poll::Pending =>
            panic!("block_on: future returned Pending — IO shim should be synchronous"),
    }
}

fn noop_waker() -> std::task::Waker {
    use std::task::{RawWaker, RawWakerVTable};

    const VTABLE: RawWakerVTable = RawWakerVTable::new(
        |p| RawWaker::new(p, &VTABLE),
        |_| {},
        |_| {},
        |_| {},
    );

    unsafe { std::task::Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
}
