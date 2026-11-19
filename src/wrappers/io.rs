// SPDX-License-Identifier: GPL-2.0
//
// IO operations on a bcachefs filesystem.
//
// read() and write() each run the read or write path's on a task, and wait
// for it.

use std::mem::MaybeUninit;

use bcachefs_kernel::c;
use bcachefs_kernel::btree::bkey::spos;
use bcachefs_kernel::data::read::{self, Rbio};
use bcachefs_kernel::data::write;
use bcachefs_kernel::errcode::BchError;
use bcachefs_kernel::fs::Fs;
use bcachefs_kernel::inode_opts;
use bcachefs_kernel::util::bio::{Bio, BioBox, BioOwner};
use bcachefs_kernel::util::sleep::Sleep;
use bcachefs_kernel::util::task::{waiter, CompletionReturn, DoneWith, Resume, Spawn, TaskSlot, WaitSync};
use bcachefs_kernel::util::vstructs::assume_init_mut;
use bcachefs_kernel::os::workqueue::Queue;

/// Maximum single IO size.
pub const MAX_IO_SIZE: usize = 1 << 20;
const PAGE_SIZE: usize = 4096;
const MAX_BVECS: usize = MAX_IO_SIZE / PAGE_SIZE;

/// Result of a write operation.
pub struct WriteResult {
    pub sectors_delta: i64,
}

// A write's op and the bvecs its bio maps @data with: on the heap, as the
// bio points into it.
struct WriteState {
    op:    c::bch_write_op,
    bvecs: [c::bio_vec; MAX_BVECS],
}

// handed to the write's task: its bvecs map the caller's @data, which the
// caller holds till the task's done
unsafe impl Send for WriteState {}

/// @data written at @offset of inode @inum, by a task, waited for.
pub fn write(
    fs: &Fs,
    inum: u64,
    offset: u64,
    subvol: u32,
    replicas: u32,
    data: &[u8],
    new_i_size: u64,
) -> Result<WriteResult, BchError> {
    // Made in place: the op's bio first, which the op's init() leaves be
    let mut state = Box::new(std::mem::MaybeUninit::<WriteState>::zeroed());
    let p = state.as_mut_ptr();
    let mut state = unsafe {
        let bio = (&raw mut (*p).op.wbio.bio).cast::<c::bio>();
        bcachefs_kernel::os::bio::init(bio, std::ptr::null_mut(), (&raw mut (*p).bvecs).cast(),
                                       MAX_BVECS as u16, 0);
        Bio::from_raw_mut(bio).map(data.as_ptr().cast_mut(), data.len());
        c::bch_write_op::init_at(&raw mut (*p).op, fs, inode_opts::get_fs(fs, false));
        state.assume_init()
    };
    let op = &mut state.op;
    op.write_point = c::write_point_specifier::hashed(0);
    op.bits.set_nr_replicas(replicas as u8);
    op.subvol      = subvol;
    op.pos         = spos(inum, offset >> 9, u32::MAX);
    op.new_i_size  = new_i_size;

    op.res.add((data.len() >> 9) as u64, replicas, c::ReservationFlags::empty())?;

    let slot = TaskSlot::alloc(())?;
    let mut done = DoneWith::new();
    let (wait, r) = CompletionReturn::new(&mut done, waiter::Sync::current());
    let task = slot.init(Spawn::Queue(Queue::system_dfl()), fs.tasks(), async move |_: &()| {
        let mut state = state;
        let op = &mut state.op;
        write::write(op, None, Resume::queue(Queue::system_dfl())).await;
        r.complete(match op.error as i32 {
            0     => Ok(WriteResult { sectors_delta: op.i_sectors_delta }),
            error => Err(BchError::from_raw(-error)),
        });
    }, None).expect("a queue's task: nothing in it fails");
    unsafe { task.start_unchecked() };
    wait.wait_sync(Sleep::UNINTERRUPTIBLE).expect("a write's task, ended with what it wrote")
}

/// @buf read from @offset of inode @inum, by a task, waited for: the buffer
/// as bytes - the read writes all of it, holes and what's past the data as
/// zeroes.
pub fn read<'a>(
    fs: &Fs,
    inum: c::subvol_inum,
    offset: u64,
    inode: &c::bch_inode_unpacked,
    buf: &'a mut [MaybeUninit<u8>],
) -> Result<&'a mut [u8], BchError> {
    type F = c::ReadFlags;
    let opts = bcachefs_kernel::inode_opts::get_inode(fs, inode);

    let mut b = BioBox::new(std::ptr::null_mut(), 1, c::req_op_REQ_OP_READ,
                            |b| Rbio::new(fs, opts, inum.subvol as u32, None, b))?;
    b.bio_mut().set_opf(c::req_op_REQ_OP_READ | c::REQ_SYNC);
    b.bio_mut().set_sector(offset >> 9);
    unsafe { b.bio_mut().map(buf.as_mut_ptr().cast(), buf.len()) };

    let flags = F::retry_if_stale | F::may_promote | F::user_mapped;
    let slot = TaskSlot::alloc(())?;
    let mut done = DoneWith::new();
    let (wait, r) = CompletionReturn::new(&mut done, waiter::Sync::current());
    let task = slot.init(Spawn::Queue(Queue::system_dfl()), fs.tasks(), async move |_: &()| {
        r.complete(read::read(&b, inum, flags, ()).await);
    }, None).expect("a queue's task: nothing in it fails");
    unsafe { task.start_unchecked() };
    wait.wait_sync(Sleep::UNINTERRUPTIBLE).expect("a read's task, ended with what it read")?;
    Ok(unsafe { assume_init_mut(buf) })
}
