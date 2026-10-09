use crate::c;
use crate::btree::bkey::AsBkeyI;
use crate::errcode::{bch_errcode, ret_to_result_void as ret_to_result, BchError};
use crate::alloc::buckets::DiskReservation;
use crate::btree::iter::{BtreeIterFlags, CommitOpts, UpdateTriggerFlags};
use crate::util::locking::MemallocFlags;
use crate::util::ffi::Opaque;
use crate::util::Printbuf;
use core::ops::ControlFlow;

/// RAII guard for a device reference. Calls bch2_dev_put on drop.
///
/// Obtained via `Fs::dev_get()`. Derefs to `&bch_dev` for read access.
pub struct DevRef(*mut c::bch_dev);

impl DevRef {
    /// Get a raw mutable pointer to the device. Needed for C functions
    /// that take `*mut bch_dev`.
    pub fn as_mut_ptr(&self) -> *mut c::bch_dev {
        self.0
    }

    /// The device name (sdX etc).
    pub fn name(&self) -> &core::ffi::CStr {
        unsafe { core::ffi::CStr::from_ptr((*self.0).name.as_ptr()) }
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

/// RAII guard for bch_fs::sb_lock. Unlocks on drop.
///
/// Mirrors the C `mutex_noio` guard: sb_lock is held precisely to guard
/// allocations that must not recurse into reclaim IO, so the lock brackets a
/// PF_MEMALLOC_NOIO scope. The explicit Drop below runs before the `_noio`
/// field is dropped, so the order is unlock-then-restore - matching the C side.
pub struct SbLockGuard<'a> {
    fs: &'a Fs,
    _noio: MemallocFlags,
}

impl Drop for SbLockGuard<'_> {
    fn drop(&mut self) {
        unsafe { c::mutex_unlock(&mut (*self.fs.raw).sb_lock.lock); }
    }
}

/// Transparent: any `*mut bch_fs` in memory is an Fs in place - see
/// BtreeTrans::borrow_raw().
#[repr(transparent)]
pub struct Fs {
    pub raw: *mut c::bch_fs,
}

#[derive(Copy, Clone)]
pub struct BorrowedFs(*mut c::bch_fs);

// SAFETY: BorrowedFs is a non-owning pointer to a live filesystem supplied by
// C. Users must ensure the borrowed bch_fs outlives all cross-thread users.
unsafe impl Send for BorrowedFs {}
// SAFETY: bch_fs internal synchronization is handled by the filesystem code.
unsafe impl Sync for BorrowedFs {}

impl BorrowedFs {
    /// Create a cross-thread non-owning `Fs` handle.
    ///
    /// Callers must ensure the underlying bch_fs remains live while any
    /// resulting borrowed Fs views are in use.
    pub fn new(fs: &Fs) -> Self {
        Self(fs.raw)
    }

    pub fn get(&self) -> core::mem::ManuallyDrop<Fs> {
        unsafe { Fs::borrow_raw(self.0) }
    }
}

// Metadata versions only go up, so they order by number: "at least version X"
// is fs.version() >= bcachefs_metadata_version::X.
impl PartialOrd for c::bcachefs_metadata_version {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for c::bcachefs_metadata_version {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.0.cmp(&other.0)
    }
}

/// As bch2_version_to_text(): the version's number and name.
impl core::fmt::Display for c::bcachefs_metadata_version {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        crate::printbuf_to_formatter(f, |out| unsafe { c::bch2_version_to_text(out, *self) })
    }
}

impl Fs {
    /// The filesystem's name, as its messages give it.
    pub fn name(&self) -> &core::ffi::CStr {
        unsafe { core::ffi::CStr::from_ptr((*self.raw).name.as_ptr()) }
    }

    /// The filesystem's options, as resolved at open.
    pub fn opts(&self) -> &c::bch_opts {
        unsafe { &(*self.raw).opts }
    }

    /// log2 of the filesystem block size in 512-byte sectors.
    pub fn block_bits(&self) -> u32 {
        unsafe { (*self.raw).block_bits as u32 }
    }

    pub fn chacha20_key_set(&self) -> bool {
        unsafe { (*self.raw).chacha20_key_set }
    }

    /// The filesystem's superblock handle.
    pub fn disk_sb(&self) -> &c::bch_sb_handle {
        unsafe { &(*self.raw).disk_sb }
    }

    /// Create a non-owning `Fs` view from a raw pointer.
    ///
    /// Returns `ManuallyDrop<Fs>` to prevent `Fs::drop` from calling
    /// `bch2_fs_exit`. Deref gives `&Fs` for all methods.
    ///
    /// # Safety
    /// `raw` must point to a valid, live `bch_fs`.
    pub unsafe fn borrow_raw(raw: *mut c::bch_fs) -> core::mem::ManuallyDrop<Fs> {
        core::mem::ManuallyDrop::new(Fs { raw })
    }

    /// As borrow_raw(), for an entry point C calls with its filesystem:
    /// safe, as an Opaque is always a live one.
    pub fn from_c(c: &crate::util::ffi::Opaque<c::bch_fs>) -> core::mem::ManuallyDrop<Fs> {
        unsafe { Self::borrow_raw(c.as_ptr()) }
    }

    /// Access the superblock handle.
    pub fn sb_handle(&self) -> &c::bch_sb_handle {
        unsafe { &(*self.raw).disk_sb }
    }

    /// Access the superblock.
    pub fn sb(&self) -> &c::bch_sb {
        self.sb_handle().sb()
    }

    /// The metadata version the filesystem is at: C's c->sb.version, the
    /// in-memory copy, which is what decides version-gated behaviour.
    pub fn version(&self) -> c::bcachefs_metadata_version {
        c::bcachefs_metadata_version(unsafe { (*self.raw).sb.version } as u32)
    }

    /// The version the last upgrade completed to: c->sb.version_upgrade_complete.
    /// Below version() while an upgrade's migrations are still running.
    pub fn version_upgrade_complete(&self) -> c::bcachefs_metadata_version {
        c::bcachefs_metadata_version(unsafe { (*self.raw).sb.version_upgrade_complete } as u32)
    }

    /// Whether btree @id lost data - topology repair dropped nodes - so
    /// repairs may reconstruct what it's missing: c->sb.btrees_lost_data.
    pub fn btree_lost_data(&self, id: c::btree_id) -> bool {
        let lost = unsafe { (*self.raw).sb.btrees_lost_data };
        lost & (1u64 << id as u32) != 0
    }

    /// Whether this is an image made with `bcachefs dump --sanitize`, which
    /// scrubs dirent names without rehashing them: c->sb.dirents_sanitized.
    pub fn dirents_sanitized(&self) -> bool {
        unsafe { (*self.raw).sb.dirents_sanitized }
    }

    /// Whether filesystem flag @f (BCH_FS_*) is set: C's test_bit() on
    /// c->flags. Other threads set and clear them, so it's an atomic load.
    pub fn flag(&self, f: c::bch_fs_flags) -> bool {
        let flags = unsafe {
            &*(core::ptr::addr_of!((*self.raw).flags) as *const core::sync::atomic::AtomicUsize)
        };
        flags.load(core::sync::atomic::Ordering::Relaxed) & (1usize << f as u32) != 0
    }

    /// The journal sequence number being written: C's journal_cur_seq().
    pub fn journal_cur_seq(&self) -> u64 {
        // atomic64_read(&j->seq)
        let seq = unsafe { &*(&raw const (*self.raw).journal.seq).cast::<core::sync::atomic::AtomicI64>() };
        seq.load(core::sync::atomic::Ordering::Relaxed) as u64
    }

    /// Go emergency read-only: the journal halts, so nothing more commits,
    /// and the rest of going read-only is queued. As
    /// bch2_fs_emergency_read_only(). Whether this call is what did it - if
    /// so, @out says so, and is no longer suppressed.
    pub fn emergency_read_only(&self, out: &mut Printbuf) -> bool {
        unsafe { c::bch2_fs_emergency_read_only(self.raw, out.as_raw()) }
    }

    /// Acquire the superblock lock, returning a guard that releases it on drop.
    pub fn sb_lock(&self) -> SbLockGuard<'_> {
        let _noio = MemallocFlags::noio();
        unsafe { c::mutex_lock(&mut (*self.raw).sb_lock.lock); }
        SbLockGuard { fs: self, _noio }
    }

    /// Write superblock to disk. Caller must hold sb_lock.
    pub fn write_super(&self) {
        unsafe { c::bch2_write_super(self.raw) };
    }

    /// Throw @error, as C's bch_err_throw() does: counted in error_throw and
    /// traced, so errors started in Rust are as visible as errors started in C.
    /// Naming an error to compare or parse against isn't throwing it - that's
    /// BchError::from().
    pub fn err(&self, error: bch_errcode) -> BchError {
        BchError::from_raw(-unsafe { c::__bch2_err_throw(self.raw, -(error as i32)) })
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

    /// Get a mutable reference to a member entry in the superblock.
    /// Caller must hold sb_lock.
    ///
    /// # Safety
    /// Caller must hold sb_lock for mutation safety.
    #[allow(clippy::mut_from_ref)] // interior mutability guarded by sb_lock
    pub unsafe fn member_mut(&self, dev_idx: u32) -> &mut c::bch_member {
        unsafe { &mut *c::bch2_members_v2_get_mut((*self.raw).disk_sb.sb, dev_idx as i32) }
    }

    /// Shut down the filesystem, returning the error code from bch2_fs_exit.
    /// Consumes self so the caller can't use it afterward; forget prevents
    /// Drop from double-freeing.
    pub fn exit(self) -> i32 {
        let ret = unsafe { c::bch2_fs_exit(self.raw) };
        core::mem::forget(self);
        ret
    }

    /// Iterate over all online member devices.
    ///
    /// Equivalent to the C `for_each_online_member` macro. Ref counting
    /// is handled automatically, including on early break.
    pub fn for_each_online_member<F>(&self, mut f: F) -> ControlFlow<()>
    where
        F: FnMut(&c::bch_dev) -> ControlFlow<()>,
    {
        let mut ca: *mut c::bch_dev = core::ptr::null_mut();
        loop {
            // any device state, READ ref-class, ref_idx 0.
            ca = unsafe { c::rust_bch2_get_next_online_dev(self.raw, ca, !0u32, 0 /* READ */, 0) };
            if ca.is_null() {
                return ControlFlow::Continue(());
            }
            if f(unsafe { &*ca }).is_break() {
                unsafe { c::rust_enumerated_ref_put(&mut (*ca).io_ref[0 /* READ */], 0) };
                return ControlFlow::Break(());
            }
        }
    }

    /// Get the root btree node for a btree ID.
    pub fn btree_id_root(&self, id: u32) -> Option<&c::btree> {
        unsafe {
            let c = &*self.raw;
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
            let c = &*self.raw;
            u32::from(c::btree_id::nr) + c.btree.cache.roots_extra.nr as u32
        }
    }

    /// Number of devices in the filesystem superblock.
    pub fn nr_devices(&self) -> u32 {
        unsafe { (*self.raw).sb.nr_devices as u32 }
    }

    /// Get a reference to a device by index. Returns None if the device
    /// doesn't exist or can't be referenced.
    pub fn dev_get(&self, dev: u32) -> Option<DevRef> {
        let ca = unsafe { c::rust_bch2_dev_tryget_noerror(self.raw, dev) };
        if ca.is_null() { None } else { Some(DevRef(ca)) }
    }

    /// Run an option's pre-set hook; None dev = filesystem scope.
    pub fn opt_hook_pre_set(&self, dev: Option<&DevRef>, id: c::bch_opt_id, v: u64)
        -> Result<(), BchError>
    {
        ret_to_result(unsafe {
            c::bch2_opt_hook_pre_set(self.raw,
                dev.map_or(core::ptr::null_mut(), |d| d.as_mut_ptr()),
                0, id, v, true, core::ptr::null_mut())
        })
    }

    /// Set an option in the superblock; None dev = filesystem scope.
    pub fn opt_set_sb(&self, dev: Option<&DevRef>, opt: &c::bch_option, v: u64,
                      val_str: Option<&core::ffi::CStr>)
    {
        unsafe {
            c::bch2_opt_set_sb(self.raw,
                dev.map_or(core::ptr::null_mut(), |d| d.as_mut_ptr()),
                opt, v,
                val_str.map_or(core::ptr::null(), |s| s.as_ptr()));
        }
    }

    /// Start the filesystem (recovery, journal replay, etc).
    pub fn start(&self) -> Result<(), BchError> {
        ret_to_result(unsafe { c::bch2_fs_start(self.raw) })
    }

    /// Allocate the buckets_nouse bitmaps for all devices.
    pub fn buckets_nouse_alloc(&self) -> Result<(), BchError> {
        ret_to_result(unsafe { c::bch2_buckets_nouse_alloc(self.raw) })
    }

    /// Mark device superblock buckets in btree metadata.
    pub fn trans_mark_dev_sb(&self, ca: &DevRef, flags: UpdateTriggerFlags) -> Result<(), BchError> {
        ret_to_result(unsafe { c::bch2_trans_mark_dev_sb(self.raw, ca.as_mut_ptr(), c::btree_iter_update_trigger_flags(flags.bits())) })
    }

    /// Flush the journal: everything committed before the call is on disk when
    /// it returns.
    pub fn journal_flush(&self) -> Result<(), BchError> {
        ret_to_result(unsafe { c::bch2_journal_flush(&mut (*self.raw).journal) })
    }

    /// Set @inum's i_size to @new_i_size and drop every extent past it, as a
    /// logged op, so a crash midway resumes rather than leaving extents past
    /// EOF. Block granular: zeroing the rest of the block @new_i_size falls
    /// in is the caller's job, as the VFS does it in the page cache.
    pub fn truncate(&self, inum: c::subvol_inum, new_i_size: u64) -> Result<(), BchError> {
        let mut i_sectors_delta = 0;
        ret_to_result(unsafe { c::bch2_truncate(self.raw, inum, new_i_size, &mut i_sectors_delta) })
    }

    /// EROFS if @subvol is a read-only subvolume (a read-only snapshot).
    pub fn subvol_is_ro(&self, subvol: u32) -> Result<(), BchError> {
        ret_to_result(unsafe { c::bch2_subvol_is_ro(self.raw, subvol) })
    }

    /// Make the filesystem incompatible with versions before @version, if
    /// it isn't already - an error if that isn't allowed: as
    /// bch2_request_incompat_feature().
    pub fn request_incompat_feature(&self, version: c::bcachefs_metadata_version)
        -> Result<(), BchError>
    {
        if version.0 <= unsafe { (*self.raw).sb.version_incompat } as u32 {
            return Ok(());
        }
        ret_to_result(unsafe { c::bch2_set_version_incompat(self.raw, version) })
    }

    /// Whether the filesystem uses @feature: c->sb.features, the in-memory
    /// copy.
    pub fn feature(&self, feature: c::bch_sb_feature) -> bool {
        let features = unsafe { (*self.raw).sb.features };
        features & (1u64 << feature as u32) != 0
    }

    /// Mark the filesystem as using @feature, writing the superblock if it
    /// wasn't already: as bch2_check_set_feature().
    pub fn check_set_feature(&self, feature: c::bch_sb_feature) {
        if !self.feature(feature) {
            unsafe { c::__bch2_check_set_feature(self.raw, feature as u32) }
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

    /// Write superblock to disk (locked version). Caller must hold sb_lock.
    /// Returns Ok(()) on success or the error code on failure.
    pub fn write_super_ret(&self) -> Result<(), BchError> {
        ret_to_result(unsafe { c::bch2_write_super(self.raw) })
    }

    /// A superblock write that's part of bringing the filesystem up, on a
    /// nostart open that will start it afterwards - allowed before start,
    /// like recovery's own. Caller must hold sb_lock.
    pub fn write_super_bringup(&self) -> Result<(), BchError> {
        ret_to_result(unsafe {
            c::bch2_write_super_flags(self.raw, c::bch_sb_write_flags::BCH_SB_WRITE_bringup)
        })
    }

    /// Apply in-memory superblock edits to the in-memory state derived from
    /// it (c->sb, member info) without writing - for an open that will start
    /// and persist them then. Caller must hold sb_lock.
    pub fn sb_update(&self) {
        unsafe { c::bch2_sb_update(self.raw) }
    }

    /// Check if a device index exists and has a device pointer.
    pub fn dev_exists(&self, dev: u32) -> bool {
        unsafe {
            let c = &*self.raw;
            (dev as usize) < c.sb.nr_devices as usize
                && !c.devs[dev as usize].is_null()
        }
    }

    /// Transition filesystem to read-only mode.
    pub fn read_only(&self) {
        unsafe { c::bch2_fs_read_only(self.raw) };
    }

    /// Set a device's allocator to RW or RO.
    pub fn dev_allocator_set_rw(&self, dev: u32, rw: bool) {
        unsafe {
            let ca = (*self.raw).devs[dev as usize];
            if !ca.is_null() {
                c::bch2_dev_allocator_set_rw(self.raw, ca, rw);
            }
        }
    }

    /// Flush all journal pins (equivalent to bch2_journal_flush_all_pins).
    pub fn journal_flush_all_pins(&self) {
        unsafe {
            c::bch2_journal_flush_pins(
                &mut (*self.raw).journal,
                u64::MAX,
            );
        }
    }

    /// Flush pins up to the current journal sequence.
    pub fn journal_flush_outstanding_pins(&self) -> bool {
        unsafe { c::bch2_journal_flush_pins(&mut (*self.raw).journal, self.journal_cur_seq()) }
    }

    /// Delete a range of keys in a btree.
    pub fn btree_delete_range(
        &self,
        btree_id: c::btree_id,
        start: c::bpos,
        end: c::bpos,
        flags: BtreeIterFlags,
    ) -> Result<(), BchError> {
        ret_to_result(unsafe {
            c::bch2_btree_delete_range(self.raw, btree_id, start, end, c::btree_iter_update_trigger_flags(flags.bits()))
        })
    }

    pub fn btree_insert(
        &self,
        btree_id:     c::btree_id,
        key:          &mut impl AsBkeyI,
        disk_res:     Option<&DiskReservation<'_>>,
        commit_flags: impl Into<CommitOpts>,
        iter_flags:   BtreeIterFlags,
    ) -> Result<(), BchError> {
        let disk_res = disk_res
            .map(|r| r.as_mut_ptr())
            .unwrap_or(core::ptr::null_mut());

        ret_to_result(unsafe {
            c::bch2_btree_insert(
                self.raw,
                btree_id,
                key.as_bkey_i_mut(),
                disk_res,
                commit_flags.into().to_c(),
                c::btree_iter_update_trigger_flags(iter_flags.bits()),
            )
        })
    }

    /// Read full device usage stats.
    pub fn dev_usage_full_read(&self, dev: u32) -> c::bch_dev_usage_full {
        unsafe {
            let ca = (*self.raw).devs[dev as usize];
            let mut usage: c::bch_dev_usage_full = core::mem::zeroed();
            c::bch2_dev_usage_full_read_fast(ca, &mut usage);
            usage
        }
    }

    /// Get the raw device pointer by index.
    ///
    /// # Safety
    /// Caller must ensure the device exists and the pointer is valid.
    pub unsafe fn dev_raw(&self, dev: u32) -> *mut c::bch_dev {
        (*self.raw).devs[dev as usize]
    }

    /// Access the mutable superblock handle for resize operations.
    ///
    /// # Safety
    /// Caller must hold sb_lock.
    #[allow(clippy::mut_from_ref)] // interior mutability guarded by sb_lock
    pub unsafe fn disk_sb_mut(&self) -> &mut c::bch_sb_handle {
        &mut (*self.raw).disk_sb
    }

    /// Filesystem block size in bytes.
    pub fn block_bytes(&self) -> u64 {
        self.opts().block_size as u64
    }

    /// Convert a bcachefs internal time to a timespec.
    pub fn time_to_timespec(&self, time: i64) -> c::timespec64 {
        const NSEC_PER_SEC: i64 = 1_000_000_000;

        let sb = unsafe { &(*self.raw).sb };
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

    /// Convert a timespec to a bcachefs internal time.
    pub fn timespec_to_time(&self, ts: c::timespec64) -> i64 {
        let sb = unsafe { &(*self.raw).sb };
        // C's (int) tv_nsec / nsec_per_time_unit: an unsigned division
        (ts.tv_sec as i64).wrapping_mul(sb.time_units_per_sec as i64)
            .wrapping_add((ts.tv_nsec as i32 as u32 / sb.nsec_per_time_unit) as i64)
            .wrapping_sub(sb.time_base_lo as i64)
    }

    /// Current time in bcachefs internal time format.
    pub fn current_time(&self) -> u64 {
        let mut now: c::timespec64 = unsafe { core::mem::zeroed() };
        unsafe { c::bch2_ktime_get_coarse_real_ts64(&mut now) };
        self.timespec_to_time(now) as u64
    }

    /// Short filesystem usage summary.
    pub fn usage_read_short(&self) -> c::bch_fs_usage_short {
        unsafe { c::bch2_fs_usage_read_short(self.raw) }
    }

    /// Set the filesystem log level.
    pub fn set_loglevel(&self, level: u32) {
        unsafe { (*self.raw).loglevel = level; }
    }
}

impl Drop for Fs {
    fn drop(&mut self) {
        unsafe { c::bch2_fs_exit(self.raw); }
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
        unsafe { c::rust_enumerated_ref_tryget(&raw mut (*self.raw).writes, idx as u32) }
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
    pub fn queue_work(self, work: &Opaque<c::work_struct>) {
        if unsafe { c::bch2_queue_work((*self.fs.raw).write_ref_wq, work.as_ptr()) } {
            core::mem::forget(self);
        }
    }
}

impl Drop for WriteRef<'_> {
    fn drop(&mut self) {
        unsafe { c::rust_enumerated_ref_put(&raw mut (*self.fs.raw).writes, self.idx as u32) };
    }
}

// Standalone helpers — pure Rust reimplementations of C static inlines.

/// Sector offset of bucket `b` on device `ca`.
pub fn bucket_to_sector(ca: &c::bch_dev, b: u64) -> u64 {
    b * ca.mi.bucket_size as u64
}

/// Size of one bucket in bytes.
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

/// Check if a btree ID is an allocator btree.
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
