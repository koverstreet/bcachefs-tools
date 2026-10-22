use crate::c;
use crate::btree::bkey::BkeyI;
use crate::errcode::{bch_errcode, ret_to_result_void as ret_to_result, BchError};
use crate::alloc::buckets::DiskReservation;
use crate::btree::iter::{BtreeIterFlags, CommitOpts};
use crate::locking::{Guard, Lock, MutexNoio, Noio, PercpuRwsemNoio};
use crate::util::Printbuf;

pub use crate::sb::members::DevRef;

/// sb_lock held: the superblock, through it - and c->sb, through its
/// published().
pub type SbLockGuard<'a> = Guard<'a, Noio<c::mutex>, c::bch_fs_sb, c::bch_sb_cpu>;

impl SbLockGuard<'_> {
    /// Member @dev_idx, of an open filesystem's superblock - where every one
    /// is whole: bch2_sb_members_v2_init() grew them at open.
    pub fn fs_member_mut(&mut self, dev_idx: u32) -> &mut c::bch_member {
        self.member_mut(dev_idx)
            .expect("a member, whole: bch2_sb_members_v2_init() grew them at open")
    }
}

/// state_lock, held for read; up_read() on drop. What needs it held is a
/// method here.
pub struct StateLockRead<'a>(&'a Fs);

impl Drop for StateLockRead<'_> {
    fn drop(&mut self) {
        unsafe { c::up_read((&raw mut (*self.0.as_ptr()).state_lock).cast()) };
    }
}

impl StateLockRead<'_> {
    /// Recompute the filesystem's capacity from its devices.
    pub fn recalc_capacity(&self) {
        unsafe { c::bch2_recalc_capacity(self.0.as_ptr()) }
    }
}

pub use crate::cstructs::types::Fs;

impl Fs {
    /// For C that takes the filesystem.
    pub fn as_ptr(&self) -> *mut Fs {
        self as *const Fs as *mut Fs
    }
}

// SAFETY: the filesystem is every thread's: C shares a bch_fs with all of
// them, its state behind its own locks, atomics and RCU - so a &Fs goes to
// any thread too. What Rust reads through it is as synchronized as the C it
// reads: that's each field's, as in C.
unsafe impl Sync for Fs {}

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

    /// The filesystem whose disk_sb @sb is, C's container_of(), and its
    /// sb_lock, which C holds: for C passing &c->disk_sb.
    ///
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

    /// The metadata version the filesystem is at: C's c->sb.version, the
    /// in-memory copy, which is what decides version-gated behaviour.
    pub fn version(&self) -> c::bcachefs_metadata_version {
        c::bcachefs_metadata_version(self.sb.version.get() as u32)
    }

    /// The version the last upgrade completed to: c->sb.version_upgrade_complete.
    /// Below version() while an upgrade's migrations are still running.
    pub fn version_upgrade_complete(&self) -> c::bcachefs_metadata_version {
        c::bcachefs_metadata_version(self.sb.version_upgrade_complete.get() as u32)
    }

    /// Whether btree @id lost data - topology repair dropped nodes - so
    /// repairs may reconstruct what it's missing: c->sb.btrees_lost_data.
    pub fn btree_lost_data(&self, id: c::btree_id) -> bool {
        self.sb.btrees_lost_data.get() & (1u64 << id.0) != 0
    }

    /// Whether this is an image made with `bcachefs dump --sanitize`, which
    /// scrubs dirent names without rehashing them: c->sb.dirents_sanitized.
    pub fn dirents_sanitized(&self) -> bool {
        self.sb.dirents_sanitized.get()
    }

    /// Whether filesystem flag @f (BCH_FS_*) is set: C's test_bit() on
    /// c->flags. Other threads set and clear them, so it's an atomic load.
    pub fn flag(&self, f: c::bch_fs_flags) -> bool {
        self.flags().load(core::sync::atomic::Ordering::Relaxed) & (1usize << f as u32) != 0
    }

    /// Set filesystem flag @f: C's set_bit(), which orders nothing.
    pub fn set_flag(&self, f: c::bch_fs_flags) {
        self.flags().fetch_or(1usize << f as u32, core::sync::atomic::Ordering::Relaxed);
    }

    /// Clear filesystem flag @f: C's clear_bit().
    pub fn clear_flag(&self, f: c::bch_fs_flags) {
        self.flags().fetch_and(!(1usize << f as u32), core::sync::atomic::Ordering::Relaxed);
    }

    /// c->flags, which C changes with set_bit() and the like.
    fn flags(&self) -> &core::sync::atomic::AtomicUsize {
        // SAFETY: an unsigned long, which C's bitops change atomically
        unsafe { &*(core::ptr::addr_of!((*self.as_ptr()).flags) as *const core::sync::atomic::AtomicUsize) }
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
        self.journal.seq.load(core::sync::atomic::Ordering::Relaxed) as u64
    }

    /// Go emergency read-only: the journal halts, so nothing more commits,
    /// and the rest of going read-only is queued. As
    /// bch2_fs_emergency_read_only(). Whether this call is what did it - if
    /// so, @out says so, and is no longer suppressed.
    pub fn emergency_read_only(&self, out: &mut Printbuf) -> bool {
        unsafe { c::bch2_fs_emergency_read_only(self.as_ptr(), out.as_raw()) }
    }

    pub fn sb_lock(&self) -> SbLockGuard<'_> {
        self.sb.lock()
    }

    /// sb_lock, as C calling in holds it.
    ///
    /// # Safety
    /// The caller holds sb_lock, for as long as the guard lives.
    pub unsafe fn sb_lock_held(&self) -> SbLockGuard<'_> {
        unsafe { self.sb.held() }
    }

    /// c->capacity.mark_lock, C's - C initializes and frees it.
    pub fn mark_lock(&self) -> &PercpuRwsemNoio {
        Lock::from_c(&self.capacity.mark_lock)
    }

    pub fn mark_lock_assert_held(&self) {
        self.mark_lock().assert_held_any()
    }

    /// The BCH_FS_* flags set.
    pub fn flags_to_text(&self, out: &mut Printbuf) {
        unsafe { out.prt_bitflags(c::bch2_fs_flag_strs.as_ptr(), self.flags().load(core::sync::atomic::Ordering::Relaxed) as u64) }
    }

    /// The write refs held, by kind.
    pub fn write_refs_to_text(&self, out: &mut Printbuf) {
        unsafe { self.writes.to_text(out, c::bch2_write_refs.as_ptr()) }
    }

    /// state_lock for read: devices and capacity don't change under it.
    pub fn state_lock_read(&self) -> StateLockRead<'_> {
        unsafe { c::down_read((&raw mut (*self.as_ptr()).state_lock).cast()) };
        StateLockRead(self)
    }

    /// It changes the superblock - its seq, its checksum: so nothing in it is
    /// borrowed across the write.
    #[track_caller]
    pub fn write_super(&self, sb: &mut SbLockGuard<'_>) {
        let _ = self.write_super_ret(sb);
    }

    /// write_super(), for a write whose only reason is a replicas entry coming
    /// or going: it may leave the slow disks behind - bch2_write_super_replicas().
    #[track_caller]
    pub fn write_super_replicas(&self, sb: &mut SbLockGuard<'_>) {
        let _ = self.write_super_replicas_ret(sb);
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

    /// Btree @id's root: a known btree's, or past them, roots_extra's -
    /// bch2_btree_id_root(). None for one past those. An extra root moves when
    /// roots_extra grows, which takes root_lock: as C's, it's for a caller
    /// holding that, or before anything can grow it.
    pub fn btree_root(&self, id: u32) -> Option<&c::btree_root> {
        let cache = &self.btree.cache;
        let nr_known = u32::from(c::btree_id::nr);

        if id < nr_known {
            return Some(&cache.roots_known[id as usize]);
        }
        let idx = (id - nr_known) as usize;
        // SAFETY: roots_extra's nr are its elements; see above
        (idx < cache.roots_extra.nr).then(|| unsafe { &*cache.roots_extra.data.add(idx) })
    }

    pub fn btree_id_root(&self, id: u32) -> Option<&c::btree> {
        // SAFETY: a root's node, live while it's the root
        self.btree_root(id).and_then(|r| unsafe { r.b.as_ref() })
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
        self.sb.nr_devices.get() as u32
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
        if version.0 <= self.sb.version_incompat.get().into() {
            return Ok(());
        }
        self.set_version_incompat(version)
    }

    /// Whether the filesystem uses @feature: c->sb.features, the in-memory
    /// copy.
    pub fn feature(&self, feature: c::bch_sb_feature) -> bool {
        self.sb.features.get() & (1u64 << feature.0) != 0
    }

    /// Mark the filesystem as using @feature, writing the superblock if it
    /// wasn't already: as bch2_check_set_feature().
    pub fn check_set_feature(&self, feature: c::bch_sb_feature) {
        if !self.feature(feature) {
            self.set_feature(feature)
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

    #[track_caller]
    pub fn write_super_ret(&self, sb: &mut SbLockGuard<'_>) -> Result<(), BchError> {
        self.write_super_flags(sb, c::bch_sb_write_flags::empty())
    }

    /// A superblock write that's part of bringing the filesystem up, on a
    /// nostart open that will start it afterwards - allowed before start,
    /// like recovery's own.
    #[track_caller]
    pub fn write_super_bringup(&self, sb: &mut SbLockGuard<'_>) -> Result<(), BchError> {
        self.write_super_flags(sb, c::bch_sb_write_flags::BCH_SB_WRITE_bringup)
    }

    /// Apply in-memory superblock edits to the in-memory state derived from
    /// it (c->sb, member info) without writing - for an open that will start
    /// and persist them then.
    pub fn sb_update(&self, sb: &mut SbLockGuard<'_>) {
        crate::sb::io::sb_update(self, sb)
    }


    pub fn read_only(&self) {
        unsafe { c::bch2_fs_read_only(self.as_ptr()) };
    }

    pub fn dev_allocator_set_rw(&self, dev: u32, rw: bool) {
        let ca = self.devs[dev as usize].as_ptr();
        if !ca.is_null() {
            unsafe { c::bch2_dev_allocator_set_rw(self.as_ptr(), ca, rw) };
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

    /// @key, a validated key of any type, inserted in a transaction of its
    /// own.
    pub fn btree_insert<'k>(
        &self,
        btree_id:     c::btree_id,
        key:          impl Into<&'k mut BkeyI>,
        disk_res:     Option<&DiskReservation<'_>>,
        commit_flags: impl Into<CommitOpts>,
        iter_flags:   BtreeIterFlags,
    ) -> Result<(), BchError> {
        self.btree_insert_k_i(btree_id, key.into(), disk_res, commit_flags.into(), iter_flags)
    }

    /// btree_insert(), once: not monomorphized per key type.
    fn btree_insert_k_i(
        &self,
        btree_id:     c::btree_id,
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
        self.opts().block_size as u64
    }

    /// A btree node's size, in sectors: btree_sectors().
    pub fn btree_sectors(&self) -> u32 {
        self.opts().btree_node_size >> 9
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
            .then(|| WriteRef { fs: self, idx })
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
        // SAFETY: the filesystem's, for as long as it is
        let wq = unsafe { crate::util::workqueue::Queue::from_raw((*self.fs.as_ptr()).write_ref_wq) };
        self.queue_work_on(wq, work)
    }

    /// queue_work(), on @wq: one for long-running work - Queue::system_long()
    /// - that shouldn't hold up the filesystem's own.
    pub fn queue_work_on(self, wq: &crate::util::workqueue::Queue, work: &FsWork) {
        // SAFETY: FsWork is pinned and initialized; the filesystem it's in
        // outlives its run, which holds this ref
        if unsafe { wq.queue_work(work.as_ptr()) } {
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

// SAFETY: all zeroes is a work_struct as kvzalloc() leaves it, before
// INIT_WORK() - which a struct C initializes, bch_fs_recovery's, has
unsafe impl ::pin_init::Zeroable for FsWork {}

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

/// A delayed work item in bch_fs: one declaration for its function and its
/// initializer, as fs_work!() - but holding no write ref: it's stopped with
/// cancel_sync() going read-only, not drained.
///
/// ```text
/// fs_delayed_work! {
///     /// What it does
///     pub(crate) work BtreeBitmapGcSchedule: field maybe_schedule_btree_bitmap_gc;
///
///     fn run(fs: &Fs) { ... }
/// }
/// ```
///
/// is the unit struct BtreeBitmapGcSchedule, with init() - the field's
/// #[init(BtreeBitmapGcSchedule::init())] - and run(), run from the workqueue.
/// Queueing and cancelling are the field's, a DelayedWork.
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

impl Drop for WriteRef<'_> {
    fn drop(&mut self) {
        unsafe { c::rust_enumerated_ref_put(&raw mut (*self.fs.as_ptr()).writes, self.idx as u32) };
    }
}

// Standalone helpers — pure Rust reimplementations of C static inlines.

/// Build a hashed writepoint specifier (sets low bit to mark as hashed).
pub fn writepoint_hashed(v: usize) -> c::write_point_specifier {
    c::write_point_specifier { v: (v | 1) as _ }
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
