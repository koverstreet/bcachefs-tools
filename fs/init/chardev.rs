// SPDX-License-Identifier: GPL-2.0

//! The control device's ioctls, as they're converted from chardev.c: each
//! called by its dispatch there.

use crate::c;
use crate::errcode::{bch_errcode, BchError};
use crate::fs::{Fs, WriteRef};
use crate::util::ffi::Opaque;
use crate::util::file::{File, FileOps, UserBuf};
use crate::util::kernel::{capable, Capability};
use crate::util::thread_with_file::{ThreadFile, ThreadWithFile};
use core::cell::UnsafeCell;
use core::mem::size_of;
use core::ptr::{read_volatile, NonNull};
use core::sync::atomic::Ordering::Relaxed;
use cstruct_macros::{c_export, vtable};
use kernel::ffi::{c_int, c_long};

/* BCH_IOCTL_DATA */

/// A data job - scrub, rereplicate, migrate...: bch2_data_job() in a
/// kthread, and a file whose read() is its progress. Not a stdio file: each
/// read is the progress now, an event, not the next of a stream.
struct DataJob {
    /// XXX: good while the job's write ref is held - until its thread returns
    /// - but the file can be read after: a read once the filesystem's gone is
    /// a use after free, as it was in C
    fs:    NonNull<Fs>,
    arg:   c::bch_ioctl_data,
    /// The thread's, read() reads as it goes: racily, as C does
    stats: UnsafeCell<c::bch_move_stats>,
}

// SAFETY: the filesystem's every thread's; stats are read racily, as above
unsafe impl Send for DataJob {}
unsafe impl Sync for DataJob {}

impl ThreadFile for DataJob {}

fn data_job(job: &DataJob, _write_ref: WriteRef<'_>) {
    let mut arg = job.arg;
    let stats = job.stats.get();
    let ret = unsafe { c::bch2_data_job(job.fs.as_ptr().cast(), stats, &mut arg) };

    // SAFETY: the thread's; read() reads it racily
    unsafe {
        if ret == -(bch_errcode::BCH_ERR_device_offline.0 as c_int) {
            (*stats).ret = c::bch_ioctl_data_event_ret::BCH_IOCTL_DATA_EVENT_RET_device_offline;
        } else {
            (*stats).ret = c::bch_ioctl_data_event_ret::BCH_IOCTL_DATA_EVENT_RET_done;
            (*stats).at.logical.data_type = c::bch_data_type(c::DATA_PROGRESS_DATA_TYPE_done);
        }
    }
}

#[vtable]
impl FileOps for DataJob {
    fn read(&self, _file: &File, buf: &mut UserBuf) -> Result<(), BchError> {
        let fs = unsafe { self.fs.as_ref() };
        // Never a & of the whole: the job's thread writes the plain fields as
        // we read. Those are READ_ONCE()s, racy as C's - pos may tear - and
        // a data job's stats are logical; the counters are atomics.
        let stats = self.stats.get();
        let (ret, data_type, pos) = unsafe {(
            read_volatile(&raw const (*stats).ret),
            read_volatile(&raw const (*stats).at.logical.data_type),
            read_volatile(&raw const (*stats).at.logical.pos),
        )};

        let mut e = c::bch_ioctl_data_event::default();
        e.type_ = c::BCH_DATA_EVENT_PROGRESS as u8;
        e.ret   = ret.0 as u8;

        let mut p = c::bch_ioctl_data_progress::default();
        p.data_type                 = data_type.0 as u8;
        p.btree_id                  = pos.btree.0 as u8;
        p.pos                       = pos.pos;
        p.sectors_done              = unsafe { &(*stats).sectors_seen }.load(Relaxed) as u64;
        p.sectors_error_corrected   = unsafe { &(*stats).sectors_error_corrected }.load(Relaxed) as u64;
        p.sectors_error_uncorrected = unsafe { &(*stats).sectors_error_uncorrected }.load(Relaxed) as u64;

        if u32::from(self.arg.op) == c::BCH_DATA_OP_scrub {
            // SAFETY: scrub's
            let scrub = unsafe { self.arg.args.scrub };
            if let Some(ca) = fs.dev_get(scrub.dev) {
                let u = ca.usage_full_read_fast();
                p.sectors_total = (c::BCH_DATA_btree as usize..u.d.len())
                    .filter(|&i| scrub.data_types & (1 << i) != 0)
                    .map(|i| u.d[i].sectors)
                    .sum();
            }
        } else {
            p.sectors_total = fs.usage_read_short().used;
        }
        e.payload.p = p;

        if buf.len() < size_of::<c::bch_ioctl_data_event>() {
            return fs.throw(bch_errcode::BCH_ERR_EINVAL_ioctl_data_read_short_buf);
        }

        // SAFETY: zero initialized, then set: every byte is initialized
        let bytes = unsafe {
            core::slice::from_raw_parts((&raw const e).cast::<u8>(), size_of::<c::bch_ioctl_data_event>())
        };
        if buf.copy_to(bytes) != bytes.len() {
            return Err(c::EFAULT.into());
        }
        Ok(())
    }
}

fn data(fs: &Fs, arg: c::bch_ioctl_data) -> Result<c_int, BchError> {
    let write_ref = fs.write_ref_tryget(c::bch_write_ref::BCH_WRITE_REF_ioctl_data)
        .ok_or(BchError::from(c::EROFS))?;

    if !capable(Capability::SysAdmin) {
        return fs.throw(bch_errcode::BCH_ERR_EPERM_non_admin);
    }

    if u32::from(arg.op) >= c::BCH_DATA_OP_NR || arg.flags != 0 {
        return fs.throw(bch_errcode::BCH_ERR_EINVAL_ioctl_data_bad_op);
    }

    // SAFETY: a write ref keeps the filesystem: it can't go read-only, let
    // alone be freed, until the ref's put - when the job's thread returns
    let write_ref: WriteRef<'static> = unsafe { core::mem::transmute(write_ref) };

    let job = ThreadWithFile::new(Ok(DataJob {
        fs:    NonNull::from(fs),
        arg,
        // SAFETY: integers, and the zeroed state bch2_data_job() inits it from
        stats: UnsafeCell::new(unsafe { core::mem::zeroed() }),
    }))?;
    job.run(move |job| data_job(job, write_ref))
}

#[c_export]
pub fn bch2_ioctl_data(c: &Opaque<c::bch_fs>, arg: c::bch_ioctl_data) -> c_long {
    match data(Fs::from_c(c), arg) {
        Ok(fd) => fd as c_long,
        Err(e) => -e.raw() as c_long,
    }
}
