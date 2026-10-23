// SPDX-License-Identifier: GPL-2.0

//! The control device's ioctls: their dispatch, and each ioctl as it's
//! converted from chardev.c. chardev.c finds the filesystem, by the device's
//! minor, and holds a ro_ref on it across the call; bch2_fs_ioctl() and
//! bch2_global_ioctl(), without one, are the dispatch. What isn't converted
//! yet it hands back to C, __bch2_fs_ioctl().
//!
//! Before a filesystem's started, only its queries are answered: anything
//! else is EINVAL_ioctl_not_started. BCH_FS_started is read once, here, and
//! given to __bch2_fs_ioctl(), so the two halves of the dispatch can't
//! disagree on it.
//!
//! The device ioctls come in pairs, a v1 and a v2 whose argument is v1's and
//! a buffer for the error message: one handler each, given an ErrMsg, which
//! is v1's logging - each as C had it - or v2's buffer. A device is named by
//! index or by path (BCH_BY_INDEX); a handler that takes state_lock holds
//! only an outer ref on it, as device removal drains ca->ref under that lock.

use crate::c;
use crate::errcode::{bch_errcode, BchError};
use crate::fs::{Fs, WriteRef};
use crate::util::ffi::Opaque;
use crate::util::file::{File, FileOps, UserBuf};
use crate::util::ioctl::{input, vstruct, Ioctl, UserStr};
use crate::util::Printbuf;
use crate::errcode::{errptr_to_result, ret_to_c, ret_to_result_void};
use crate::sb::members::{DevOuterRef, DevRef};
use crate::{bch_err, bch_err_dev, bch_err_msg, bch_err_msg_dev, prt};
use crate::util::kernel::{capable, Capability};
use crate::util::thread_with_file::{ThreadFile, ThreadWithFile};
use core::cell::UnsafeCell;
use core::mem::size_of;
use core::ptr::{read_volatile, NonNull};
use core::sync::atomic::Ordering::Relaxed;
use core::ffi::c_void;
use cstruct_macros::{c_export, c_extern, vtable};
use kernel::ffi::{c_int, c_long, c_uint};
use kernel::uaccess::{UserPtr, UserSlice};

/* The dispatch */

c_extern! {
    pub fn __bch2_fs_ioctl(c: *mut c::bch_fs, cmd: c_uint, arg: *mut c_void, started: bool) -> c_long;
}

fn c_fs_ioctl(fs: &Fs, cmd: c_uint, user: UserPtr, started: bool) -> Result<c_long, BchError> {
    let ret = unsafe { __bch2_fs_ioctl(fs.as_ptr().cast(), cmd, user.as_mut_ptr(), started) };
    if ret < 0 { Err(BchError::from_raw(-ret as i32)) } else { Ok(ret) }
}

pub fn fs_ioctl(fs: &Fs, cmd: c_uint, user: UserPtr) -> Result<c_long, BchError> {
    use crate::check::ioctl::fsck_online;
    use crate::sb::counters::ioctl::query_counters;

    use bch_errcode as E;

    let started = fs.flag(c::bch_fs_flags::BCH_FS_started);
    match cmd {
        _ if !started => c_fs_ioctl(fs, cmd, user, started),

        <c::BCH_IOCTL_DATA as Ioctl>::OPCODE =>
            input::<c::BCH_IOCTL_DATA, _>(user, |arg| data(fs, arg)),
        <c::BCH_IOCTL_FSCK_ONLINE as Ioctl>::OPCODE =>
            input::<c::BCH_IOCTL_FSCK_ONLINE, _>(user, |arg| fsck_online(fs, arg)),
        <c::BCH_IOCTL_QUERY_COUNTERS as Ioctl>::OPCODE =>
            vstruct::<c::BCH_IOCTL_QUERY_COUNTERS, _, _>(user, |arg| query_counters(fs, arg)),

        <c::BCH_IOCTL_DISK_ADD as Ioctl>::OPCODE =>
            input::<c::BCH_IOCTL_DISK_ADD, _>(user, |a| disk_add(fs, a, ErrMsg::Log, E::BCH_ERR_EINVAL_ioctl_disk_add_bad_flags)),
        <c::BCH_IOCTL_DISK_ADD_v2 as Ioctl>::OPCODE =>
            input::<c::BCH_IOCTL_DISK_ADD_v2, _>(user, |a| { let (a, m) = a.split(); disk_add(fs, a, m, E::BCH_ERR_EINVAL_ioctl_disk_add_v2_bad_flags) }),
        <c::BCH_IOCTL_DISK_REMOVE as Ioctl>::OPCODE =>
            input::<c::BCH_IOCTL_DISK_REMOVE, _>(user, |a| disk_remove(fs, a, ErrMsg::Log, E::BCH_ERR_EINVAL_ioctl_disk_remove_bad_flags)),
        <c::BCH_IOCTL_DISK_REMOVE_v2 as Ioctl>::OPCODE =>
            input::<c::BCH_IOCTL_DISK_REMOVE_v2, _>(user, |a| { let (a, m) = a.split(); disk_remove(fs, a, m, E::BCH_ERR_EINVAL_ioctl_disk_remove_v2_bad_flags) }),
        <c::BCH_IOCTL_DISK_ONLINE as Ioctl>::OPCODE =>
            input::<c::BCH_IOCTL_DISK_ONLINE, _>(user, |a| disk_online(fs, a, ErrMsg::Log, E::BCH_ERR_EINVAL_ioctl_disk_online_bad_flags)),
        <c::BCH_IOCTL_DISK_ONLINE_v2 as Ioctl>::OPCODE =>
            input::<c::BCH_IOCTL_DISK_ONLINE_v2, _>(user, |a| { let (a, m) = a.split(); disk_online(fs, a, m, E::BCH_ERR_EINVAL_ioctl_disk_online_v2_bad_flags) }),
        <c::BCH_IOCTL_DISK_OFFLINE as Ioctl>::OPCODE =>
            input::<c::BCH_IOCTL_DISK_OFFLINE, _>(user, |a| disk_offline(fs, a, ErrMsg::Log, E::BCH_ERR_EINVAL_ioctl_disk_offline_bad_flags)),
        <c::BCH_IOCTL_DISK_OFFLINE_v2 as Ioctl>::OPCODE =>
            input::<c::BCH_IOCTL_DISK_OFFLINE_v2, _>(user, |a| { let (a, m) = a.split(); disk_offline(fs, a, m, E::BCH_ERR_EINVAL_ioctl_disk_offline_v2_bad_flags) }),
        <c::BCH_IOCTL_DISK_SET_STATE as Ioctl>::OPCODE =>
            input::<c::BCH_IOCTL_DISK_SET_STATE, _>(user, |a| disk_set_state(fs, a, ErrMsg::Log, E::BCH_ERR_EINVAL_ioctl_disk_set_state_bad_args)),
        <c::BCH_IOCTL_DISK_SET_STATE_v2 as Ioctl>::OPCODE =>
            input::<c::BCH_IOCTL_DISK_SET_STATE_v2, _>(user, |a| { let (a, m) = a.split(); disk_set_state(fs, a, m, E::BCH_ERR_EINVAL_ioctl_disk_set_state_v2_bad_args) }),
        <c::BCH_IOCTL_DISK_RESIZE as Ioctl>::OPCODE =>
            input::<c::BCH_IOCTL_DISK_RESIZE, _>(user, |a| disk_resize(fs, a, ErrMsg::Log, E::BCH_ERR_EINVAL_ioctl_disk_resize_bad_flags)),
        <c::BCH_IOCTL_DISK_RESIZE_v2 as Ioctl>::OPCODE =>
            input::<c::BCH_IOCTL_DISK_RESIZE_v2, _>(user, |a| { let (a, m) = a.split(); disk_resize(fs, a, m, E::BCH_ERR_EINVAL_ioctl_disk_resize_v2_bad_flags) }),
        <c::BCH_IOCTL_DISK_RESIZE_JOURNAL as Ioctl>::OPCODE =>
            input::<c::BCH_IOCTL_DISK_RESIZE_JOURNAL, _>(user, |a| disk_resize_journal(fs, a, ErrMsg::Log,
                E::BCH_ERR_EINVAL_ioctl_disk_resize_journal_bad_flags, E::BCH_ERR_EINVAL_ioctl_disk_resize_journal_too_big)),
        <c::BCH_IOCTL_DISK_RESIZE_JOURNAL_v2 as Ioctl>::OPCODE =>
            input::<c::BCH_IOCTL_DISK_RESIZE_JOURNAL_v2, _>(user, |a| { let (a, m) = a.split(); disk_resize_journal(fs, a, m,
                E::BCH_ERR_EINVAL_ioctl_disk_resize_journal_v2_bad_flags, E::BCH_ERR_EINVAL_ioctl_disk_resize_journal_v2_too_big) }),

        _ => c_fs_ioctl(fs, cmd, user, started),
    }
}

fn global_ioctl(cmd: c_uint, user: UserPtr) -> Result<c_long, BchError> {
    use crate::check::ioctl::fsck_offline;

    match cmd {
        <c::BCH_IOCTL_FSCK_OFFLINE as Ioctl>::OPCODE =>
            vstruct::<c::BCH_IOCTL_FSCK_OFFLINE, _, _>(user, fsck_offline),
        _ => Err(c::ENOTTY.into()),
    }
}

/// What the syscall returns: an error as its class.
pub fn ioctl_ret(ret: Result<c_long, BchError>) -> c_long {
    ret.unwrap_or_else(|e| e.class() as c_long)
}

#[c_export]
pub fn bch2_fs_ioctl(c: &Opaque<c::bch_fs>, cmd: c_uint, arg: *mut c_void) -> c_long {
    ioctl_ret(fs_ioctl(Fs::from_c(c), cmd, UserPtr::from_ptr(arg)))
}

#[c_export]
pub fn bch2_global_ioctl(cmd: c_uint, arg: *mut c_void) -> c_long {
    ioctl_ret(global_ioctl(cmd, UserPtr::from_ptr(arg)))
}

/* The device ioctls */

enum ErrMsg {
    Log,
    User(c::bch_ioctl_err_msg),
}

impl ErrMsg {
    /// @ret, @err logged by @log if this is v1's, else copied to the user's
    /// buffer, @ret's error after it.
    fn report(self, err: &mut Printbuf, ret: Result<(), BchError>,
              log: impl FnOnce(Result<(), BchError>, &Printbuf) -> Result<(), BchError>)
        -> Result<(), BchError>
    {
        match self {
            ErrMsg::Log       => log(ret, err),
            ErrMsg::User(dst) => copy_err_msg(&dst, err, ret),
        }
    }
}

/// What fits of @err, @ret's error appended, to the user's buffer @dst: @ret,
/// or the copy's error.
fn copy_err_msg(dst: &c::bch_ioctl_err_msg, err: &mut Printbuf, ret: Result<(), BchError>)
    -> Result<(), BchError>
{
    if let Err(e) = &ret {
        prt!(err, "error={}", e.msg());
    }
    let msg = err.as_bytes();
    if !msg.is_empty() {
        let n = msg.len().min(dst.msg_len as usize);
        UserSlice::new(UserPtr::from_addr(dst.msg_ptr as usize), n).writer().write_slice(&msg[..n])?;
    }
    ret
}

/// # Safety
/// @dst and @src are live.
#[c_export]
pub unsafe fn bch2_copy_ioctl_err_msg(dst: *mut c::bch_ioctl_err_msg, src: *mut c::printbuf,
                                      ret: c_int) -> c_int {
    let (dst, src) = unsafe { (&*dst, Printbuf::borrow_raw(src)) };
    ret_to_c(copy_err_msg(dst, src, ret_to_result_void(ret)))
}

/// A v2 device ioctl's argument: its v1's, and the error message's buffer.
trait V2 {
    type V1;
    fn split(self) -> (Self::V1, ErrMsg);
}

impl V2 for c::bch_ioctl_disk_v2 {
    type V1 = c::bch_ioctl_disk;
    fn split(self) -> (c::bch_ioctl_disk, ErrMsg) {
        (c::bch_ioctl_disk { flags: self.flags, pad: self.pad, dev: self.dev }, ErrMsg::User(self.err))
    }
}

impl V2 for c::bch_ioctl_disk_set_state_v2 {
    type V1 = c::bch_ioctl_disk_set_state;
    fn split(self) -> (c::bch_ioctl_disk_set_state, ErrMsg) {
        (c::bch_ioctl_disk_set_state { flags: self.flags, new_state: self.new_state, pad: self.pad, dev: self.dev },
         ErrMsg::User(self.err))
    }
}

impl V2 for c::bch_ioctl_disk_resize_v2 {
    type V1 = c::bch_ioctl_disk_resize;
    fn split(self) -> (c::bch_ioctl_disk_resize, ErrMsg) {
        (c::bch_ioctl_disk_resize { flags: self.flags, pad: self.pad, dev: self.dev, nbuckets: self.nbuckets },
         ErrMsg::User(self.err))
    }
}

impl V2 for c::bch_ioctl_disk_resize_journal_v2 {
    type V1 = c::bch_ioctl_disk_resize_journal;
    fn split(self) -> (c::bch_ioctl_disk_resize_journal, ErrMsg) {
        (c::bch_ioctl_disk_resize_journal { flags: self.flags, pad: self.pad, dev: self.dev, nbuckets: self.nbuckets },
         ErrMsg::User(self.err))
    }
}

/// What remove, offline and set_state take: the data they may lose.
const FORCE_FLAGS: u32 = c::BCH_FORCE_IF_DATA_LOST | c::BCH_FORCE_IF_METADATA_LOST |
    c::BCH_FORCE_IF_DEGRADED | c::BCH_BY_INDEX;

fn dev_lookup(fs: &Fs, dev: u64, flags: u32) -> Result<DevRef, BchError> {
    if flags & c::BCH_BY_INDEX != 0 {
        if dev >= u64::from(fs.nr_devices()) {
            return Err(c::EINVAL.into());
        }
        fs.dev_get(dev as u32).ok_or(BchError::from(c::EINVAL))
    } else {
        let path = UserStr::read(dev, c::PATH_MAX as usize)?;
        let ca = unsafe { c::bch2_dev_lookup(fs.as_ptr().cast(), path.as_cstr().as_ptr().cast()) };
        let ca = errptr_to_result(NonNull::new(ca).expect("a device, or an error"))?;
        // SAFETY: bch2_dev_lookup()'s ref
        Ok(unsafe { DevRef::from_raw(ca.as_ptr()) })
    }
}

fn dev_lookup_outer(fs: &Fs, dev: u64, flags: u32) -> Result<DevOuterRef, BchError> {
    Ok(dev_lookup(fs, dev, flags)?.outer_ref())
}

fn admin(fs: &Fs) -> Result<(), BchError> {
    if capable(Capability::SysAdmin) { Ok(()) } else { fs.throw(bch_errcode::BCH_ERR_EPERM_non_admin) }
}

fn disk_add(fs: &Fs, a: c::bch_ioctl_disk, msg: ErrMsg, bad_flags: bch_errcode) -> Result<(), BchError> {
    admin(fs)?;
    if a.flags != 0 || a.pad != 0 {
        return fs.throw(bad_flags);
    }
    let path = UserStr::read(a.dev, c::PATH_MAX as usize)?;

    let mut err = Printbuf::new();
    let ret = ret_to_result_void(unsafe {
        c::bch2_dev_add(fs.as_ptr().cast(), path.as_cstr().as_ptr().cast(), err.as_raw())
    });
    msg.report(&mut err, ret, |ret, err| bch_err_msg!(fs, ret, "{}", err))
}

fn disk_remove(fs: &Fs, a: c::bch_ioctl_disk, msg: ErrMsg, bad_flags: bch_errcode) -> Result<(), BchError> {
    admin(fs)?;
    if a.flags & !FORCE_FLAGS != 0 || a.pad != 0 {
        return fs.throw(bad_flags);
    }
    let ca = dev_lookup_outer(fs, a.dev, a.flags)?;

    let mut err = Printbuf::new();
    // It takes our ref_outer: nothing of ca after
    let ret = ret_to_result_void(unsafe {
        c::bch2_dev_remove(fs.as_ptr().cast(), ca.into_raw(), a.flags as c_int, err.as_raw())
    });
    msg.report(&mut err, ret, |ret, err| {
        if ret.is_err() {
            bch_err!(fs, "{}", err);
        }
        ret
    })
}

fn disk_online(fs: &Fs, a: c::bch_ioctl_disk, msg: ErrMsg, bad_flags: bch_errcode) -> Result<(), BchError> {
    admin(fs)?;
    if a.flags != 0 || a.pad != 0 {
        return fs.throw(bad_flags);
    }
    let path = UserStr::read(a.dev, c::PATH_MAX as usize)?;

    let mut err = Printbuf::new();
    let ret = ret_to_result_void(unsafe {
        c::bch2_dev_online(fs.as_ptr().cast(), path.as_cstr().as_ptr().cast(), err.as_raw())
    });
    msg.report(&mut err, ret, |ret, err| {
        if ret.is_err() {
            bch_err!(fs, "{}", err);
        }
        ret
    })
}

fn disk_offline(fs: &Fs, a: c::bch_ioctl_disk, msg: ErrMsg, bad_flags: bch_errcode) -> Result<(), BchError> {
    admin(fs)?;
    if a.flags & !FORCE_FLAGS != 0 || a.pad != 0 {
        return fs.throw(bad_flags);
    }
    let ca = dev_lookup_outer(fs, a.dev, a.flags)?;

    let mut err = Printbuf::new();
    let ret = ret_to_result_void(unsafe {
        c::bch2_dev_offline(fs.as_ptr().cast(), ca.as_mut_ptr(), a.flags as c_int, err.as_raw())
    });
    msg.report(&mut err, ret, |ret, err| {
        if ret.is_err() {
            bch_err_dev!(&ca, "{}", err);
        }
        ret
    })
}

fn disk_set_state(fs: &Fs, a: c::bch_ioctl_disk_set_state, msg: ErrMsg, bad_args: bch_errcode)
    -> Result<(), BchError>
{
    admin(fs)?;
    if a.flags & !FORCE_FLAGS != 0 || a.pad != [0; 3] ||
       u32::from(a.new_state) >= c::BCH_MEMBER_STATE_NR {
        return fs.throw(bad_args);
    }
    let ca = dev_lookup_outer(fs, a.dev, a.flags)?;

    let mut err = Printbuf::new();
    let ret = ret_to_result_void(unsafe {
        c::bch2_dev_set_state(fs.as_ptr().cast(), ca.as_mut_ptr(),
                              c::bch_member_state(a.new_state.into()), a.flags as c_int, err.as_raw())
    });
    msg.report(&mut err, ret, |ret, _| bch_err_msg_dev!(&ca, ret, "setting device state"))
}

fn disk_resize(fs: &Fs, a: c::bch_ioctl_disk_resize, msg: ErrMsg, bad_flags: bch_errcode)
    -> Result<(), BchError>
{
    admin(fs)?;
    if a.flags & !c::BCH_BY_INDEX != 0 || a.pad != 0 {
        return fs.throw(bad_flags);
    }
    let ca = dev_lookup_outer(fs, a.dev, a.flags)?;

    let mut err = Printbuf::new();
    let ret = ret_to_result_void(unsafe {
        c::bch2_dev_resize(fs.as_ptr().cast(), ca.as_mut_ptr(), a.nbuckets, err.as_raw())
    });
    msg.report(&mut err, ret, |ret, err| {
        if ret.is_err() {
            bch_err_dev!(&ca, "{}", err);
        }
        ret
    })
}

fn disk_resize_journal(fs: &Fs, a: c::bch_ioctl_disk_resize_journal, msg: ErrMsg,
                       bad_flags: bch_errcode, too_big: bch_errcode) -> Result<(), BchError> {
    admin(fs)?;
    if a.flags & !c::BCH_BY_INDEX != 0 || a.pad != 0 {
        return fs.throw(bad_flags);
    }
    if a.nbuckets > u64::from(u32::MAX) {
        return fs.throw(too_big);
    }
    let ca = dev_lookup_outer(fs, a.dev, a.flags)?;

    let ret = ret_to_result_void(unsafe {
        c::bch2_set_nr_journal_buckets(fs.as_ptr().cast(), ca.as_mut_ptr(), a.nbuckets as c_uint)
    });
    msg.report(&mut Printbuf::new(), ret, |ret, _| ret)
}

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
