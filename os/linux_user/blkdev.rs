// SPDX-License-Identifier: GPL-2.0

//! The block layer, for userspace: a block device is a file descriptor - a
//! block device's, or a file's - and its bios are read, written, flushed and
//! discarded by io_uring, or by syscall where there's none.

use crate::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::os::kobject::Kobject;
use crate::util::bio::Bio;
use crate::util::futex::EventCount;
use crate::util::sleep::Sleep;
use crate::util::Printbuf;
use core::ffi::{c_void, CStr};
use core::ptr::{self, NonNull};
use io_uring::{opcode, squeue, types, IoUring};
use rustix::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use rustix::fs::{FileType, Mode, OFlags};
use std::ffi::CString;
use std::sync::{Mutex, OnceLock};

fn io_err(e: rustix::io::Errno) -> BchError {
    std::io::Error::from(e).into()
}

/// A block device: a file descriptor - a block device's, or a file's - and
/// its sysfs directory, which userspace's sysfs never has.
pub struct BlockDevice {
    name: CString,
    dev:  c::dev_t,
    fd:   OwnedFd,
    kobj: Kobject,
}

impl BlockDevice {
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    fn fstat(&self) -> rustix::fs::Stat {
        rustix::fs::fstat(self.fd()).expect("fstat() of an open block device")
    }
}

/// What open_by_path() opens: the device itself, here.
pub type BdevFile = BlockDevice;

fn is_blk(st: &rustix::fs::Stat) -> bool {
    FileType::from_raw_mode(st.st_mode) == FileType::BlockDevice
}

// ── Devices ──────────────────────────────────────────────────────────────

/// @path opened, as @mode says: bdev_file_open_by_path(). The exclusive claim,
/// with BLK_OPEN_EXCL, is O_EXCL's; userspace has no @holder or @ops to call.
pub fn open_by_path(path: &CStr, mode: c::blk_mode_t, _holder: *const c_void,
                    _ops: *const c::blk_holder_ops) -> Result<NonNull<BdevFile>, BchError> {
    let (read, write) = (mode & c::BLK_OPEN_READ != 0, mode & c::BLK_OPEN_WRITE != 0);
    let mut flags = match (read, write) {
        (true, true)  => OFlags::RDWR,
        (false, true) => OFlags::WRONLY,
        _             => OFlags::RDONLY,
    };
    if mode & c::BLK_OPEN_BUFFERED == 0 {
        flags |= OFlags::DIRECT;
    }
    if mode & c::BLK_OPEN_EXCL != 0 {
        flags |= OFlags::EXCL;
    }
    if mode & c::BLK_OPEN_CREAT != 0 {
        flags |= OFlags::CREATE;
    }

    let fd = rustix::fs::open(path, flags, Mode::from_raw_mode(0o600)).map_err(io_err)?;
    let st = rustix::fs::fstat(&fd).map_err(io_err)?;

    /* strip directory */
    let name = path.to_bytes();
    let name = name.rsplit(|&b| b == b'/').next().unwrap_or(name);

    Ok(NonNull::from(Box::leak(Box::new(BlockDevice {
        name: CString::new(name).expect("a path's last component, a CStr's"),
        dev:  st.st_rdev as c::dev_t,
        fd,
        kobj: ::pin_init::zeroed(),
    }))))
}

/// The device @file has open.
///
/// # Safety
/// @file is open_by_path()'s, and not yet closed.
pub unsafe fn file_bdev(file: NonNull<BdevFile>) -> *mut BlockDevice {
    file.as_ptr()
}

/// bdev_fput(): @file closed, what it wrote synced first.
///
/// # Safety
/// @file is open_by_path()'s, and nothing uses it, or its device, after.
pub unsafe fn close(file: NonNull<BdevFile>) {
    let bdev = unsafe { Box::from_raw(file.as_ptr()) };
    let _ = rustix::fs::fdatasync(&bdev.fd);
}

/// Its number, major and minor: a file's is 0.
pub fn dev(bdev: &BlockDevice) -> c::dev_t {
    bdev.dev
}

/// Its name: the path it was opened by, its last component.
pub fn prt_name(out: &mut Printbuf, bdev: &BlockDevice) {
    out.write_bytes(bdev.name.to_bytes());
}

/// Its sysfs directory's kobject.
pub fn kobj(bdev: &BlockDevice) -> &Kobject {
    &bdev.kobj
}

/// @path's device, by number: lookup_bdev(), which userspace hasn't got.
pub fn lookup(_path: &CStr) -> Result<c::dev_t, BchError> {
    Err(c::EINVAL.into())
}

/* BLKGETSIZE64: _IOR(0x12, 114, size_t) */
type BlkGetSize64 = rustix::ioctl::Getter<{ rustix::ioctl::opcode::read::<usize>(0x12, 114) }, u64>;

/// The device's size, in 512 byte sectors: a file's, or the block device's.
pub fn capacity(bdev: &BlockDevice) -> u64 {
    let st = bdev.fstat();
    if !is_blk(&st) {
        return st.st_size as u64 >> 9;
    }
    unsafe { rustix::ioctl::ioctl(bdev.fd(), BlkGetSize64::new()) }.expect("BLKGETSIZE64 of an open block device") >> 9
}

/// The smallest IO @bdev takes: a file's block size, or the block device's
/// logical block size.
pub fn logical_block_size(bdev: &BlockDevice) -> u32 {
    let st = bdev.fstat();
    if !is_blk(&st) {
        return st.st_blksize as u32;
    }
    rustix::fs::ioctl_blksszget(bdev.fd()).expect("BLKSSZGET of an open block device")
}

/// The most @bdev discards at once, in sectors - 0: userspace discards
/// nothing.
pub fn max_discard_sectors(_bdev: &BlockDevice) -> u32 {
    0
}

/// Discard @nr_sectors from @sector on @bdev, and wait for it: userspace
/// discards nothing.
pub fn issue_discard(_bdev: &BlockDevice, _sector: u64, _nr_sectors: u64, _gfp: c::gfp_t)
    -> Result<(), BchError>
{
    Ok(())
}

/// Whether @bdev is a spinning disk, as its queue in sysfs says: a file
/// isn't.
pub fn rotational(bdev: &BlockDevice) -> bool {
    let st = bdev.fstat();
    if !is_blk(&st) {
        return false;
    }

    let dev = st.st_rdev;
    let path = format!("/sys/dev/block/{}:{}/queue/rotational",
                       rustix::fs::major(dev), rustix::fs::minor(dev));
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .is_some_and(|v| v != 0)
}

/// Its page cache dropped: userspace has none.
pub fn invalidate(_bdev: &BlockDevice) {}

/// @bio for @bdev: bio_set_dev(). To the C bio, a struct block_device *,
/// which only points to it.
pub fn bio_set_dev(bio: &mut c::bio, bdev: *mut BlockDevice) {
    bio.bi_bdev = bdev.cast();
}

/// The device @bio is for, if it's set: bio_set_dev()'s.
///
/// # Safety
/// @bio's device, if set, is open.
pub unsafe fn bio_bdev<'a>(bio: &c::bio) -> Option<&'a BlockDevice> {
    unsafe { bio.bi_bdev.cast::<BlockDevice>().as_ref() }
}

// ── IO ───────────────────────────────────────────────────────────────────

/// The io_uring IO goes through, from init() - till then, and where there's
/// no io_uring, it's synchronous, by syscall.
static RING: OnceLock<Option<Ring>> = OnceLock::new();

struct Ring {
    ring:    IoUring,
    /* submitters, one at a time on the submission queue */
    sq_lock: Mutex<()>,
    /* bumped as completions are reaped, for a submitter the queue's full for */
    reaped:  EventCount,
}

/// A completion of no bio's: a preflush, linked ahead of its bio's IO.
const NO_BIO: u64 = 0;

/// Userspace's IO set up: an io_uring and the thread completing its bios, or
/// - the kernel has no io_uring, or it's turned off - synchronous IO.
pub fn init() {
    let mut made = false;
    RING.get_or_init(|| {
        made = true;
        let ring = match IoUring::new(256) {
            Ok(ring) => ring,
            Err(e) => {
                eprintln!("bcachefs: no io_uring ({e}): IO will be synchronous");
                return None;
            }
        };
        /*
         * A bio's iovecs are on its submitter's stack, gone once the
         * submission's been made: the kernel has to have read them by then.
         */
        if !ring.params().is_feature_submit_stable() {
            eprintln!("bcachefs: io_uring without IORING_FEAT_SUBMIT_STABLE: IO will be synchronous");
            return None;
        }
        Some(Ring { ring, sq_lock: Mutex::new(()), reaped: EventCount::new() })
    });

    if made && RING.get().is_some_and(Option::is_some) {
        std::thread::Builder::new()
            .name("io_uring_completion".into())
            .spawn(completion_thread)
            .expect("spawning the io_uring completion thread");
    }
}

fn completion_thread() {
    let r = RING.get().and_then(Option::as_ref).expect("an io_uring, if there's a thread for it");

    loop {
        if let Err(e) = r.ring.submit_and_wait(1) {
            /* interrupted, or completions to reap before it takes more: */
            if !retryable(&e) {
                panic!("io_uring_enter() error: {e}");
            }
        }

        for cqe in unsafe { r.ring.completion_shared() } {
            let bio = cqe.user_data() as *mut c::bio;
            if bio.is_null() {
                continue;
            }
            unsafe { complete(bio, cqe.result()) };
        }
        r.reaped.wake();
    }
}

fn retryable(e: &std::io::Error) -> bool {
    use rustix::io::Errno;
    [Errno::INTR, Errno::AGAIN, Errno::BUSY].iter().any(|&r| e.raw_os_error() == Some(r.raw_os_error()))
}

/// @bio's IO done, its result @ret - bytes, or -errno - into its status.
unsafe fn complete(bio: *mut c::bio, ret: i32) {
    unsafe {
        let op = (*bio).bi_opf & ((1 << c::REQ_OP_BITS) - 1);
        let rw = op == c::req_op_REQ_OP_READ || op == c::req_op_REQ_OP_WRITE;

        if if rw { ret != (*bio).bi_iter.bi_size as i32 } else { ret < 0 } {
            (*bio).bi_status = if ret == -(c::ENOSPC as i32) { c::BLK_STS_NOSPC } else { c::BLK_STS_IOERR };
        }
        c::bio_endio(bio);
    }
}

/// @bio's data, as iovecs: its submitter's, till the IO's submitted.
fn iovecs(bio: &c::bio) -> Vec<libc::iovec> {
    let mut iov = Vec::new();
    for chunk in unsafe { Bio::from_raw(bio) }.range() {
        chunk.with_mapped(|p, len| iov.push(libc::iovec { iov_base: p.cast(), iov_len: len }));
    }
    iov
}

/// submit_bio(): its bi_end_io is called when it's done - from the
/// completion thread, or before this returns if the IO's synchronous.
///
/// # Safety
/// @bio is ours, ready, and nothing touches it till its endio.
pub unsafe fn submit_bio(bio: *mut c::bio) {
    match RING.get().and_then(Option::as_ref) {
        Some(r) => unsafe { submit_ring(r, bio) },
        None    => unsafe { submit_sync(bio) },
    }
}

unsafe fn submit_ring(r: &Ring, bio: *mut c::bio) {
    let b = unsafe { &mut *bio };
    let fd = types::Fd(unsafe { bio_bdev(b) }.expect("a bio submitted with no device").fd().as_raw_fd());
    let op = b.bi_opf & ((1 << c::REQ_OP_BITS) - 1);
    let offset = b.bi_iter.bi_sector << 9;
    let iov = iovecs(b);

    let io = match op {
        c::req_op_REQ_OP_READ =>
            opcode::Readv::new(fd, iov.as_ptr(), iov.len() as u32).offset(offset).build(),
        c::req_op_REQ_OP_WRITE =>
            opcode::Writev::new(fd, iov.as_ptr(), iov.len() as u32)
                .offset(offset)
                .rw_flags(if b.bi_opf & c::REQ_FUA != 0 { libc::RWF_SYNC } else { 0 })
                .build(),
        c::req_op_REQ_OP_FLUSH => opcode::Fsync::new(fd).build(),
        c::req_op_REQ_OP_DISCARD =>
            opcode::Fallocate::new(fd, b.bi_iter.bi_size.into())
                .offset(offset)
                .mode(libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE)
                .build(),
        _ => panic!("submit_bio(): a bio of op {op}"),
    }.user_data(bio as u64);

    /* A preflush syncs what's written before the IO starts: */
    let preflush = (b.bi_opf & c::REQ_PREFLUSH != 0).then(|| {
        opcode::Fsync::new(fd)
            .flags(types::FsyncFlags::DATASYNC)
            .build()
            .flags(squeue::Flags::IO_LINK)
            .user_data(NO_BIO)
    });

    loop {
        let seen = r.reaped.snapshot();
        {
            let _g = r.sq_lock.lock().expect("io_uring submission lock poisoned");
            let mut sq = unsafe { r.ring.submission_shared() };
            let need = 1 + usize::from(preflush.is_some());
            if sq.capacity() - sq.len() >= need {
                unsafe {
                    if let Some(p) = &preflush {
                        sq.push(p).expect("room for a preflush, checked");
                    }
                    sq.push(&io).expect("room for an IO, checked");
                }
                sq.sync();

                /*
                 * Till the kernel's taken it: the iovecs go when this
                 * returns. Still under the lock, so it's the last entry.
                 */
                loop {
                    if let Err(e) = r.ring.submit() {
                        if !retryable(&e) {
                            panic!("io_uring_enter() submitting: {e}");
                        }
                    }
                    sq.sync();
                    if sq.is_empty() {
                        return;
                    }
                    std::thread::yield_now();
                }
            }
        }
        /* The queue's full: wait for the completion thread to reap */
        let _ = r.reaped.wait(seen, Sleep::UNINTERRUPTIBLE.io());
    }
}

unsafe fn submit_sync(bio: *mut c::bio) {
    let b = unsafe { &mut *bio };
    let fd = unsafe { bio_bdev(b) }.expect("a bio submitted with no device").fd();
    let op = b.bi_opf & ((1 << c::REQ_OP_BITS) - 1);
    let offset = b.bi_iter.bi_sector << 9;

    let preflush = if b.bi_opf & c::REQ_PREFLUSH != 0 { rustix::fs::fdatasync(fd) } else { Ok(()) };
    if let Err(e) = preflush {
        eprintln!("fsync error: {}", std::io::Error::from(e));
        b.bi_status = c::BLK_STS_IOERR;
        unsafe { c::bio_endio(bio) };
        return;
    }

    let ret = match op {
        c::req_op_REQ_OP_READ | c::req_op_REQ_OP_WRITE => {
            let iov = iovecs(b);
            let ret = unsafe {
                if op == c::req_op_REQ_OP_READ {
                    libc::preadv(fd.as_raw_fd(), iov.as_ptr(), iov.len() as i32, offset as i64)
                } else {
                    libc::pwritev2(fd.as_raw_fd(), iov.as_ptr(), iov.len() as i32, offset as i64,
                                   if b.bi_opf & c::REQ_FUA != 0 { libc::RWF_SYNC } else { 0 })
                }
            };

            /*
             * A short read is the ordinary result of probing: the device
             * scan reads a superblock off every block device on the
             * machine, and the ones that aren't ours are frequently too
             * small for the read. Saying so is the caller's business -
             * read_super_silent() is called silent because it is meant to
             * be, and it cannot be if the layer under it talks.
             *
             * A negative ret is a real failure and still gets said, once,
             * with its errno. Either way bi_status carries the verdict to
             * whoever asked, which is how the block layer we stand in for
             * does it.
             */
            if ret < 0 {
                let e = std::io::Error::last_os_error();
                eprintln!("IO error: {e}");
                -e.raw_os_error().unwrap_or(c::EIO as i32)
            } else {
                ret as i32
            }
        }
        c::req_op_REQ_OP_FLUSH =>
            rustix::fs::fsync(fd).map_or_else(|e| -e.raw_os_error(), |()| 0),
        c::req_op_REQ_OP_DISCARD =>
            rustix::fs::fallocate(fd, rustix::fs::FallocateFlags::PUNCH_HOLE | rustix::fs::FallocateFlags::KEEP_SIZE,
                                  offset, b.bi_iter.bi_size.into())
                .map_or_else(|e| -e.raw_os_error(), |()| 0),
        _ => panic!("submit_bio(): a bio of op {op}"),
    };

    if (op == c::req_op_REQ_OP_READ || op == c::req_op_REQ_OP_WRITE) &&
        b.bi_opf & c::REQ_FUA != 0 && ret >= 0 && rustix::fs::fdatasync(fd).is_err() {
        eprintln!("fsync error: {}", std::io::Error::last_os_error());
        b.bi_status = c::BLK_STS_IOERR;
    }

    unsafe { complete(bio, ret) };
}

/// submit_bio_wait(): submitted, and waited for, its own endio not called.
///
/// # Safety
/// As submit_bio()'s.
pub unsafe fn submit_bio_wait(bio: *mut c::bio) -> Result<(), BchError> {
    let done = EventCount::new();
    let seen = done.snapshot();

    unsafe {
        (*bio).bi_private = ptr::from_ref(&done).cast_mut().cast();
        (*bio).bi_end_io  = Some(submit_bio_wait_endio);
        (*bio).bi_opf    |= c::REQ_SYNC;
        submit_bio(bio);
    }

    /* the endio's wake() is its last touch of @done */
    while done.snapshot() == seen {
        let _ = done.wait(seen, Sleep::UNINTERRUPTIBLE.io());
    }
    ret_to_result_void(unsafe { c::blk_status_to_errno((*bio).bi_status) })
}

unsafe extern "C" fn submit_bio_wait_endio(bio: *mut c::bio) {
    unsafe { (*(*bio).bi_private.cast::<EventCount>()).wake() };
}

/// @f, as with the block layer's plug held: userspace's has no plug.
pub fn plugged<R>(f: impl FnOnce() -> R) -> R {
    f()
}
