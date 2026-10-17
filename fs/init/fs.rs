use crate::c;
use crate::btree::bkey::AsBkeyI;
use crate::errcode::{bch_errcode, ret_to_result_void as ret_to_result, BchError};
use crate::alloc::buckets::DiskReservation;
use crate::btree::iter::{BtreeIterFlags, CommitOpts, UpdateTriggerFlags};
use crate::util::locking::MemallocFlags;
use crate::util::Printbuf;
use core::ops::ControlFlow;

/// A device ref, put on drop. Derefs to &bch_dev.
pub struct DevRef(*mut c::bch_dev);

impl DevRef {
    /// For C functions taking *mut bch_dev.
    pub fn as_mut_ptr(&self) -> *mut c::bch_dev {
        self.0
    }

    /// The device name (sdX etc).
    pub fn name(&self) -> &core::ffi::CStr {
        unsafe { core::ffi::CStr::from_ptr((*self.0).name.as_ptr().cast()) }
    }
}

impl core::ops::Deref for DevRef {
    type Target = c::bch_dev;
    fn deref(&self) -> &c::bch_dev {
        unsafe { &*self.0 }
    }
}

impl Drop for DevRef {
    fn drop(&mut self) {
        unsafe { c::rust_bch2_dev_put(self.0) };
    }
}

/// sb_lock, unlocked on drop. It guards allocations that mustn't recurse into
/// reclaim IO, so it brackets a PF_MEMALLOC_NOIO scope: Drop unlocks before the
/// _noio field restores, as the C does.
pub struct SbLockGuard<'a> {
    fs: &'a Fs,
    _noio: MemallocFlags,
}

impl Drop for SbLockGuard<'_> {
    fn drop(&mut self) {
        unsafe { c::mutex_unlock(&mut (*self.fs.as_ptr()).sb_lock.lock); }
    }
}

pub use crate::cstructs::types::Fs;

impl Fs {
    /// For C that takes the filesystem.
    pub fn as_ptr(&self) -> *mut Fs {
        self as *const Fs as *mut Fs
    }
}

#[derive(Copy, Clone)]
pub struct BorrowedFs(*mut c::bch_fs);

// SAFETY: BorrowedFs is a non-owning pointer to a live filesystem supplied by
// C. Users must ensure the borrowed bch_fs outlives all cross-thread users.
unsafe impl Send for BorrowedFs {}
// SAFETY: bch_fs internal synchronization is handled by the filesystem code.
unsafe impl Sync for BorrowedFs {}

impl BorrowedFs {
    /// A cross-thread non-owning handle: the bch_fs must outlive its users.
    pub fn new(fs: &Fs) -> Self {
        Self(fs.as_ptr())
    }

    pub fn get(&self) -> &Fs {
        unsafe { Fs::borrow_raw(self.0) }
    }
}

/// The version's number and name.
impl core::fmt::Display for c::bcachefs_metadata_version {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        crate::printbuf_to_formatter(f, |out| unsafe { c::bch2_version_to_text(out, *self) })
    }
}

impl Fs {
    /// The filesystem's name, as its messages give it.
    pub fn name(&self) -> &core::ffi::CStr {
        unsafe { core::ffi::CStr::from_ptr((*self.as_ptr()).name.as_ptr().cast()) }
    }

    /// The filesystem's options, as resolved at open.
    pub fn opts(&self) -> &c::bch_opts {
        &self.opts
    }

    /// log2 of the filesystem block size in 512-byte sectors.
    pub fn block_bits(&self) -> u32 {
        self.block_bits as u32
    }

    /// The inode number shard the current thread allocates from: as
    /// bch2_inode_shard_idx().
    pub fn inode_shard_idx(&self) -> u64 {
        unsafe { c::rust_bch2_inode_shard_idx(self.as_ptr()) }
    }

    pub fn chacha20_key_set(&self) -> bool {
        self.chacha20_key_set
    }

    pub fn disk_sb(&self) -> &c::bch_sb_handle {
        &self.disk_sb
    }

    /// # Safety
    /// @raw points to a live bch_fs, that outlives 'a.
    pub unsafe fn borrow_raw<'a>(raw: *mut c::bch_fs) -> &'a Fs {
        unsafe { &*raw }
    }

    /// For an entry point C calls with its filesystem: safe, as an Opaque is
    /// always a live one.
    pub fn from_c(c: &crate::util::ffi::Opaque<c::bch_fs>) -> &Fs {
        unsafe { Self::borrow_raw(c.as_ptr()) }
    }

    pub fn sb_handle(&self) -> &c::bch_sb_handle {
        &self.disk_sb
    }

    pub fn sb(&self) -> &c::bch_sb {
        self.sb_handle().sb()
    }

    /// The metadata version the filesystem is at: C's c->sb.version, the
    /// in-memory copy, which is what decides version-gated behaviour.
    pub fn version(&self) -> c::bcachefs_metadata_version {
        c::bcachefs_metadata_version(self.sb.version as u32)
    }

    /// The version the last upgrade completed to: c->sb.version_upgrade_complete.
    /// Below version() while an upgrade's migrations are still running.
    pub fn version_upgrade_complete(&self) -> c::bcachefs_metadata_version {
        c::bcachefs_metadata_version(self.sb.version_upgrade_complete as u32)
    }

    /// Whether btree @id lost data - topology repair dropped nodes - so
    /// repairs may reconstruct what it's missing: c->sb.btrees_lost_data.
    pub fn btree_lost_data(&self, id: c::btree_id) -> bool {
        self.sb.btrees_lost_data & (1u64 << id.0) != 0
    }

    /// Whether this is an image made with `bcachefs dump --sanitize`, which
    /// scrubs dirent names without rehashing them: c->sb.dirents_sanitized.
    pub fn dirents_sanitized(&self) -> bool {
        self.sb.dirents_sanitized
    }

    /// Whether filesystem flag @f (BCH_FS_*) is set: C's test_bit() on
    /// c->flags. Other threads set and clear them, so it's an atomic load.
    pub fn flag(&self, f: c::bch_fs_flags) -> bool {
        let flags = unsafe {
            &*(core::ptr::addr_of!((*self.as_ptr()).flags) as *const core::sync::atomic::AtomicUsize)
        };
        flags.load(core::sync::atomic::Ordering::Relaxed) & (1usize << f as u32) != 0
    }

    /// Whether a mount is drawing recovery progress from
    /// BCH_IOCTL_RECOVERY_STATUS, so progress indicators needn't print it.
    /// The ioctl sets it - WRITE_ONCE() - so it's an atomic load.
    pub fn stdio_progress_reader(&self) -> bool {
        let r = unsafe {
            &*(core::ptr::addr_of!((*self.as_ptr()).stdio_progress_reader)
               as *const core::sync::atomic::AtomicBool)
        };
        r.load(core::sync::atomic::Ordering::Relaxed)
    }

    /// The journal sequence number being written: C's journal_cur_seq().
    pub fn journal_cur_seq(&self) -> u64 {
        // atomic64_read(&j->seq)
        let seq = unsafe { &*(&raw const (*self.as_ptr()).journal.seq).cast::<core::sync::atomic::AtomicI64>() };
        seq.load(core::sync::atomic::Ordering::Relaxed) as u64
    }

    /// Go emergency read-only: the journal halts, so nothing more commits,
    /// and the rest of going read-only is queued. As
    /// bch2_fs_emergency_read_only(). Whether this call is what did it - if
    /// so, @out says so, and is no longer suppressed.
    pub fn emergency_read_only(&self, out: &mut Printbuf) -> bool {
        unsafe { c::bch2_fs_emergency_read_only(self.as_ptr(), out.as_raw()) }
    }

    pub fn sb_lock(&self) -> SbLockGuard<'_> {
        let _noio = MemallocFlags::noio();
        unsafe { c::mutex_lock(&mut (*self.as_ptr()).sb_lock.lock); }
        SbLockGuard { fs: self, _noio }
    }

    /// Caller holds sb_lock.
    pub fn write_super(&self) {
        unsafe { c::bch2_write_super(self.as_ptr()) };
    }

    /// Throw @error, as C's bch_err_throw() does: counted in error_throw and
    /// traced, so errors started in Rust are as visible as errors started in C.
    /// Naming an error to compare or parse against isn't throwing it - that's
    /// BchError::from().
    pub fn err(&self, error: bch_errcode) -> BchError {
        BchError::from_raw(-unsafe { c::__bch2_err_throw(self.as_ptr(), -(error.0 as i32)) })
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

    /// # Safety
    /// Caller holds sb_lock.
    #[allow(clippy::mut_from_ref)] // interior mutability guarded by sb_lock
    pub unsafe fn member_mut(&self, dev_idx: u32) -> &mut c::bch_member {
        unsafe { &mut *c::bch2_members_v2_get_mut((*self.as_ptr()).disk_sb.sb, dev_idx as i32) }
    }

    /// Refs are put, including on early break.
    pub fn for_each_online_member<F>(&self, mut f: F) -> ControlFlow<()>
    where
        F: FnMut(&c::bch_dev) -> ControlFlow<()>,
    {
        let mut ca: *mut c::bch_dev = core::ptr::null_mut();
        loop {
            // any device state, READ ref-class, ref_idx 0.
            ca = unsafe { c::rust_bch2_get_next_online_dev(self.as_ptr(), ca, !0u32, 0 /* READ */, 0) };
            if ca.is_null() {
                return ControlFlow::Continue(());
            }
            if f(unsafe { &*ca }).is_break() {
                unsafe { c::rust_enumerated_ref_put(&mut (*ca).io_ref[0 /* READ */], 0) };
                return ControlFlow::Break(());
            }
        }
    }

    pub fn btree_id_root(&self, id: u32) -> Option<&c::btree> {
        unsafe {
            let c = &*self.as_ptr();
            let nr_known = u32::from(c::btree_id::nr);

            let r = if id < nr_known {
                &c.btree.cache.roots_known[id as usize]
            } else {
                let idx = (id - nr_known) as usize;
                if idx >= c.btree.cache.roots_extra.nr {
                    return None;
                }
                &*c.btree.cache.roots_extra.data.add(idx)
            };

            let b = r.b;
            if b.is_null() { None } else { Some(&*b) }
        }
    }

    /// Total number of btree IDs (known + dynamic) on this filesystem.
    pub fn btree_id_nr_alive(&self) -> u32 {
        unsafe {
            let c = &*self.as_ptr();
            u32::from(c::btree_id::nr) + c.btree.cache.roots_extra.nr as u32
        }
    }

    /// Number of devices in the filesystem superblock.
    pub fn nr_devices(&self) -> u32 {
        self.sb.nr_devices as u32
    }

    /// Get a reference to a device by index. Returns None if the device
    /// doesn't exist or can't be referenced.
    pub fn dev_get(&self, dev: u32) -> Option<DevRef> {
        let ca = unsafe { c::rust_bch2_dev_tryget_noerror(self.as_ptr(), dev) };
        if ca.is_null() { None } else { Some(DevRef(ca)) }
    }

    /// Run an option's pre-set hook; None dev = filesystem scope.
    pub fn opt_hook_pre_set(&self, dev: Option<&DevRef>, id: c::bch_opt_id, v: u64)
        -> Result<(), BchError>
    {
        ret_to_result(unsafe {
            c::bch2_opt_hook_pre_set(self.as_ptr(),
                dev.map_or(core::ptr::null_mut(), |d| d.as_mut_ptr()),
                0, id, v, true, core::ptr::null_mut())
        })
    }

    /// Set an option in the superblock; None dev = filesystem scope.
    pub fn opt_set_sb(&self, dev: Option<&DevRef>, opt: &c::bch_option, v: u64,
                      val_str: Option<&core::ffi::CStr>)
    {
        unsafe {
            c::bch2_opt_set_sb(self.as_ptr(),
                dev.map_or(core::ptr::null_mut(), |d| d.as_mut_ptr()),
                opt, v,
                val_str.map_or(core::ptr::null(), |s| s.as_ptr().cast()));
        }
    }

    pub fn start(&self) -> Result<(), BchError> {
        ret_to_result(unsafe { c::bch2_fs_start(self.as_ptr()) })
    }

    pub fn buckets_nouse_alloc(&self) -> Result<(), BchError> {
        ret_to_result(unsafe { c::bch2_buckets_nouse_alloc(self.as_ptr()) })
    }

    /// Mark device superblock buckets in btree metadata.
    pub fn trans_mark_dev_sb(&self, ca: &DevRef, flags: UpdateTriggerFlags) -> Result<(), BchError> {
        ret_to_result(unsafe { c::bch2_trans_mark_dev_sb(self.as_ptr(), ca.as_mut_ptr(), c::btree_iter_update_trigger_flags::from_bits_retain(flags.bits())) })
    }

    /// Flush the journal: everything committed before the call is on disk when
    /// it returns.
    pub fn journal_flush(&self) -> Result<(), BchError> {
        ret_to_result(unsafe { c::bch2_journal_flush(&mut (*self.as_ptr()).journal) })
    }

    /// Set @inum's i_size to @new_i_size and drop every extent past it, as a
    /// logged op, so a crash midway resumes rather than leaving extents past
    /// EOF. Block granular: zeroing the rest of the block @new_i_size falls
    /// in is the caller's job, as the VFS does it in the page cache.
    pub fn truncate(&self, inum: c::subvol_inum, new_i_size: u64) -> Result<(), BchError> {
        crate::data::io_misc::truncate(self, inum, new_i_size, &mut 0)
    }

    /// Make the filesystem incompatible with versions before @version, if
    /// it isn't already - an error if that isn't allowed: as
    /// bch2_request_incompat_feature().
    pub fn request_incompat_feature(&self, version: c::bcachefs_metadata_version)
        -> Result<(), BchError>
    {
        if version.0 <= unsafe { (*self.as_ptr()).sb.version_incompat } as u32 {
            return Ok(());
        }
        ret_to_result(unsafe { c::bch2_set_version_incompat(self.as_ptr(), version) })
    }

    /// Whether the filesystem uses @feature: c->sb.features, the in-memory
    /// copy.
    pub fn feature(&self, feature: c::bch_sb_feature) -> bool {
        self.sb.features & (1u64 << feature.0) != 0
    }

    /// Mark the filesystem as using @feature, writing the superblock if it
    /// wasn't already: as bch2_check_set_feature().
    pub fn check_set_feature(&self, feature: c::bch_sb_feature) {
        if !self.feature(feature) {
            unsafe { c::__bch2_check_set_feature(self.as_ptr(), feature.0) }
        }
    }

    /// Whether casefolding can be used: an error without CONFIG_UNICODE, or
    /// with the casefold_disabled option - as bch2_fs_casefold_enabled().
    pub fn casefold_enabled(&self) -> Result<(), BchError> {
        if !cfg!(CONFIG_UNICODE) {
            return self.throw(bch_errcode::BCH_ERR_no_casefolding_without_utf8);
        }
        if self.opts().casefold_disabled != 0 {
            return self.throw(bch_errcode::BCH_ERR_casefolding_disabled);
        }
        Ok(())
    }

    /// Caller holds sb_lock.
    pub fn write_super_ret(&self) -> Result<(), BchError> {
        ret_to_result(unsafe { c::bch2_write_super(self.as_ptr()) })
    }

    /// A superblock write that's part of bringing the filesystem up, on a
    /// nostart open that will start it afterwards - allowed before start,
    /// like recovery's own. Caller must hold sb_lock.
    pub fn write_super_bringup(&self) -> Result<(), BchError> {
        ret_to_result(unsafe {
            c::bch2_write_super_flags(self.as_ptr(), c::bch_sb_write_flags::BCH_SB_WRITE_bringup)
        })
    }

    /// Apply in-memory superblock edits to the in-memory state derived from
    /// it (c->sb, member info) without writing - for an open that will start
    /// and persist them then. Caller must hold sb_lock.
    pub fn sb_update(&self) {
        unsafe { c::bch2_sb_update(self.as_ptr()) }
    }

    /// Check if a device index exists and has a device pointer.
    pub fn dev_exists(&self, dev: u32) -> bool {
        unsafe {
            let c = &*self.as_ptr();
            (dev as usize) < c.sb.nr_devices as usize
                && !c.devs[dev as usize].is_null()
        }
    }

    pub fn read_only(&self) {
        unsafe { c::bch2_fs_read_only(self.as_ptr()) };
    }

    pub fn dev_allocator_set_rw(&self, dev: u32, rw: bool) {
        unsafe {
            let ca = (*self.as_ptr()).devs[dev as usize];
            if !ca.is_null() {
                c::bch2_dev_allocator_set_rw(self.as_ptr(), ca, rw);
            }
        }
    }

    pub fn journal_flush_all_pins(&self) {
        unsafe {
            c::bch2_journal_flush_pins(
                &mut (*self.as_ptr()).journal,
                u64::MAX,
            );
        }
    }

    /// Flush pins up to the current journal sequence.
    pub fn journal_flush_outstanding_pins(&self) -> bool {
        unsafe { c::bch2_journal_flush_pins(&mut (*self.as_ptr()).journal, self.journal_cur_seq()) }
    }

    pub fn btree_delete_range(
        &self,
        btree_id: c::btree_id,
        start: c::bpos,
        end: c::bpos,
        flags: BtreeIterFlags,
    ) -> Result<(), BchError> {
        ret_to_result(unsafe {
            c::bch2_btree_delete_range(self.as_ptr(), btree_id, start, end, c::btree_iter_update_trigger_flags::from_bits_retain(flags.bits()))
        })
    }

    pub fn btree_insert(
        &self,
        btree_id:     c::btree_id,
        key:          &mut (impl AsBkeyI + ?Sized),
        disk_res:     Option<&DiskReservation<'_>>,
        commit_flags: impl Into<CommitOpts>,
        iter_flags:   BtreeIterFlags,
    ) -> Result<(), BchError> {
        let disk_res = disk_res
            .map(|r| r.as_mut_ptr())
            .unwrap_or(core::ptr::null_mut());

        ret_to_result(unsafe {
            c::bch2_btree_insert(
                self.as_ptr(),
                btree_id,
                key.as_bkey_i_mut().as_ffi_mut(),
                disk_res,
                commit_flags.into().to_c(),
                c::btree_iter_update_trigger_flags::from_bits_retain(iter_flags.bits()),
            )
        })
    }

    pub fn dev_usage_full_read(&self, dev: u32) -> c::bch_dev_usage_full {
        unsafe {
            let ca = (*self.as_ptr()).devs[dev as usize];
            let mut usage: c::bch_dev_usage_full = core::mem::zeroed();
            c::bch2_dev_usage_full_read_fast(ca, &mut usage);
            usage
        }
    }

    /// # Safety
    /// The device exists.
    pub unsafe fn dev_raw(&self, dev: u32) -> *mut c::bch_dev {
        (*self.as_ptr()).devs[dev as usize]
    }

    /// For resize.
    ///
    /// # Safety
    /// Caller holds sb_lock.
    #[allow(clippy::mut_from_ref)] // interior mutability guarded by sb_lock
    pub unsafe fn disk_sb_mut(&self) -> &mut c::bch_sb_handle {
        &mut (*self.as_ptr()).disk_sb
    }

    pub fn block_bytes(&self) -> u64 {
        self.opts().block_size as u64
    }

    pub fn time_to_timespec(&self, time: i64) -> c::timespec64 {
        const NSEC_PER_SEC: i64 = 1_000_000_000;

        let sb = unsafe { &(*self.as_ptr()).sb };
        let time = time.wrapping_add(sb.time_base_lo as i64);
        let units = sb.time_units_per_sec as i64;

        let mut sec  = time / units;
        let mut nsec = (time % units) * sb.nsec_per_time_unit as i64;

        // set_normalized_timespec64()
        while nsec >= NSEC_PER_SEC {
            nsec -= NSEC_PER_SEC;
            sec += 1;
        }
        while nsec < 0 {
            nsec += NSEC_PER_SEC;
            sec -= 1;
        }

        let mut t: c::timespec64 = unsafe { core::mem::zeroed() };
        t.tv_sec  = sec as _;
        t.tv_nsec = nsec as _;
        t
    }

    // tv_sec is the kernel's time64_t, or userspace's time_t - 32 bits on some
    // targets
    #[allow(clippy::unnecessary_cast)]
    pub fn timespec_to_time(&self, ts: c::timespec64) -> i64 {
        let sb = unsafe { &(*self.as_ptr()).sb };
        // C's (int) tv_nsec / nsec_per_time_unit: an unsigned division
        (ts.tv_sec as i64).wrapping_mul(sb.time_units_per_sec as i64)
            .wrapping_add((ts.tv_nsec as i32 as u32 / sb.nsec_per_time_unit) as i64)
            .wrapping_sub(sb.time_base_lo as i64)
    }

    /// In the filesystem's time units.
    pub fn current_time(&self) -> u64 {
        let mut now: c::timespec64 = unsafe { core::mem::zeroed() };
        unsafe { c::bch2_ktime_get_coarse_real_ts64(&mut now) };
        self.timespec_to_time(now) as u64
    }

    pub fn usage_read_short(&self) -> c::bch_fs_usage_short {
        unsafe { c::bch2_fs_usage_read_short(self.as_ptr()) }
    }

    pub fn set_loglevel(&self, level: u32) {
        unsafe { (*self.as_ptr()).loglevel = level; }
    }
}

/// The memory of a new bch_fs: Fs::init()'s - its Rust-typed fields'
/// initializers run, everything else zeroed - for bch2_fs_init() to go on
/// with, as it did from kvzalloc(). NULL if out of memory.
#[cstruct_macros::c_export]
pub fn bch2_fs_alloc_mem() -> *mut c::bch_fs {
    crate::util::alloc::alloc_pinned(Fs::init()).unwrap_or(core::ptr::null_mut())
}

/// Drop @c and free it: the end of its teardown, as kvfree() was. Its
/// Rust-typed fields are dropped; its C ones C's teardown has done, and their
/// Drop - a printbuf's - finds them empty.
///
/// # Safety
/// @c is from bch2_fs_alloc_mem(), and nothing uses it after.
#[cstruct_macros::c_export]
pub unsafe fn bch2_fs_free_mem(c: *mut c::bch_fs) {
    unsafe { crate::util::alloc::free_pinned(c) }
}

/// A filesystem opened from userspace, which the holder closes:
/// bch2_fs_exit() - stop it, and put the ref - on exit() or drop.
pub struct OpenFs(core::ptr::NonNull<Fs>);

impl OpenFs {
    /// # Safety
    /// @fs is from bch2_fs_open(), and this takes over its ref.
    pub unsafe fn from_raw(fs: core::ptr::NonNull<Fs>) -> Self {
        OpenFs(fs)
    }

    /// bch2_fs_exit()'s error, which Drop discards.
    pub fn exit(self) -> i32 {
        let ret = unsafe { c::bch2_fs_exit(self.0.as_ptr()) };
        core::mem::forget(self);
        ret
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

/// A write ref on @fs, of kind @idx: while one is held, the filesystem can't
/// go read-only. Dropping it puts it.
pub struct WriteRef<'f> {
    fs:  &'f Fs,
    idx: c::bch_write_ref,
}

impl Fs {
    /// A write ref of kind @idx, unless the filesystem is going read-only: as
    /// enumerated_ref_tryget(&c->writes, idx).
    pub fn write_ref_tryget(&self, idx: c::bch_write_ref) -> Option<WriteRef<'_>> {
        unsafe { c::rust_enumerated_ref_tryget(&raw mut (*self.as_ptr()).writes, idx as u32) }
            .then_some(WriteRef { fs: self, idx })
    }
}

impl<'f> WriteRef<'f> {
    /// Take ownership of a ref handed over without a WriteRef - by
    /// queue_work(), to the work it queues.
    ///
    /// # Safety
    /// The caller owns a write ref of kind @idx on @fs, and hands it over.
    pub unsafe fn adopt(fs: &'f Fs, idx: c::bch_write_ref) -> Self {
        WriteRef { fs, idx }
    }

    /// Queue @work on @fs's write_ref_wq, handing it this ref - its function
    /// adopt()s it, and drops it when done. If @work is already queued, the
    /// run that's pending has a ref, and this one is put.
    pub fn queue_work(self, work: &FsWork) {
        if unsafe { c::bch2_queue_work((*self.fs.as_ptr()).write_ref_wq, work.as_ptr()) } {
            core::mem::forget(self);
        }
    }
}

/// A work item in the filesystem, run holding a write ref - struct
/// work_struct to C. fs_work!() declares one: what it runs, the field it's
/// in, the ref kind it holds - see there.
#[repr(transparent)]
pub struct FsWork {
    work: core::cell::UnsafeCell<c::work_struct>,
    _pin: core::marker::PhantomPinned,
}

impl FsWork {
    /// For C, and the workqueue.
    pub fn as_ptr(&self) -> *mut c::work_struct {
        self.work.get()
    }

    /// INIT_WORK(): with @func, and a lockdep class of its own - @key, named
    /// @name. fs_work!()'s init() is this, with a static key per work.
    #[cfg(kernel)]
    #[doc(hidden)]
    pub fn new(
        func: unsafe extern "C" fn(*mut c::work_struct),
        name: &'static core::ffi::CStr,
        key:  core::pin::Pin<&'static kernel::sync::LockClassKey>,
    ) -> impl ::pin_init::PinInit<Self> {
        // SAFETY: init_work_with_key() initializes all of it, and it's pinned
        unsafe {
            ::pin_init::pin_init_from_closure(move |slot: *mut Self| {
                c::init_work_with_key(slot.cast(), Some(func), false, name.as_ptr().cast(),
                                      key.as_ptr());
                Ok(())
            })
        }
    }

    /// INIT_WORK(), as the shim has it: no lockdep.
    #[cfg(not(kernel))]
    #[doc(hidden)]
    pub fn new(func: unsafe extern "C" fn(*mut c::work_struct)) -> impl ::pin_init::PinInit<Self> {
        // SAFETY: each field, written in place - its entry lists itself
        unsafe {
            ::pin_init::pin_init_from_closure(move |slot: *mut Self| {
                let w = slot.cast::<c::work_struct>();
                (*w).data.counter = 0;
                let entry = &raw mut (*w).entry;
                (*entry).next = entry;
                (*entry).prev = entry;
                (*w).func = Some(func);
                Ok(())
            })
        }
    }
}

/// A work item in bch_fs, queued holding a write ref: one declaration for its
/// function, its queueing and its initializer, so the ref kind the work
/// adopts is the one queue() took.
///
/// ```text
/// fs_work! {
///     /// What it does
///     pub(crate) work SubvolDelete:
///         field snapshots.wait_for_pagecache_and_delete_work,
///         write_ref BCH_WRITE_REF_snapshot_delete_pagecache;
///
///     fn run(fs: &Fs) { ... }
/// }
/// ```
///
/// is the unit struct SubvolDelete, with:
/// - queue(&Fs): try for the write ref and queue the work, handing it the
///   ref - Err(EROFS) going read-only; already queued, the pending run has one
/// - init(): the field's initializer - its #[init(SubvolDelete::init())]
/// and run() runs from the workqueue with the ref held, put when it returns.
#[macro_export]
macro_rules! fs_work {
    (
        $(#[$attr:meta])*
        $vis:vis work $name:ident:
            field $($field:ident).+,
            write_ref $ref:ident;

        fn run($fs:ident: &Fs) $body:block
    ) => {
        $(#[$attr])*
        $vis struct $name;

        impl $name {
            $vis fn queue(fs: &$crate::fs::Fs) -> Result<(), $crate::errcode::BchError> {
                let r = fs.write_ref_tryget($crate::c::bch_write_ref::$ref)
                    .ok_or_else(|| $crate::errcode::BchError::from($crate::c::EROFS))?;
                r.queue_work(&fs.$($field).+);
                Ok(())
            }

            $vis fn init() -> impl ::pin_init::PinInit<$crate::fs::FsWork> {
                #[cfg(kernel)]
                {
                    $crate::fs::FsWork::new(
                        Self::work_fn,
                        // SAFETY: NUL-terminated
                        unsafe {
                            ::core::ffi::CStr::from_bytes_with_nul_unchecked(
                                concat!(stringify!($name), "\0").as_bytes())
                        },
                        ::kernel::static_lock_class!())
                }
                #[cfg(not(kernel))]
                $crate::fs::FsWork::new(Self::work_fn)
            }

            fn run($fs: &$crate::fs::Fs) $body

            /// # Safety
            /// @work is the field's, queued by queue(), which handed it a ref.
            unsafe extern "C" fn work_fn(work: *mut $crate::c::work_struct) {
                let c = unsafe {
                    work.byte_sub(::core::mem::offset_of!($crate::c::bch_fs, $($field).+))
                        .cast::<$crate::c::bch_fs>()
                };
                let fs = unsafe { $crate::fs::Fs::borrow_raw(c) };
                let _write_ref = unsafe {
                    $crate::fs::WriteRef::adopt(fs, $crate::c::bch_write_ref::$ref)
                };
                Self::run(fs)
            }
        }
    };
}

impl Drop for WriteRef<'_> {
    fn drop(&mut self) {
        unsafe { c::rust_enumerated_ref_put(&raw mut (*self.fs.as_ptr()).writes, self.idx as u32) };
    }
}

// Standalone helpers — pure Rust reimplementations of C static inlines.

pub fn bucket_to_sector(ca: &c::bch_dev, b: u64) -> u64 {
    b * ca.mi.bucket_size as u64
}

pub fn bucket_bytes(ca: &c::bch_dev) -> u64 {
    ca.mi.bucket_size as u64 * 512
}

/// Build a hashed writepoint specifier (sets low bit to mark as hashed).
pub fn writepoint_hashed(v: usize) -> c::write_point_specifier {
    c::write_point_specifier { v: (v | 1) as _ }
}

/// Convert a device index to a target (TARGET_DEV_START = 1).
pub fn dev_to_target(dev: u32) -> u16 {
    1 + dev as u16
}

pub fn btree_id_is_alloc(id: u32) -> bool {
    matches!(
        c::btree_id::from_raw(id),
        Some(c::btree_id::alloc
            | c::btree_id::backpointers
            | c::btree_id::stripe_backpointers
            | c::btree_id::need_discard
            | c::btree_id::freespace
            | c::btree_id::bucket_gens
            | c::btree_id::lru
            | c::btree_id::accounting
            | c::btree_id::reconcile_work
            | c::btree_id::reconcile_hipri
            | c::btree_id::reconcile_pending
            | c::btree_id::reconcile_scan)
    )
}
