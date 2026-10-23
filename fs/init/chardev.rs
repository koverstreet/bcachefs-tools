// SPDX-License-Identifier: GPL-2.0

//! The control device's ioctls, and their dispatch. chardev.c finds the
//! filesystem, by the device's minor, and holds a ro_ref on it across the
//! call to bch2_fs_ioctl() - or calls bch2_global_ioctl(), without one; a
//! mounted filesystem's file ioctls end up in bch2_fs_ioctl() too.
//!
//! Before a filesystem's started, only its queries are answered: anything
//! else is EINVAL_ioctl_not_started. The usage queries read in-memory
//! accounting, there once the accounting_read recovery pass has run - not
//! BCH_FS_started - or from the start, on a new filesystem, which runs no
//! passes; before, they're EINVAL_*_accounting_not_read, "no data yet" and
//! not zeroes. passes_complete only grows, so reading it without the lock can
//! only be stale toward not ready: one more poll, never a wrong answer.
//!
//! Some arguments point into userspace - read_super's sb, query_btree_keys'
//! buf, a v2's error message: what's written there is bounded by the length
//! the argument gives, as C's copy_to_user()s were.
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
use crate::util::ioctl::{arg, inout, input, output, vstruct, Ioctl, UserArg, UserStr, UserVstruct};
use crate::util::alloc::{darray_take, flags::GFP_KERNEL, KVVec};
use crate::util::rcu::read_lock;
use crate::util::vstructs::plain_bytes;
use crate::accounting;
use crate::btree::iter::{BtreeIter, BtreeIterFlags};
use crate::btree_trans;
use core::mem::offset_of;
use core::ops::ControlFlow;
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
use cstruct_macros::{c_export, vtable};
use kernel::ffi::{c_int, c_long, c_uint};
use kernel::uaccess::{UserPtr, UserSlice};

/* The dispatch */

pub fn fs_ioctl(fs: &Fs, cmd: c_uint, user: UserPtr) -> Result<c_long, BchError> {
    use crate::check::ioctl::fsck_online;
    use crate::sb::counters::ioctl::query_counters;

    use bch_errcode as E;

    match cmd {
        <c::BCH_IOCTL_QUERY_UUID as Ioctl>::OPCODE =>
            output::<c::BCH_IOCTL_QUERY_UUID>(user, || query_uuid(fs)),
        <c::BCH_IOCTL_FS_USAGE as Ioctl>::OPCODE =>
            arg::<c::BCH_IOCTL_FS_USAGE, _>(user, |a| fs_usage(fs, a)),
        <c::BCH_IOCTL_DEV_USAGE as Ioctl>::OPCODE => {
            // before the argument's read, as C had it
            if !accounting_read_done(fs) {
                return fs.throw(E::BCH_ERR_EINVAL_ioctl_dev_usage_accounting_not_read);
            }
            inout::<c::BCH_IOCTL_DEV_USAGE, _>(user, |a| dev_usage(fs, a))
        }
        <c::BCH_IOCTL_DEV_USAGE_V2 as Ioctl>::OPCODE =>
            vstruct::<c::BCH_IOCTL_DEV_USAGE_V2, _, _>(user, |a| dev_usage_v2(fs, a)),
        <c::BCH_IOCTL_READ_SUPER as Ioctl>::OPCODE =>
            input::<c::BCH_IOCTL_READ_SUPER, _>(user, |a| read_super(fs, a)),
        <c::BCH_IOCTL_DISK_GET_IDX as Ioctl>::OPCODE =>
            input::<c::BCH_IOCTL_DISK_GET_IDX, _>(user, |a| disk_get_idx(fs, a)),

        _ if !fs.flag(c::bch_fs_flags::BCH_FS_started) =>
            fs.throw(E::BCH_ERR_EINVAL_ioctl_not_started),

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

        <c::BCH_IOCTL_QUERY_ACCOUNTING as Ioctl>::OPCODE =>
            arg::<c::BCH_IOCTL_QUERY_ACCOUNTING, _>(user, |a| query_accounting_v1(fs, a)),
        <c::BCH_IOCTL_QUERY_ACCOUNTING_v2 as Ioctl>::OPCODE =>
            arg::<c::BCH_IOCTL_QUERY_ACCOUNTING_v2, _>(user, |a| query_accounting_v2(fs, a)),
        <c::BCH_IOCTL_QUERY_BTREE_KEYS as Ioctl>::OPCODE =>
            inout::<c::BCH_IOCTL_QUERY_BTREE_KEYS, _>(user, |a| query_btree_keys(fs, a)),

        _ => Err(c::ENOTTY.into()),
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

/* The queries */

fn accounting_read_done(fs: &Fs) -> bool {
    fs.flag(c::bch_fs_flags::BCH_FS_new_fs) ||
        fs.recovery.runner.passes_complete.get() &
            (1 << c::bch_recovery_pass::BCH_RECOVERY_PASS_accounting_read as u32) != 0
}

fn query_uuid(fs: &Fs) -> Result<c::bch_ioctl_query_uuid, BchError> {
    Ok(c::bch_ioctl_query_uuid { uuid: c::uuid_t { b: fs.sb.user_uuid.each_ref().map(|b| b.get()) } })
}

fn fs_usage(fs: &Fs, user: UserArg<c::bch_ioctl_fs_usage>) -> Result<(), BchError> {
    if !accounting_read_done(fs) {
        return fs.throw(bch_errcode::BCH_ERR_EINVAL_ioctl_fs_usage_accounting_not_read);
    }
    let entries_bytes = user.read()?.replica_entries_bytes;

    let mut d = c::darray_char::default();
    let ret = ret_to_result_void(unsafe { c::bch2_fs_replicas_usage_read(fs.as_ptr().cast(), &mut d) });
    // SAFETY: C's darray, filled
    let replicas = unsafe { darray_take(&mut d) };
    ret?;
    if (entries_bytes as usize) < replicas.len() {
        return Err(c::ERANGE.into());
    }
    user.bytes_at(offset_of!(c::bch_ioctl_fs_usage, replicas), replicas.len()).writer().write_slice(&replicas)?;

    let u = fs.usage_read_short();
    let mut arg = c::bch_ioctl_fs_usage {
        capacity:              u.capacity,
        used:                  u.used,
        online_reserved:       fs.online_reserved(),
        replica_entries_bytes: replicas.len() as u32,
        ..Default::default()
    };
    for (i, r) in arg.persistent_reserved.iter_mut().enumerate() {
        let pos = c::disk_accounting_pos::from_arm(c::bch_acct_persistent_reserved { nr_replicas: i as u8 });
        accounting::mem_read(fs, &pos, core::slice::from_mut(r));
    }
    user.write(&arg)
}

/// Both versions: @arg's inputs in, its outputs out, the accounting keys to
/// @dst. v1's outputs are a prefix of v2's.
fn query_accounting(fs: &Fs, arg: &mut c::bch_ioctl_query_accounting_v2,
                    dst: impl FnOnce(usize) -> UserSlice) -> Result<(), BchError> {
    use c::disk_accounting_type as T;

    if !accounting_read_done(fs) {
        return fs.throw(bch_errcode::BCH_ERR_EINVAL_ioctl_query_accounting_not_read);
    }

    /*
     * Per-inode and per-snapshot accounting expose per-object usage - other
     * users' file sizes (logical and on-disk, keyed by inode number) and the
     * existence and size of snapshots - so they're admin-only. The remaining
     * types are fs-wide or per-device aggregates and stay unprivileged, so
     * 'bcachefs fs usage' still works for normal users. This ioctl is
     * reachable by any user via the file ioctl path, not just the root-owned
     * control device.
     */
    let privileged = 1 << T::BCH_DISK_ACCOUNTING_inum.0 | 1 << T::BCH_DISK_ACCOUNTING_snapshot.0;
    if arg.accounting_types_mask & privileged != 0 && !capable(Capability::SysAdmin) {
        return fs.throw(bch_errcode::BCH_ERR_EPERM_non_admin);
    }

    /*
     * The stripe fragmentation counters have no upgrade entry: on a
     * filesystem predating them, and until check_allocations has recomputed
     * accounting, zero is indistinguishable from never-computed. The compat
     * bit is what says they mean something, so without it don't return them
     * at all - an absent entry is a reading userspace can act on, a zero
     * would be a lie.
     */
    if fs.sb.compat.get() & 1 << c::bch_sb_compat::BCH_COMPAT_stripe_frag_accounting.0 == 0 {
        arg.accounting_types_mask &= !(1 << T::BCH_DISK_ACCOUNTING_stripe_frag.0 |
                                       1 << T::BCH_DISK_ACCOUNTING_dev_stripe_frag.0);
    }

    let mut d = c::darray_char::default();
    let ret = ret_to_result_void(unsafe {
        c::bch2_fs_accounting_read(fs.as_ptr().cast(), &mut d, arg.accounting_types_mask)
    });
    // SAFETY: C's darray, filled
    let keys = unsafe { darray_take(&mut d) };
    ret?;
    if (arg.accounting_u64s as usize) * size_of::<u64>() < keys.len() {
        return Err(c::ERANGE.into());
    }
    dst(keys.len()).writer().write_slice(&keys)?;

    arg.capacity        = fs.capacity.capacity.wrapping_sub(fs.usage_base().hidden);
    arg.used            = fs.usage_read_short().used;
    arg.online_reserved = fs.online_reserved();
    arg.accounting_u64s = (keys.len() / size_of::<u64>()) as u32;

    const R: usize = c::BCH_REPLICAS_MAX as usize;
    const { assert!(R <= c::BCH_IOCTL_QUERY_ACCOUNTING_FREE_NR as usize) };
    let (mut free, mut free_now) = ([0u64; R], [0u64; R]);
    unsafe { c::bch2_fs_sectors_placeable(fs.as_ptr().cast(), free.as_mut_ptr(), free_now.as_mut_ptr()) };
    arg.free     = Default::default();
    arg.free_now = Default::default();
    arg.free[..R].copy_from_slice(&free);
    arg.free_now[..R].copy_from_slice(&free_now);
    Ok(())
}

fn query_accounting_v1(fs: &Fs, user: UserArg<c::bch_ioctl_query_accounting>) -> Result<(), BchError> {
    let mut arg = user.read()?;
    let mut v2 = c::bch_ioctl_query_accounting_v2 {
        accounting_u64s:       arg.accounting_u64s,
        accounting_types_mask: arg.accounting_types_mask,
        ..Default::default()
    };
    query_accounting(fs, &mut v2,
        |len| user.bytes_at(offset_of!(c::bch_ioctl_query_accounting, accounting), len))?;

    arg.capacity        = v2.capacity;
    arg.used            = v2.used;
    arg.online_reserved = v2.online_reserved;
    arg.accounting_u64s = v2.accounting_u64s;
    user.write(&arg)
}

fn query_accounting_v2(fs: &Fs, user: UserArg<c::bch_ioctl_query_accounting_v2>) -> Result<(), BchError> {
    let mut arg = user.read()?;
    query_accounting(fs, &mut arg,
        |len| user.bytes_at(offset_of!(c::bch_ioctl_query_accounting_v2, accounting), len))?;
    user.write(&arg)
}

/* obsolete, didn't allow for new data types: */
fn dev_usage(fs: &Fs, arg: &mut c::bch_ioctl_dev_usage) -> Result<(), BchError> {
    if arg.flags & !c::BCH_BY_INDEX != 0 || arg.pad[..3] != [0; 3] {
        return fs.throw(bch_errcode::BCH_ERR_EINVAL_ioctl_dev_usage_bad_flags);
    }
    let ca = dev_lookup(fs, arg.dev, arg.flags)?;
    let src = ca.usage_full_read_fast();

    arg.state       = ca.mi.state;
    arg.bucket_size = ca.mi.bucket_size.into();
    arg.nr_buckets  = ca.mi.nbuckets - u64::from(ca.mi.first_bucket);
    for (d, s) in arg.d.iter_mut().zip(&src.d) {
        d.buckets    = s.buckets;
        d.sectors    = s.sectors;
        d.fragmented = s.fragmented;
    }
    Ok(())
}

fn dev_usage_v2(fs: &Fs, user: UserVstruct<c::bch_ioctl_dev_usage_v2>) -> Result<(), BchError> {
    if !accounting_read_done(fs) {
        return fs.throw(bch_errcode::BCH_ERR_EINVAL_ioctl_dev_usage_v2_accounting_not_read);
    }
    let mut arg = user.head()?;
    if arg.flags & !c::BCH_BY_INDEX != 0 || arg.pad[..3] != [0; 3] {
        return fs.throw(bch_errcode::BCH_ERR_EINVAL_ioctl_dev_usage_v2_bad_flags);
    }
    let ca = dev_lookup(fs, arg.dev, arg.flags)?;
    let src = ca.usage_full_read_fast();

    arg.state         = ca.mi.state;
    arg.bucket_size   = ca.mi.bucket_size.into();
    arg.nr_data_types = arg.nr_data_types.min(c::BCH_DATA_NR as u8);
    arg.nr_buckets    = ca.mi.nbuckets - u64::from(ca.mi.first_bucket);
    user.set_head(&arg)?;

    let d = user.tail(&arg);
    for i in 0..d.len() {
        d.set(i, &c::bch_ioctl_dev_usage_type {
            buckets:    src.d[i].buckets,
            sectors:    src.d[i].sectors,
            fragmented: src.d[i].fragmented,
        })?;
    }
    Ok(())
}

fn read_super(fs: &Fs, arg: c::bch_ioctl_read_super) -> Result<(), BchError> {
    admin(fs)?;
    if arg.flags & !(c::BCH_BY_INDEX | c::BCH_READ_DEV) != 0 || arg.pad != 0 {
        return fs.throw(bch_errcode::BCH_ERR_EINVAL_ioctl_read_super_bad_flags);
    }

    let sb_lock = fs.sb_lock();
    let ca;
    let sb = if arg.flags & c::BCH_READ_DEV != 0 {
        ca = dev_lookup(fs, arg.dev, arg.flags)?;
        match fs.dev_sb(&sb_lock, &ca) {
            Some(sb) => sb,
            None     => return fs.throw(bch_errcode::BCH_ERR_device_offline),
        }
    } else {
        &**sb_lock
    };

    let bytes = sb.sb_bytes();
    if bytes.len() as u64 > arg.size {
        return Err(c::ERANGE.into());
    }
    UserSlice::new(UserPtr::from_addr(arg.sb as usize), bytes.len()).writer().write_slice(bytes)?;
    Ok(())
}

/// The dev_t userspace's encoding @dev names, as the kernel's
/// huge_decode_dev(): new_decode_dev()'s layout, major in bits 8-19, minor
/// in 0-7 and 20-31.
fn huge_decode_dev(dev: u64) -> u32 {
    let dev = dev as u32;
    let major = (dev & 0xfff00) >> 8;
    let minor = (dev & 0xff) | ((dev >> 12) & 0xfff00);
    major << c::MINORBITS | minor
}

fn disk_get_idx(fs: &Fs, arg: c::bch_ioctl_disk_get_idx) -> Result<c_int, BchError> {
    let dev = huge_decode_dev(arg.dev);
    admin(fs)?;
    if dev == 0 {
        return fs.throw(bch_errcode::BCH_ERR_EINVAL_ioctl_disk_get_idx_bad_dev);
    }

    let rcu = read_lock();
    let idx = fs.online_devs_rcu(&rcu).find(|ca| ca.dev == dev).map(|ca| ca.dev_idx);
    match idx {
        Some(idx) => Ok(idx.into()),
        None      => fs.throw(bch_errcode::BCH_ERR_ENOENT_dev_idx_not_found),
    }
}

fn query_btree_keys(fs: &Fs, arg: &mut c::bch_ioctl_query_btree_keys) -> Result<(), BchError> {
    const FLAGS: [(u32, BtreeIterFlags); 4] = [
        (c::BCH_IOCTL_QUERY_BTREE_KEYS_slots,              BtreeIterFlags::SLOTS),
        (c::BCH_IOCTL_QUERY_BTREE_KEYS_prev,               BtreeIterFlags::PREV),
        (c::BCH_IOCTL_QUERY_BTREE_KEYS_all_snapshots,      BtreeIterFlags::ALL_SNAPSHOTS),
        (c::BCH_IOCTL_QUERY_BTREE_KEYS_nofilter_whiteouts, BtreeIterFlags::NOFILTER_WHITEOUTS),
    ];

    /*
     * Exposes all filesystem metadata - dirents, inodes, xattrs - and is
     * reachable by any user via the file ioctl path, so admin only:
     */
    admin(fs)?;

    if arg.flags & !FLAGS.iter().fold(0, |m, (f, _)| m | f) != 0 {
        return fs.throw(bch_errcode::BCH_ERR_EINVAL_ioctl_query_btree_keys_bad_flags);
    }
    let mut flags = FLAGS.iter()
        .filter(|(f, _)| arg.flags & f != 0)
        .fold(BtreeIterFlags::empty(), |m, (_, i)| m | *i);

    /* Interior nodes aren't extents - only meaningful at level 0: */
    if arg.level != 0 {
        flags |= BtreeIterFlags::NOT_EXTENTS;
    }

    // an id past the last is what bch2_btree_iter_params_valid() refuses first
    let valid = c::btree_id::from_raw(arg.btree).is_some_and(|btree| unsafe {
        c::bch2_btree_iter_params_valid(btree, arg.level, arg.start, arg.end,
            c::btree_iter_update_trigger_flags::from_bits_retain(flags.bits()))
    });
    if !valid {
        return fs.throw(bch_errcode::BCH_ERR_EINVAL_ioctl_query_btree_keys_bad_params);
    }

    /* bound per-call kernel allocation; the caller loops anyways */
    let buf_size = arg.buf_size.min(1 << 20) as usize;

    let mut out = KVVec::<u8>::new();
    let full = {
        let trans = btree_trans!(fs);
        let mut iter = BtreeIter::new_level(&*trans, arg.btree, arg.start, arg.level, flags);
        iter.for_each_max(&trans, arg.end, |_, iter, k| {
            /* if the buffer fills before this key, resume at it: */
            arg.start = iter.pos();

            let bytes = k.k.u64s as usize * size_of::<u64>();
            if out.len() + bytes > buf_size {
                /* buffer full - or, if empty, can't fit even one key: */
                return if out.is_empty() { Err(c::ERANGE.into()) } else { Ok(ControlFlow::Break(true)) };
            }
            out.extend_from_slice(plain_bytes(k.k), GFP_KERNEL)?;
            out.extend_from_slice(k.val_bytes(), GFP_KERNEL)?;
            Ok(ControlFlow::Continue(()))
        })?
    };

    arg.done = (!full).into();
    arg.used = out.len() as u32;
    UserSlice::new(UserPtr::from_addr(arg.buf as usize), out.len()).writer().write_slice(&out)?;
    Ok(())
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
