// SPDX-License-Identifier: GPL-2.0
//
// IO operations on a bcachefs filesystem.
//
// read() and write() each run the read or write path's on a task, and wait
// for it.

use bcachefs_kernel::c;
use bcachefs_kernel::btree::bkey::spos;
use bcachefs_kernel::data::read::{self, Rbio};
use bcachefs_kernel::data::write;
use bcachefs_kernel::errcode::BchError;
use bcachefs_kernel::fs::Fs;
use bcachefs_kernel::inode_opts;
use bcachefs_kernel::util::alloc::init_at;
use bcachefs_kernel::os::bio::{Bio, BioBox, BioInPlace, BioSubmit, Buffer};
use bcachefs_kernel::util::sleep::Sleep;
use bcachefs_kernel::util::task::{waiter, Join, Resume, WaitSync};
use bcachefs_kernel::os::workqueue::Queue;

/// Maximum single IO size.
pub const MAX_IO_SIZE: usize = 1 << 20;

/// Result of a write operation.
pub struct WriteResult {
    pub sectors_delta: i64,
}

// A write's op and its bio, which maps @data: on the heap, made in place.
struct WriteState<'a> {
    op:   c::bch_write_op,
    wbio: Bio<Buffer<&'a [u8]>>,
}

// handed to the write's task: its bvecs map the caller's @data
unsafe impl Send for WriteState<'_> {}

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
    // Made in place: the write bio's bio, then the op
    let mut state = Box::new(std::mem::MaybeUninit::<WriteState<'_>>::zeroed());
    let p = state.as_mut_ptr();
    let mut state = unsafe {
        let bio = &raw mut (*p).wbio;
        init_at(BioInPlace::own(Buffer::nr_vecs(&data), c::req_op_REQ_OP_WRITE,
                                |at| at.init_from(|b| Ok::<_, BchError>(Buffer::fill(b, data)))), bio)?;
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

    let mut join = Join::new();
    let wait = join.spawn(Queue::system_dfl(), fs.tasks(), move || async move {
        let mut state = state;
        let WriteState { op, wbio, .. } = &mut *state;
        write::write(op, write::WriteBio::Bio(wbio), write::WriteFor::Tools, Resume::queue(Queue::system_dfl())).await;
        match op.error as i32 {
            0     => Ok(WriteResult { sectors_delta: op.i_sectors_delta }),
            error => Err(BchError::from_raw(-error)),
        }
    }, waiter::Sync::current())?;
    wait.wait_sync(Sleep::UNINTERRUPTIBLE).expect("a write's task, ended with what it wrote")
}

/// @buf read from @offset of inode @inum, by a task, waited for - all of it,
/// holes and what's past the data as zeroes.
pub fn read(
    fs: &Fs,
    inum: c::subvol_inum,
    offset: u64,
    inode: &c::bch_inode_unpacked,
    buf: &mut [u8],
) -> Result<(), BchError> {
    type F = c::ReadFlags;
    let opts = bcachefs_kernel::inode_opts::get_inode(fs, inode);

    let mut b = BioBox::new(Buffer::nr_vecs(&&mut *buf), c::req_op_REQ_OP_READ, |b| {
        let into = &mut *buf;
        Rbio::new(opts, inum.subvol as u32, None, b.init_from(move |bio| Ok(Buffer::fill(bio, into))))
    })?;
    b.bio_mut().set_opf(c::req_op_REQ_OP_READ | c::REQ_SYNC);
    b.bio_mut().set_sector(offset >> 9);

    let flags = F::retry_if_stale | F::may_promote | F::user_mapped;
    let mut join = Join::new();
    let wait = join.spawn(Queue::system_dfl(), fs.tasks(), move || async move { read::read(fs, b.erased(), inum, flags).await },
                          waiter::Sync::current())?;
    wait.wait_sync(Sleep::UNINTERRUPTIBLE).expect("a read's task, ended with what it read")
}
