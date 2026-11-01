// SPDX-License-Identifier: GPL-2.0

//! Fs - struct bch_fs - its accessors and locks, and OpenFs, the handle that
//! exits a filesystem when dropped.

#![allow(non_camel_case_types)]

use crate::c;
use crate::btree::bkey::BkeyI;
use crate::errcode::{bch_errcode, errptr_to_result, ret_to_result_void as ret_to_result, BchError};
use crate::alloc::capacity::DiskReservation;
use crate::alloc::replicas::can_write_with_devs;
use crate::btree::iter::{BtreeIterFlags, CommitOpts, UpdateTriggerFlags};
use crate::init::dev::dev_in_fs;
use crate::locking::{Guard, MutexNoio, Noio, PercpuRwsemNoio, ReadGuard};
use crate::prt;
use crate::sb::io::{read_super, Holder};
use crate::sb::members::dev_to_text_short;
use crate::util::Printbuf;
use crate::util::alloc::{flags::GFP_KERNEL, Arc, KVVec};
use crate::util::backref::BackRef;
use crate::util::enumerated_ref::EnumeratedRef;
use crate::util::log::{log_msg_start, print_lines, print_str, LogLevel, LogMsg};
use crate::util::rcu::read_lock;
use crate::util::refcount::RefGuard;
use crate::util::sleep::wake_up;
use crate::util::workqueue::Queue;
use core::ffi::CStr;
use core::future::Future;
use core::ptr::NonNull;
use cstruct_macros::c_export;
#[cfg(kernel)]
use crate::util::refcount::{Refcount, TryRefcount};
pub use crate::sb::members::DevRef;
use core::ffi::c_int;
use cstruct_macros::c_extern;
use crate::format::{Bpos, Btree};

c_extern! {
    pub fn bch2_fs_alloc(sb: *mut crate::util::vstructs::Head<c::bch_sb>, opts: *mut c::bch_opts,
                         sbs: *mut c::bch_sb_handles, holder: *const core::ffi::c_void,
                         user_key: *const c::bch_key,
                         out: *mut c::printbuf) -> *mut c::bch_fs;
    pub fn bch2_fs_init_rw(c: *mut c::bch_fs) -> c_int;
    pub fn bch2_fs_emergency_read_only(c: *mut c::bch_fs, out: *mut c::printbuf) -> bool;
    pub fn bch2_fs_read_only(c: *mut c::bch_fs);
    pub fn bch2_fs_read_write(c: *mut c::bch_fs) -> c_int;
    pub fn bch2_fs_stop(c: *mut c::bch_fs) -> c_int;
    pub fn bch2_fs_exit(c: *mut c::bch_fs) -> c_int;
    pub static bch2_fs_flag_strs: [*const crate::util::ffi::c_char; 0];
    pub static bch2_write_refs: [*const crate::util::ffi::c_char; 0];
    pub static bch2_dev_read_refs: [*const crate::util::ffi::c_char; 0];
    pub static bch2_dev_write_refs: [*const crate::util::ffi::c_char; 0];
}

/// sb_lock, held: the superblock, and c->sb, which it publishes
pub type SbLockGuard<'a> = Guard<'a, Noio<c::mutex>, c::bch_fs_sb, c::bch_sb_cpu>;

impl SbLockGuard<'_> {
    pub fn fs_member_mut(&mut self, dev_idx: u32) -> &mut c::bch_member {
        self.member_mut(dev_idx)
            .expect("a member, whole: bch2_sb_members_v2_init() grew them at open")
    }
}

pub struct StateLockRead<'a> {
    fs:     &'a Fs,
    _guard: ReadGuard<'a, c::rw_semaphore>,
}

pub struct StateLockWrite<'a> {
    fs:     &'a Fs,
    _guard: Guard<'a, c::rw_semaphore>,
}

impl<'a> StateLockWrite<'a> {
    pub(crate) fn fs(&self) -> &'a Fs {
        self.fs
    }
}

impl StateLockRead<'_> {
    pub fn recalc_capacity(&self) {
        crate::alloc::capacity::recalc_capacity(self.fs)
    }
}

pub use crate::cstructs::types::Fs;

impl Fs {
    pub fn as_ptr(&self) -> *mut Fs {
        self as *const Fs as *mut Fs
    }
}

// SAFETY: as C shares a bch_fs: every field is synchronized by its own lock,
// atomic or RCU
unsafe impl Sync for Fs {}

impl Fs {
    pub fn name(&self) -> &core::ffi::CStr {
        unsafe { core::ffi::CStr::from_ptr((*self.as_ptr()).name.as_ptr().cast()) }
    }

    pub fn opts(&self) -> &c::PublishedOpts {
        &self.opts
    }

    pub fn opts_writer<'a>(&'a self, sb: &'a mut SbLockGuard<'_>) -> &'a c::PublishedOptsWriter {
        assert!(core::ptr::eq(sb.lock(), &self.sb), "opts_writer(): another filesystem's sb_lock");
        // SAFETY: @sb holds sb_lock, and is borrowed mutably while this is
        unsafe { self.opts.published_writer() }
    }

    /// log2 of the block size, in 512-byte sectors
    pub fn block_bits(&self) -> u32 {
        self.block_bits as u32
    }

    pub fn inode_shard_idx(&self) -> u64 {
        unsafe { c::rust_bch2_inode_shard_idx(self.as_ptr()) }
    }

    /// Never unset once set
    pub fn encryption_key(&self) -> Option<&c::bch_key> {
        self.sb.encryption_key.get()
    }

    /// # Safety
    /// @raw points to a live bch_fs, that outlives 'a.
    pub unsafe fn borrow_raw<'a>(raw: *mut c::bch_fs) -> &'a Fs {
        unsafe { &*raw }
    }

    pub fn from_c(c: &crate::util::ffi::Opaque<c::bch_fs>) -> &Fs {
        unsafe { Self::borrow_raw(c.as_ptr()) }
    }

    /// # Safety
    /// @sb is a live filesystem's disk_sb, and the caller holds its sb_lock
    /// for 'a.
    pub unsafe fn from_disk_sb<'a>(sb: *const c::bch_fs_sb) -> (&'a Fs, SbLockGuard<'a>) {
        let off = core::mem::offset_of!(Fs, sb) + MutexNoio::<c::bch_fs_sb, c::bch_sb_cpu>::PROTECTED_OFFSET;
        // SAFETY: the caller's
        unsafe {
            let fs: &'a Fs = Self::borrow_raw(sb.byte_sub(off).cast_mut().cast());
            (fs, fs.sb_lock_held())
        }
    }

    pub fn version(&self) -> c::bcachefs_metadata_version {
        c::bcachefs_metadata_version(self.sb.version.get() as u32)
    }

    /// Below version() while an upgrade's migrations are still running
    pub fn version_upgrade_complete(&self) -> c::bcachefs_metadata_version {
        c::bcachefs_metadata_version(self.sb.version_upgrade_complete.get() as u32)
    }

    /// Topology repair dropped nodes from btree @id
    pub fn btree_lost_data(&self, id: Btree) -> bool {
        self.sb.btrees_lost_data.get() & (1u64 << id.0) != 0
    }

    /// An image from `bcachefs dump --sanitize`, which scrubs dirent names
    /// without rehashing them
    pub fn dirents_sanitized(&self) -> bool {
        self.sb.dirents_sanitized.get()
    }

    pub fn flag(&self, f: c::bch_fs_flags) -> bool {
        self.flags().load(core::sync::atomic::Ordering::Relaxed) & (1usize << f as u32) != 0
    }

    pub fn set_flag(&self, f: c::bch_fs_flags) {
        self.flags().fetch_or(1usize << f as u32, core::sync::atomic::Ordering::Relaxed);
    }

    pub fn clear_flag(&self, f: c::bch_fs_flags) {
        self.flags().fetch_and(!(1usize << f as u32), core::sync::atomic::Ordering::Relaxed);
    }

    fn flags(&self) -> &core::sync::atomic::AtomicUsize {
        // SAFETY: an unsigned long, which C's bitops change atomically
        unsafe { &*(core::ptr::addr_of!((*self.as_ptr()).flags) as *const core::sync::atomic::AtomicUsize) }
    }

    /// A mount is reading recovery progress from BCH_IOCTL_RECOVERY_STATUS,
    /// so progress indicators needn't print it
    pub fn stdio_progress_reader(&self) -> bool {
        let r = unsafe {
            &*(core::ptr::addr_of!((*self.as_ptr()).stdio_progress_reader)
               as *const core::sync::atomic::AtomicBool)
        };
        r.load(core::sync::atomic::Ordering::Relaxed)
    }

    pub fn journal_cur_seq(&self) -> u64 {
        self.journal.seq.load(core::sync::atomic::Ordering::Relaxed) as u64
    }

    /// True if this call is the one that went read-only - then @out is
    /// printed, not suppressed
    pub fn emergency_read_only(&self, out: &mut Printbuf) -> bool {
        unsafe { c::bch2_fs_emergency_read_only(self.as_ptr(), out.as_raw()) }
    }

    pub fn sb_lock(&self) -> SbLockGuard<'_> {
        self.sb.lock()
    }

    pub fn holder(&self) -> &Arc<Holder> {
        self.holder.as_ref().expect("a filesystem's holder, from bch2_fs_alloc()")
    }

    /// # Safety
    /// The caller holds sb_lock, for as long as the guard lives.
    pub unsafe fn sb_lock_held(&self) -> SbLockGuard<'_> {
        unsafe { self.sb.held() }
    }

    pub fn mark_lock(&self) -> &PercpuRwsemNoio {
        &self.capacity.mark_lock
    }

    pub fn mark_lock_assert_held(&self) {
        self.mark_lock().assert_held_any()
    }

    pub fn flags_to_text(&self, out: &mut Printbuf) {
        unsafe { out.prt_bitflags(c::bch2_fs_flag_strs.as_ptr(), self.flags().load(core::sync::atomic::Ordering::Relaxed) as u64) }
    }

    /// Devices and capacity don't change under it
    pub fn state_lock_read(&self) -> StateLockRead<'_> {
        StateLockRead { fs: self, _guard: self.state_lock.read() }
    }

    pub fn state_lock_write(&self) -> StateLockWrite<'_> {
        StateLockWrite { fs: self, _guard: self.state_lock.write() }
    }

    #[track_caller]
    pub fn write_super(&self, sb: &mut SbLockGuard<'_>) {
        let _ = self.write_super_ret(sb);
    }

    /// For a write only for a replicas entry: it may leave the slow disks behind
    #[track_caller]
    pub fn write_super_replicas(&self, sb: &mut SbLockGuard<'_>) {
        let _ = self.write_super_replicas_ret(sb);
    }

    /// Counted and traced, as bch_err_throw(); BchError::from() names an error
    /// without throwing it. Cold, as C's unlikely(ret).
    #[cold]
    pub fn err(&self, error: impl Into<BchError>) -> BchError {
        BchError::from_raw(-unsafe { c::__bch2_err_throw(self.as_ptr(), -error.into().raw()) })
    }

    pub fn throw<T>(&self, error: bch_errcode) -> Result<T, BchError> {
        Err(self.err(error))
    }

    pub fn require<T>(&self, value: Option<T>, error: bch_errcode) -> Result<T, BchError> {
        match value {
            Some(v) => Ok(v),
            None    => self.throw(error),
        }
    }

    pub fn ensure(&self, condition: bool, error: bch_errcode) -> Result<(), BchError> {
        if condition {
            Ok(())
        } else {
            self.throw(error)
        }
    }

    pub fn btree_id_root(&self, id: u32) -> Option<&c::BtreeNode> {
        self.btree.cache.roots.node(id)
    }

    pub fn btree_id_nr_alive(&self) -> u32 {
        u32::from(Btree::nr) + self.btree.cache.roots.roots_nr_extra.get()
    }

    pub fn nr_devices(&self) -> u32 {
        self.sb.nr_devices.get() as u32
    }

    /// Everything committed before the call is on disk when it returns
    pub fn journal_flush(&self) -> Result<(), BchError> {
        ret_to_result(unsafe { c::bch2_journal_flush(&mut (*self.as_ptr()).journal) })
    }

    /// A logged op, resumed after a crash. Block granular: zeroing the rest of
    /// the last block is the caller's, as the VFS does in the page cache.
    pub fn truncate(&self, inum: c::subvol_inum, new_i_size: u64) -> Result<(), BchError> {
        crate::data::io_misc::truncate(self, inum, new_i_size, &mut 0)
    }

    pub fn request_incompat_feature(&self, version: c::bcachefs_metadata_version)
        -> Result<(), BchError>
    {
        if version.0 <= self.sb.version_incompat.get().into() {
            return Ok(());
        }
        self.set_version_incompat(version)
    }

    pub fn feature(&self, feature: c::bch_sb_feature) -> bool {
        self.sb.features.get() & (1u64 << feature.0) != 0
    }

    pub fn check_set_feature(&self, feature: c::bch_sb_feature) {
        if !self.feature(feature) {
            self.set_feature(feature)
        }
    }

    pub fn casefold_enabled(&self) -> Result<(), BchError> {
        if !cfg!(CONFIG_UNICODE) {
            return self.throw(bch_errcode::BCH_ERR_no_casefolding_without_utf8);
        }
        if self.opts().casefold_disabled.get() != 0 {
            return self.throw(bch_errcode::BCH_ERR_casefolding_disabled);
        }
        Ok(())
    }

    #[track_caller]
    pub fn write_super_ret(&self, sb: &mut SbLockGuard<'_>) -> Result<(), BchError> {
        self.write_super_flags(sb, c::bch_sb_write_flags::empty())
    }

    /// Allowed before start, for a nostart open that starts later
    #[track_caller]
    pub fn write_super_bringup(&self, sb: &mut SbLockGuard<'_>) -> Result<(), BchError> {
        self.write_super_flags(sb, c::bch_sb_write_flags::BCH_SB_WRITE_bringup)
    }

    /// c->sb and member info updated from the superblock, without writing it
    pub fn sb_update(&self, sb: &mut SbLockGuard<'_>) {
        crate::sb::io::sb_update(self, sb)
    }

    pub fn read_only(&self) {
        unsafe { c::bch2_fs_read_only(self.as_ptr()) };
    }

    pub fn read_write(&self) -> Result<(), BchError> {
        ret_to_result(unsafe { c::bch2_fs_read_write(self.as_ptr()) })
    }

    pub fn stop(&self) {
        unsafe { c::bch2_fs_stop(self.as_ptr()) };
    }

    pub fn journal_flush_all_pins(&self) {
        unsafe {
            c::bch2_journal_flush_pins(
                &mut (*self.as_ptr()).journal,
                u64::MAX,
            );
        }
    }

    pub fn journal_flush_outstanding_pins(&self) -> bool {
        unsafe { c::bch2_journal_flush_pins(&mut (*self.as_ptr()).journal, self.journal_cur_seq()) }
    }

    pub fn btree_delete_range(
        &self,
        btree_id:     Btree,
        start:        Bpos,
        end:          Bpos,
        iter_flags:   BtreeIterFlags,
        update_flags: UpdateTriggerFlags,
    ) -> Result<(), BchError> {
        let flags = c::btree_iter_update_trigger_flags::from_bits_retain(iter_flags.bits() | update_flags.bits());
        ret_to_result(unsafe { c::bch2_btree_delete_range(self.as_ptr(), btree_id, start, end, flags) })
    }

    pub fn btree_insert<'k>(
        &self,
        btree_id:     Btree,
        key:          impl Into<&'k mut BkeyI>,
        disk_res:     Option<&DiskReservation<'_>>,
        commit_flags: impl Into<CommitOpts>,
        iter_flags:   BtreeIterFlags,
    ) -> Result<(), BchError> {
        self.btree_insert_k_i(btree_id, key.into(), disk_res, commit_flags.into(), iter_flags)
    }

    /// Not monomorphized per key type
    fn btree_insert_k_i(
        &self,
        btree_id:     Btree,
        key:          &mut BkeyI,
        disk_res:     Option<&DiskReservation<'_>>,
        commit_flags: CommitOpts,
        iter_flags:   BtreeIterFlags,
    ) -> Result<(), BchError> {
        let disk_res = disk_res
            .map(|r| r.as_mut_ptr())
            .unwrap_or(core::ptr::null_mut());

        ret_to_result(unsafe {
            c::bch2_btree_insert(
                self.as_ptr(),
                btree_id,
                key.as_ffi_mut(),
                disk_res,
                commit_flags.to_c(),
                c::btree_iter_update_trigger_flags::from_bits_retain(iter_flags.bits()),
            )
        })
    }

    /// # Safety
    /// The device exists.
    pub unsafe fn dev_raw(&self, dev: u32) -> *mut c::bch_dev {
        self.devs[dev as usize].as_ptr()
    }

    pub fn block_bytes(&self) -> u64 {
        self.opts().block_size.get() as u64
    }

    pub fn btree_sectors(&self) -> u32 {
        self.opts().btree_node_size.get() >> 9
    }

    pub fn time_to_timespec(&self, time: i64) -> c::timespec64 {
        const NSEC_PER_SEC: i64 = 1_000_000_000;

        let sb = &self.sb;
        let time = time.wrapping_add(sb.time_base_lo.get() as i64);
        let units = sb.time_units_per_sec.get() as i64;

        let mut sec  = time / units;
        let mut nsec = (time % units) * sb.nsec_per_time_unit.get() as i64;

        // set_normalized_timespec64()
        while nsec >= NSEC_PER_SEC {
            nsec -= NSEC_PER_SEC;
            sec += 1;
        }
        while nsec < 0 {
            nsec += NSEC_PER_SEC;
            sec -= 1;
        }

        let mut t: c::timespec64 = pin_init::Zeroable::zeroed();
        t.tv_sec  = sec as _;
        t.tv_nsec = nsec as _;
        t
    }

    // tv_sec is the kernel's time64_t, or userspace's time_t - 32 bits on some
    // targets
    #[allow(clippy::unnecessary_cast)]
    pub fn timespec_to_time(&self, ts: c::timespec64) -> i64 {
        let sb = &self.sb;
        // C's (int) tv_nsec / nsec_per_time_unit: an unsigned division
        (ts.tv_sec as i64).wrapping_mul(sb.time_units_per_sec.get() as i64)
            .wrapping_add((ts.tv_nsec as i32 as u32 / sb.nsec_per_time_unit.get()) as i64)
            .wrapping_sub(sb.time_base_lo.get() as i64)
    }

    /// In the filesystem's time units.
    pub fn current_time(&self) -> u64 {
        let mut now: c::timespec64 = pin_init::Zeroable::zeroed();
        unsafe { c::ktime_get_coarse_real_ts64(&mut now) };
        self.timespec_to_time(now) as u64
    }

    pub fn set_loglevel(&self, level: u32) {
        unsafe { (*self.as_ptr()).loglevel = level; }
    }
}

/// Rust fields initialized, the rest zeroed, for bch2_fs_init(), taking the
/// ref on @holder. NULL if out of memory.
///
/// # Safety
/// @holder is a ref on its devices' Holder, from Arc::into_raw()
#[cstruct_macros::c_export]
pub unsafe fn bch2_fs_alloc_mem(holder: *const core::ffi::c_void) -> *mut c::bch_fs {
    let holder = unsafe { Arc::from_raw(holder.cast::<Holder>()) };
    let Ok(c) = crate::util::alloc::alloc_pinned(Fs::init()) else { return core::ptr::null_mut() };
    unsafe { *(*c).holder = Some(holder) };
    c
}

/// The end of teardown: C's has emptied the C fields, so their Drop finds
/// nothing.
///
/// # Safety
/// @c is from bch2_fs_alloc_mem(), and nothing uses it after.
#[cstruct_macros::c_export]
pub unsafe fn bch2_fs_free_mem(c: *mut c::bch_fs) {
    unsafe { crate::util::alloc::free_pinned(c) }
}

/// Exits the filesystem when dropped
pub struct OpenFs(core::ptr::NonNull<Fs>);

// SAFETY: offline fsck opens it in the ioctl and exits it in its kthread
unsafe impl Send for OpenFs {}

impl OpenFs {
    /// # Safety
    /// @fs is open, and this takes over its ref.
    pub unsafe fn from_raw(fs: core::ptr::NonNull<Fs>) -> Self {
        OpenFs(fs)
    }

    /// @key unwraps the superblock's key, instead of the keyring
    pub fn open<'a>(devs: impl ExactSizeIterator<Item = &'a CStr>, opts: &mut c::bch_opts,
                    key: Option<&c::bch_key>) -> Result<OpenFs, BchError> {
        Self::claim(devs, opts, &Holder::new()?)?.open(opts, key)
    }

    /// The open's first half: @devs claimed for @holder, and read
    pub fn claim<'a>(devs: impl ExactSizeIterator<Item = &'a CStr>, opts: &mut c::bch_opts,
                     holder: &Arc<Holder>) -> Result<Claimed, BchError> {
        let mut out = Printbuf::new();
        out.indent_add_nextline(2);

        match Self::claim_to(devs, opts, holder, &mut out) {
            Ok(sbs) => Ok(Claimed { sbs, holder: holder.clone(), out }),
            Err(e) => Err(open_failed(e, &mut out)),
        }
    }

    fn claim_to<'a>(devs: impl ExactSizeIterator<Item = &'a CStr>, opts: &mut c::bch_opts,
                    holder: &Arc<Holder>, out: &mut Printbuf) -> Result<KVVec<c::bch_sb_handle>, BchError> {
        if devs.len() == 0 {
            return Err(c::EINVAL.into());
        }

        let mut sbs = KVVec::with_capacity(devs.len(), GFP_KERNEL)?;
        for path in devs {
            sbs.push(read_super(path, opts, Some(holder))?, GFP_KERNEL)?;
        }
        filter_dead(&mut sbs, opts, Some(out))?;
        Ok(sbs)
    }

    pub fn into_raw(self) -> core::ptr::NonNull<Fs> {
        core::mem::ManuallyDrop::new(self).0
    }

    /// Drop discards the error
    pub fn exit(self) -> i32 {
        let ret = unsafe { c::bch2_fs_exit(self.0.as_ptr()) };
        core::mem::forget(self);
        ret
    }
}

fn open_failed(e: BchError, out: &mut Printbuf) -> BchError {
    prt!(out, "error starting filesystem: {}", e.msg());
    print_lines(&LogLevel::ERR.prefix(), out.as_bytes());
    e
}

/// An open's first half: its devices claimed and read, the dead ones dropped
pub struct Claimed {
    sbs:    KVVec<c::bch_sb_handle>,
    holder: Arc<Holder>,
    out:    Printbuf,
}

impl Claimed {
    /// A multi-device filesystem's UUID
    pub fn uuid(&self) -> Option<[u8; 16]> {
        let sb = self.sbs[0].io.buf.sb();
        sb.multi_device().then_some(sb.uuid.b)
    }

    /// @key unwraps the superblock's key, instead of the keyring
    pub fn open(mut self, opts: &mut c::bch_opts, key: Option<&c::bch_key>) -> Result<OpenFs, BchError> {
        match self.open_to(opts, key) {
            Err(e) => Err(open_failed(e, &mut self.out)),
            Ok(fs) => {
                if !self.out.as_bytes().is_empty() {
                    LogMsg::with_level(&fs, LogLevel::INFO).write_bytes(self.out.as_bytes());
                }
                Ok(fs)
            }
        }
    }

    fn open_to(&mut self, opts: &mut c::bch_opts, key: Option<&c::bch_key>) -> Result<OpenFs, BchError> {
        let out = &mut self.out;
        let sbs = &mut self.sbs;

        // bch2_fs_alloc() takes the handles it attaches, leaving them zeroed
        let best = sbs[0].io.buf.sb;
        let mut lent = c::bch_sb_handles { nr: sbs.len(), size: sbs.capacity(), data: sbs.as_mut_ptr(),
                                           preallocated: [] };
        let key = key.map_or(core::ptr::null(), |k| k);
        // SAFETY: the handles stay sbs', which frees them; the holder ref is
        // the filesystem's
        let fs = unsafe {
            c::bch2_fs_alloc(best, opts, &mut lent, Arc::into_raw(self.holder.clone()).cast(), key,
                             out.as_raw())
        };
        let fs = errptr_to_result(NonNull::new(fs).expect("bch2_fs_alloc(): a filesystem or an error"))?;
        // SAFETY: bch2_fs_alloc()'s, ours to exit
        let fs = unsafe { OpenFs::from_raw(fs.cast()) };
        self.holder.fs.store(fs.as_ptr(), core::sync::atomic::Ordering::Release);

        // opt_version_init()'s messages, before startup's
        if !out.as_bytes().is_empty() {
            print_str(&fs, LogLevel::INFO, out);
            out.reset();
        }

        if fs.opts().nostart.get() == 0 {
            fs.start_to(out)?;
        }
        Ok(fs)
    }
}

impl core::ops::Deref for OpenFs {
    type Target = Fs;

    fn deref(&self) -> &Fs {
        unsafe { self.0.as_ref() }
    }
}

impl Drop for OpenFs {
    fn drop(&mut self) {
        unsafe { c::bch2_fs_exit(self.0.as_ptr()); }
    }
}

/// Which is the filesystem's superblock: the newest - highest seq, then write
/// time - and of those, the first, as C took it
pub fn newest<'a>(sbs: impl IntoIterator<Item = &'a c::bch_sb>) -> Option<usize> {
    sbs.into_iter()
        .map(|sb| core::cmp::Reverse((sb.seq.get(), sb.write_time.get())))
        .enumerate()
        .min_by_key(|&(_, age)| age)
        .map(|(i, _)| i)
}

/// Newest first, without those removed from the filesystem or split from it
pub fn filter_dead(sbs: &mut KVVec<c::bch_sb_handle>, opts: &c::bch_opts,
                   mut out: Option<&mut Printbuf>) -> Result<(), BchError> {
    let Some(best) = newest(sbs.iter().map(|h| h.io.buf.sb())) else {
        return Ok(());
    };

    let mut dead = KVVec::from_elem(false, sbs.len(), GFP_KERNEL)?;
    for i in (0..sbs.len()).rev() {
        let name = sbs[i].dev.name().to_str().unwrap_or("?");
        match dev_in_fs(sbs[best].io.buf.sb(), Some(sbs[best].dev.bdev()), &sbs[i], opts) {
            Ok(()) => {}
            Err(e) if e.matches(bch_errcode::BCH_ERR_device_has_been_removed) ||
                      e.matches(bch_errcode::BCH_ERR_device_splitbrain) => {
                if let Some(out) = out.as_deref_mut() {
                    prt!(out, "Not using device {name}: {}\n", e.msg());
                }
                dead[i] = true;
            }
            Err(e) => {
                if let Some(out) = out.as_deref_mut() {
                    prt!(out, "Cannot mount with device {name}: {}\n", e.msg());
                }
                return Err(e);
            }
        }
    }

    let best = best - dead[..best].iter().filter(|&&d| d).count();
    let mut i = 0;
    sbs.retain(|_| {
        i += 1;
        !dead[i - 1]
    });
    sbs.swap(0, best);
    Ok(())
}

impl Fs {
    pub fn start(&self) -> Result<(), BchError> {
        let mut err = Printbuf::new();
        log_msg_start(&mut err, self.name());
        let prefix = err.as_bytes().len();

        let ret = self.start_to(&mut err);
        if let Err(e) = &ret {
            prt!(&mut err, "error starting filesystem: {}", e.msg());
        }
        if err.as_bytes().len() != prefix {
            print_str(self, LogLevel::ERR, &mut err);
        }
        ret
    }

    fn start_to(&self, err: &mut Printbuf) -> Result<(), BchError> {
        let ret = self.bring_up(err);
        unsafe { (*self.as_ptr()).recovery_task = core::ptr::null_mut() };
        ret
    }

    fn bring_up(&self, err: &mut Printbuf) -> Result<(), BchError> {
        use c::bch_fs_flags as Flag;
        assert!(!self.flag(Flag::BCH_FS_started));

        // the open skipped what a start needs: opt_version_init()
        if self.opts().will_not_start.get() != 0 {
            return self.throw(bch_errcode::BCH_ERR_EINVAL_will_not_start);
        }
        // superblock writes from here on are part of bringing it up
        self.set_flag(Flag::BCH_FS_start_begun);

        {
            let state = self.state_lock_write();
            let rcu = read_lock();
            for ca in self.online_devs_rcu(&rcu) {
                if u32::from(ca.mi.get().state) == c::bch_member_state::BCH_MEMBER_STATE_rw.0 {
                    state.dev_allocator_add(ca);
                }
            }
            drop(rcu);
            crate::alloc::capacity::recalc_capacity(self);
        }

        self.may_start(err)?;

        ret_to_result(crate::data::reconcile::work::bch2_fs_reconcile_init(self))?;
        crate::sb::counters::RecentCounters::queue(self);

        // now, so it's persisted before user-data accounting fires: a fresh
        // filesystem shut down before then would never record it, and its next
        // mount would find replicas missing from the superblock
        let _ = self.request_incompat_feature(c::bcachefs_metadata_version::no_sb_user_data_replicas);

        // allocated now if it may be needed: a mount failing on
        // kthread_create() is very annoying
        if crate::init::passes::go_rw_in_recovery(self) {
            ret_to_result(unsafe { c::bch2_fs_init_rw(self.as_ptr()) })?;
        }

        // some options can only be checked once started
        crate::opts::opts_hooks_pre_set(self)?;

        let initialized = self.sb_lock().sb().initialized();
        ret_to_result(unsafe {
            if initialized { c::bch2_fs_recovery(self.as_ptr()) } else { c::bch2_fs_initialize(self.as_ptr()) }
        })?;

        crate::opts::opts_hooks_pre_set(self)?;

        self.set_flag(Flag::BCH_FS_started);
        wake_up(&self.ro_ref_wait);
        self.ro_ref_events.wake();

        let _state = self.state_lock_write();
        if self.opts().read_only.get() != 0 {
            self.read_only();
        } else if !self.flag(Flag::BCH_FS_rw) {
            self.read_write()?;
        }
        Ok(())
    }

    fn may_start(&self, err: &mut Printbuf) -> Result<(), BchError> {
        use c::DegradedAction as Degraded;

        if self.opts().no_version_check.get() != 0 {
            prt!(err, "Cannot start with opts.no_version_check\n");
            return self.throw(bch_errcode::BCH_ERR_EINVAL_no_version_check_start);
        }

        let degraded = self.opts().degraded();
        let flags = if degraded == Degraded::very {
            c::BCH_FORCE_IF_DEGRADED | c::BCH_FORCE_IF_LOST
        } else if degraded == Degraded::yes {
            c::BCH_FORCE_IF_DEGRADED
        } else {
            let mut missing = false;
            for ca in self.members() {
                if !ca.is_online() &&
                   (u32::from(ca.mi.get().state) != c::bch_member_state::BCH_MEMBER_STATE_evacuating.0 ||
                    ca.has_data() != 0) {
                    prt!(err, "Cannot mount without device {}\n", ca.dev_idx);
                    err.indent_add(2);
                    dev_to_text_short(err, self.sb_lock().sb(), ca.dev_idx.into());
                    err.indent_sub(2);
                    missing = true;
                }
            }
            if !missing {
                return Ok(());
            }

            // Which refusal this is decides what there is to consent to -
            // mount.bcachefs asks whoever is at the machine - so it's the
            // error code, not prose. degraded=yes starts while every replica
            // set has a readable copy; past that, only degraded=very.
            let online = c::bch_devs_mask { d: self.devs_online.load() };
            return self.throw(if self.can_read_fs_with_devs(&online, c::BCH_FORCE_IF_DEGRADED, Some(err)) {
                bch_errcode::BCH_ERR_insufficient_devices_data_intact
            } else {
                bch_errcode::BCH_ERR_insufficient_devices_data_lost
            });
        };

        let online = c::bch_devs_mask { d: self.devs_online.load() };
        let rw = c::bch_devs_mask { d: self.allocator.rw_devs[0].load() };
        if !self.can_read_fs_with_devs(&online, flags, Some(&mut *err)) ||
           (self.opts().read_only.get() == 0 &&
            !can_write_with_devs(self, &rw, c::bch_write_check::BCH_WRITE_CHECK_start, flags, err)) {
            self.missing_devs_to_text(err);
            return self.throw(bch_errcode::BCH_ERR_insufficient_devices_to_start);
        }
        Ok(())
    }

    #[cold]
    pub fn missing_devs_to_text(&self, out: &mut Printbuf) {
        prt!(out, "Missing devices\n");
        for ca in self.members() {
            if !ca.is_online() && ca.has_data() != 0 {
                prt!(out, "Device {}\n", ca.dev_idx);
                out.indent_add(2);
                dev_to_text_short(out, self.sb_lock().sb(), ca.dev_idx.into());
                out.indent_sub(2);
            }
        }
    }
}

#[cold]
#[c_export]
pub fn bch2_missing_devs_to_text(out: &mut Printbuf, c: &Fs) {
    c.missing_devs_to_text(out)
}

/// While one is held, the filesystem can't finish going read-only
pub type WriteRef = RefGuard<EnumeratedRef>;

impl Fs {
    pub fn write_refs_to_text(&self, out: &mut Printbuf) {
        // SAFETY: bch2_write_refs names c->writes' kinds
        unsafe { self.writes.to_text(out, c::bch2_write_refs.as_ptr()) }
    }

    /// For work holding a write ref: going read-only drains it
    pub fn write_ref_wq(&self) -> &crate::util::workqueue::Queue {
        // SAFETY: the filesystem's, for as long as it is
        unsafe { crate::util::workqueue::Queue::from_raw(self.write_ref_wq) }
    }

    /// Holds a write ref until @make's future is done; EROFS if going read-only
    pub fn spawn_write<M, F>(&self, make: M) -> Result<(), BchError>
    where
        M: FnOnce(BackRef<Fs>) -> F + Send + 'static,
        F: Future<Output = ()> + Send + 'static,
    {
        let w: WriteRef = RefGuard::tryget_live(&self.writes).ok_or(BchError::from(c::EROFS))?;
        // SAFETY: going read-only waits for the task's write ref: the
        // filesystem and its write_ref_wq outlive it
        let (fs, wq) = unsafe { (BackRef::new(self), Queue::from_raw(self.write_ref_wq)) };
        // SAFETY: what the future has is 'static, or the BackRef above's
        unsafe {
            crate::util::task::spawn_detached(crate::util::task::Spawn::Queue(wq), async move {
                let _w = w;
                make(fs).await
            }, crate::util::alloc::flags::GFP_KERNEL)
        }
    }
}

/// A delayed work item in bch_fs: a unit struct with init(), for the field's
/// #[init], and run(). It holds no write ref: going read-only cancels it.
///
/// ```text
/// fs_delayed_work! {
///     pub(crate) work BtreeBitmapGcSchedule: field maybe_schedule_btree_bitmap_gc;
///
///     fn run(fs: &Fs) { ... }
/// }
/// ```
#[macro_export]
macro_rules! fs_delayed_work {
    (
        $(#[$attr:meta])*
        $vis:vis work $name:ident: field $($field:ident).+;

        fn run($fs:ident: &Fs) $body:block
    ) => {
        $(#[$attr])*
        $vis struct $name;

        impl $name {
            $vis fn init() -> impl ::pin_init::PinInit<$crate::util::workqueue::DelayedWork> {
                #[cfg(kernel)]
                {
                    // SAFETY: NUL-terminated
                    let name = |s: &'static str| unsafe {
                        ::core::ffi::CStr::from_bytes_with_nul_unchecked(s.as_bytes())
                    };
                    $crate::util::workqueue::DelayedWork::new(
                        Self::work_fn,
                        name(concat!(stringify!($name), "\0")),
                        ::kernel::static_lock_class!(),
                        name(concat!(stringify!($name), ".timer\0")),
                        ::kernel::static_lock_class!())
                }
                #[cfg(not(kernel))]
                $crate::util::workqueue::DelayedWork::new(Self::work_fn)
            }

            fn run($fs: &$crate::fs::Fs) $body

            /// # Safety
            /// @work is the field's.
            unsafe extern "C" fn work_fn(work: *mut $crate::c::work_struct) {
                let c = unsafe {
                    work.byte_sub(::core::mem::offset_of!($crate::c::delayed_work, work))
                        .byte_sub(::core::mem::offset_of!($crate::c::bch_fs, $($field).+))
                        .cast::<$crate::c::bch_fs>()
                };
                Self::run(unsafe { $crate::fs::Fs::borrow_raw(c) })
            }
        }
    };
}

/// BCH_FS_stopping set: for ro_ref_events' waiters, as for BCH_FS_started
#[c_export]
pub fn bch2_fs_stopping_wake(c: &crate::util::ffi::Opaque<c::bch_fs>) {
    Fs::from_c(c).ro_ref_events.wake();
}

/// c->ro_ref: bch2_fs_stop() waits for them, and tryget fails once it's
/// stopping
#[cfg(kernel)]
pub enum RoRefs {}

#[cfg(kernel)]
pub type RoRef = RefGuard<Fs, RoRefs>;

#[cfg(kernel)]
impl Fs {
    fn ro_ref_raw(&self) -> *mut c::refcount_t {
        // SAFETY: the filesystem's, for as long as it is
        unsafe { &raw mut (*self.as_ptr()).ro_ref }
    }
}

// SAFETY: a refcount_t, which the filesystem outlives
#[cfg(kernel)]
unsafe impl Refcount<RoRefs> for Fs {
    fn inc(&self) {
        // SAFETY: c->ro_ref
        unsafe { c::refcount_inc(self.ro_ref_raw()) }
    }

    unsafe fn dec(&self) {
        // SAFETY: c->ro_ref, the caller's ref; the waitqueue's live while
        // there's a ref to wait for
        unsafe {
            if c::refcount_dec_and_test(self.ro_ref_raw()) {
                c::__wake_up(&raw mut (*self.as_ptr()).ro_ref_wait, kernel::task::TASK_NORMAL, 1,
                             core::ptr::null_mut());
            }
        }
    }
}

// SAFETY: refcount_inc_not_zero() takes a ref when it succeeds
#[cfg(kernel)]
unsafe impl TryRefcount<RoRefs> for Fs {
    fn try_inc(&self) -> bool {
        // SAFETY: c->ro_ref
        !self.flag(c::bch_fs_flags::BCH_FS_stopping) &&
            unsafe { c::rust_refcount_inc_not_zero(self.ro_ref_raw()) }
    }
}

/// The low bit marks it hashed
pub fn writepoint_hashed(v: usize) -> c::write_point_specifier {
    c::write_point_specifier { v: (v | 1) as _ }
}

pub fn writepoint_ptr(wp: &c::write_point) -> c::write_point_specifier {
    c::write_point_specifier { v: core::ptr::from_ref(wp) as _ }
}

pub fn btree_id_is_alloc(id: u32) -> bool {
    matches!(
        Btree::from_raw(id),
        Some(Btree::alloc
            | Btree::backpointers
            | Btree::stripe_backpointers
            | Btree::need_discard
            | Btree::freespace
            | Btree::bucket_gens
            | Btree::lru
            | Btree::accounting
            | Btree::reconcile_work
            | Btree::reconcile_hipri
            | Btree::reconcile_pending
            | Btree::reconcile_scan)
    )
}
