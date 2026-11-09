// SPDX-License-Identifier: GPL-2.0

//! Fs - struct bch_fs - its accessors and locks, and OpenFs, the handle that
//! exits a filesystem when dropped.

#![allow(non_camel_case_types)]

use crate::init::passes::RecoveryPass;
use crate::btree::node::BtreeNode;
use crate::c;
use crate::errcode::{bch_errcode, ret_to_c, BchError};
use crate::alloc::accounting_mem::accounting_exit;
use crate::alloc::foreground::allocator_foreground_init;
use crate::alloc::replicas::can_write_with_devs;
use crate::btree::init::{btree_exit, btree_init};
use crate::btree::journal_overlay::journal_keys_put_initial;
use crate::data::checksum::encryption_init;
use crate::data::compress::compress_init;
use crate::data::ec::head::ec_exit;
use crate::data::reconcile::work::reconcile_exit;
use crate::debug::sysfs::{FsDir, FS_GROUPS};
use crate::init::dev::{dev_exit, dev_in_fs};
use crate::init::error::free_fsck_errs;
use crate::init::registry::register;
use crate::init::rw::writes_disabled;
use crate::inode::shard_cpu_init;
use crate::journal::seq_blacklist::blacklist_table_initialize;
use crate::locking::{ExclusiveGuard, MutexNoio, Noio, PercpuRwsemNoio, RawLockBase, SharedGuard};
use crate::{bch_verbose, new_lock, prt};
use crate::sb::counters::counters_init;
use crate::sb::io::{read_super, sb_to_fs, version, Holder};
use crate::sb::members::{dev_to_text_short, fs_mi_field_upgrades, member_exists, SbMembersMut};
use crate::sb::version::prep_for_start;
use crate::util::names;
use crate::util::Printbuf;
use crate::util::alloc::{Arc, KVVec};
use crate::util::backref::BackRef;
use crate::util::enumerated_ref::EnumeratedRef;
use crate::util::ffi::c_char;
use crate::util::kernel::READ;
use crate::util::sched;
use crate::util::kobject::{GroupList, KobjOwner, Kobject};
use crate::util::log::{log_msg_start, print_lines, print_str, LogLevel, LogMsg};
use crate::util::percpu::PerCpu;
use crate::util::printbuf::prt_bdevname;
use crate::util::rcu::{read_lock, RcuPtrWriter};
use crate::util::refcount::{ARef, RefGuard, WaitableRef};
#[cfg(kernel)]
use crate::util::{place::Placed, refcount::AtomicRef};
use core::ffi::CStr;
use core::future::Future;
use core::mem::size_of;
use core::ptr::NonNull;
use core::sync::atomic::Ordering::Relaxed;
pub use crate::sb::members::DevRef;
use core::ffi::{c_int, c_uint};
use cstruct_macros::c_extern;
use crate::format::{Btree, MemberState, MetadataVersion, SbFeature};

c_extern! {
    pub static bch2_write_refs: [*const crate::util::ffi::c_char; 0];
}

/// sb_lock, held: the superblock, and c->sb, which it publishes
pub type SbLockGuard<'a> = ExclusiveGuard<'a, Noio<c::mutex>, c::bch_fs_sb, c::bch_sb_cpu>;

impl SbLockGuard<'_> {
    pub fn fs_member_mut(&mut self, dev_idx: u32) -> &mut c::bch_member {
        self.member_mut(dev_idx)
            .expect("a member, whole: bch2_sb_members_v2_init() grew them at open")
    }
}

pub struct StateLockRead<'a> {
    fs:     &'a Fs,
    _guard: SharedGuard<'a, c::rw_semaphore>,
}

pub struct StateLockWrite<'a> {
    fs:     &'a Fs,
    _guard: ExclusiveGuard<'a, c::rw_semaphore>,
}

impl<'a> StateLockWrite<'a> {
    pub(crate) fn fs(&self) -> &'a Fs {
        self.fs
    }

    /// c->devs[], as their writer has them.
    pub(crate) fn devs(&self) -> &[RcuPtrWriter<ARef<Dev>>; c::BCH_SB_MEMBERS_MAX as usize] {
        // state_lock is held, for as long as we are
        unsafe { self.fs.devs.published_writer() }
    }
}

impl StateLockRead<'_> {
    pub fn recalc_capacity(&self) {
        crate::alloc::capacity::recalc_capacity(self.fs)
    }
}

pub use crate::cstructs::types::Fs;
pub use crate::cstructs::types::Dev;

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
        crate::util::ffi::cstr_in(&self.name)
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

    /*
     * Shard index for inode-number allocation. We used to use the current CPU id,
     * but threads migrate across CPUs and the win from per-CPU allocator
     * separation evaporates — concurrent allocators end up sharing shards (and
     * fighting on the same alloc_cursor btree node) any time the scheduler
     * shuffles them onto the same core. Hashing the task's pid is stable per
     * thread, so concurrent allocators in different threads keep their separation
     * regardless of which CPU they're currently running on.
     */
    pub fn inode_shard_idx(&self) -> u64 {
        let bits = self.opts().shard_inode_numbers_bits.get();
        if bits == 0 {
            return 0;
        }
        let pid = unsafe { crate::util::percpu::bch2_task_pid(sched::current()) };
        crate::util::hash::hash_64(pid as u64, bits.into())
    }

    /// Never unset once set
    pub fn encryption_key(&self) -> Option<&c::bch_key> {
        self.sb.encryption_key.get()
    }

    /// # Safety
    /// @raw points to a live bch_fs, that outlives 'a.
    pub unsafe fn borrow_raw<'a>(raw: *mut Fs) -> &'a Fs {
        unsafe { &*raw }
    }

    pub fn from_c(c: &crate::util::ffi::Opaque<Fs>) -> &Fs {
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

    pub fn version(&self) -> MetadataVersion {
        MetadataVersion(self.sb.version.get() as u32)
    }

    /// Below version() while an upgrade's migrations are still running
    pub fn version_upgrade_complete(&self) -> MetadataVersion {
        MetadataVersion(self.sb.version_upgrade_complete.get() as u32)
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

    /// A mount is reading recovery progress from BCH_IOCTL_RECOVERY_STATUS,
    /// so progress indicators needn't print it
    pub fn stdio_progress_reader(&self) -> bool {
        self.stdio_progress_reader.load(Relaxed)
    }

    pub fn journal_cur_seq(&self) -> u64 {
        self.journal.lock.seq.load(core::sync::atomic::Ordering::Relaxed) as u64
    }

    pub fn sb_lock(&self) -> SbLockGuard<'_> {
        self.sb.lock()
    }

    pub fn holder(&self) -> &Arc<Holder> {
        self.holder.as_ref().expect("a filesystem's holder, from alloc()")
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

    pub fn btree_id_root(&self, id: u32) -> Option<&BtreeNode> {
        self.btree.cache.roots.node(id)
    }

    /// Btree @id's root error, published as the root is.
    pub fn btree_id_root_set_error(&self, id: u32, err: c_int) {
        self.btree.cache.roots.lock().published().roots_error[id as usize].set(err as i16);
    }

    pub fn btree_id_nr_alive(&self) -> u32 {
        Btree::NR + self.btree.cache.roots.roots_nr_extra.get()
    }

    pub fn nr_devices(&self) -> u32 {
        self.sb.nr_devices.get() as u32
    }

    /// Everything committed before the call is on disk when it returns
    pub fn journal_flush(&self) -> Result<(), BchError> {
        self.journal.flush()
    }

    /// A logged op, resumed after a crash. Block granular: zeroing the rest of
    /// the last block is the caller's, as the VFS does in the page cache.
    pub fn truncate(&self, inum: c::subvol_inum, new_i_size: u64) -> Result<(), BchError> {
        crate::data::io_misc::truncate(self, inum, new_i_size, &mut 0)
    }

    pub fn request_incompat_feature(&self, version: MetadataVersion)
        -> Result<(), BchError>
    {
        if version.0 <= self.sb.version_incompat.get().into() {
            return Ok(());
        }
        self.set_version_incompat(version)
    }

    pub fn feature(&self, feature: SbFeature) -> bool {
        self.sb.features.get() & (1u64 << feature.0) != 0
    }

    pub fn check_set_feature(&self, feature: SbFeature) {
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

    pub fn journal_flush_all_pins(&self) -> bool {
        self.journal.flush_pins(u64::MAX)
    }

    pub fn journal_flush_outstanding_pins(&self) -> bool {
        self.journal.flush_pins(self.journal_cur_seq())
    }

    /// # Safety
    /// The device exists.
    pub unsafe fn dev_raw(&self, dev: u32) -> *mut Dev {
        self.devs[dev as usize].as_ptr()
    }

    pub fn block_bytes(&self) -> u64 {
        self.opts().block_size.get() as u64
    }

    pub fn block_sectors(&self) -> u32 {
        u32::from(self.opts().block_size.get()) >> 9
    }

    pub fn btree_sectors(&self) -> u32 {
        self.opts().btree_node_size.get() >> 9
    }

    pub fn btree_max_u64s(&self) -> usize {
        crate::btree::types::btree_max_u64s(self.opts().btree_node_size.get())
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
        self.timespec_to_time(crate::util::kernel::ktime_get_coarse_real_ts64()) as u64
    }

    pub fn set_loglevel(&self, level: u32) {
        self.loglevel.store(level, Relaxed);
    }
}

/// The filesystem @sbs are the devices of, with options @opts: Fs::init()'s
/// fields, the rest from init_from(). It takes the devices it attaches,
/// leaving their handles zeroed; on failure, it's exited.
fn alloc(sbs: &mut KVVec<c::bch_sb_handle>, opts: &c::bch_opts, holder: Arc<Holder>,
         user_key: Option<&c::bch_key>, out: &mut Printbuf) -> Result<OpenFs, BchError> {
    let c = crate::util::alloc::alloc_pinned_or(Fs::init(opts), bch_errcode::BCH_ERR_ENOMEM_fs_alloc)?;
    let fs = unsafe {
        *(*c).holder = Some(holder);
        OpenFs::from_raw(NonNull::new_unchecked(c))
    };
    fs.init_from(sbs, opts, user_key, out)?;
    Ok(fs)
}

/// The default encoding, utf8 12.1.0: UNICODE_AGE()'s major, minor and rev
/// bytes
#[cfg(CONFIG_UNICODE)]
const BCH_FS_DEFAULT_UTF8_ENCODING: c_uint = (12 << 16) | (1 << 8);

impl Fs {
    fn init_from(&self, sbs: &mut KVVec<c::bch_sb_handle>, opts: &c::bch_opts, user_key: Option<&c::bch_key>,
                 out: &mut Printbuf) -> Result<(), BchError> {
        let p = self.as_ptr();

        unsafe {
            (*p).stdio.store(opts.stdio as usize as *mut c::stdio_redirect, Relaxed);
            (*p).recovery_task.store(sched::current(), Relaxed);
        }

        #[cfg(kernel)]
        unsafe { c::__module_get(&raw mut kernel::bindings::__this_module) };

        unsafe {
            c::closure_init(&raw mut (*p).cl, core::ptr::null_mut());

            (*p).minor = -1;

            let Ok(()) = c::mutex::init(&raw mut (*p).opt_change_lock, c"&c->opt_change_lock", new_lock!(@key));
        }

        allocator_foreground_init(self);
        unsafe {
            let Ok(()) = c::spinlock_t::init(&raw mut (*p).write_error_lock, c"&c->write_error_lock",
                                             new_lock!(@key));
        }

        /* must be initialized before we throw any errors */
        let now = PerCpu::alloc().map_err(|_| BchError::from(bch_errcode::BCH_ERR_ENOMEM_fs_counters_init))?;
        unsafe { (*p).counters.now = now };

        {
            let mut sb = self.sb_lock();
            sb_to_fs(self, &mut sb, sbs[0].io.buf.sb())?;
            self.sb_resize(&mut sb).members_v2_init()
                .ok_or_else(|| self.err(bch_errcode::BCH_ERR_ENOSPC_sb_members_v2))?;
        }

        shard_cpu_init(self);

        let mut name = Printbuf::new();
        if self.sb.multi_device.get() {
            prt!(&mut name, "{}", self.sb.user_uuid());
        } else {
            prt_bdevname(&mut name, Some(sbs[0].dev.bdev()));
        }
        if name.allocation_failure() {
            return Err(bch_errcode::BCH_ERR_ENOMEM_fs_name_alloc.into());
        }
        // strscpy()
        let dst = unsafe { &mut (*p).name };
        let len = name.as_bytes().len().min(dst.len() - 1);
        for (d, &s) in dst.iter_mut().zip(&name.as_bytes()[..len]) {
            *d = s as c_char;
        }
        dst[len] = 0;

        if unsafe { self.writes.init(c::bch_write_ref::NR, Some(writes_disabled)) }.is_err() {
            return self.throw(bch_errcode::BCH_ERR_ENOMEM_fs_other_alloc);
        }

        blacklist_table_initialize(self)?;
        crate::debug::async_objs::lists::async_obj_init(self)?;
        unsafe { self.btree.cache.nodes.init() }?;
        unsafe { self.btree.key_cache.keys.init() }?;
        btree_init(self)?;
        compress_init(self)?;
        counters_init(self);
        encryption_init(self, user_key)?;
        #[cfg(not(NO_BCACHEFS_FS))]
        unsafe { self.vfs.inodes.init() }?;

        #[cfg(CONFIG_UNICODE)]
        if self.casefold_enabled().is_ok() {
            /* Default encoding until we can potentially have more as an option. */
            let enc = unsafe { c::utf8_load(BCH_FS_DEFAULT_UTF8_ENCODING) };
            match NonNull::new(enc).map(crate::errcode::errptr_to_result) {
                Some(Ok(enc)) => unsafe { (*p).cf_encoding = enc.as_ptr() },
                _ => {
                    prt!(out, "Cannot load UTF-8 encoding for filesystem. Version: {}.{}.{}\n",
                         BCH_FS_DEFAULT_UTF8_ENCODING >> 16,
                         (BCH_FS_DEFAULT_UTF8_ENCODING >> 8) & 0xff,
                         BCH_FS_DEFAULT_UTF8_ENCODING & 0xff);
                    return self.throw(bch_errcode::BCH_ERR_EINVAL_utf8_load_failed);
                }
            }
        }
        #[cfg(not(CONFIG_UNICODE))]
        if self.feature(SbFeature::casefolding) {
            prt!(out, "Cannot mount a filesystem with casefolding on a kernel without CONFIG_UNICODE\n");
            return self.throw(bch_errcode::BCH_ERR_EINVAL_casefolding_no_unicode);
        }

        for i in 0..self.nr_devices() {
            if !member_exists(self.sb_lock().sb(), i) {
                continue;
            }
            self.dev_alloc(i)?;
        }

        self.journal_entry_res_resize(&self.btree.root_journal_res,
            crate::btree::interior::roots_journal_u64s(c::BTREE_ID_NR));
        self.journal_entry_res_resize(&self.super_entries_journal_res,
            crate::journal::write::PREP_ENTRIES_U64S as c_uint);
        self.journal_entry_res_resize(&self.rewind_limit_res,
            (size_of::<c::jset_entry_rewind_limit>() / size_of::<u64>()) as c_uint);

        {
            let state = self.state_lock_write();
            for sb in sbs.iter_mut() {
                state.dev_attach_bdev(sb, out)?;
            }
        }

        if self.opts().no_version_check.get() == 0 {
            /*
             * Do this early, so that we never expose a filesystem object that
             * hasn't been version downgraded
             */
            self.opt_version_init(out)?;
        }

        if cfg!(CONFIG_BCACHEFS_DEBUG) {
            prt!(out, "*** DEBUG BUILD ***\n");
        }

        /* first: a UUID already open fails as that, not as a sysfs name taken */
        register(self)?;
        self.online()
    }

    #[inline(never)]
    fn opt_version_init(&self, out: &mut Printbuf) -> Result<(), BchError> {
        use c::bch_fs_flags as Flag;
        let opts = self.opts();

        {
            let mut sb = self.sb_lock();
            let w = self.opts_writer(&mut sb);

            if opts.norecovery.get() != 0 {
                let snapshots_read = RecoveryPass::snapshots_read as _;
                w.recovery_pass_last.set(match opts.recovery_pass_last.get() {
                    0    => snapshots_read,
                    last => last.min(snapshots_read),
                });
                w.nochanges.set(1);
            }

            if opts.nochanges.get() != 0 {
                w.read_only.set(1);
            }

            if opts.will_not_start.get() != 0 {
                w.nostart.set(1);
            }

            if opts.journal_rewind.get() != 0 {
                w.fsck.set(1);
            }
        }

        if !(self.feature(SbFeature::small_image) || self.feature(SbFeature::no_default_sb)) ||
           self.will_resize_on_mount() {
            self.flags.set(Flag::may_upgrade_downgrade, true);
        }

        prt!(out, "starting version {}\n", version(self.sb.version.get()));

        let mut mount_opts = Printbuf::new();
        crate::opts::opts_to_text(&mut mount_opts, &opts.load(), Some(self), Some(self.sb_lock().sb().as_unvalidated()),
                                  None, c::opt_flags::OPT_MOUNT.bits(), 0, c::OPT_SHOW_MOUNT_STYLE);
        if !mount_opts.as_bytes().is_empty() {
            prt!(out, "with options: ");
            out.write_bytes(mount_opts.as_bytes());
            out.newline();
        }

        if self.sb.version_incompat_allowed.get() != self.sb.version.get() {
            prt!(out, "allowing incompatible features up to {}\n", version(self.sb.version_incompat_allowed.get()));
        }

        if opts.verbose.get() != 0 {
            prt!(out, "features: {}\n", names::set::<SbFeature>(self.sb.features.get()));
        }

        if self.sb.multi_device.get() {
            prt!(out, "with devices: ");
            for (i, ca) in self.online_members().enumerate() {
                if i > 0 {
                    out.write_bytes(b",");
                }
                out.write_bytes(ca.name().to_bytes());
            }
            out.newline();
        }

        /* cf_encoding log message should be here, but it breaks xfstests - sigh */

        if opts.journal_rewind.get() != 0 {
            prt!(out, "rewinding journal, fsck required\n");
        }

        if opts.will_not_start.get() == 0 {
            prep_for_start(self, out);
        }

        if self.sb.clean.get() != 0 {
            self.flags.set(Flag::clean_recovery, true);
        }
        if opts.fsck.get() != 0 {
            self.flags.set(Flag::in_fsck, true);
        }
        self.flags.set(Flag::in_recovery, true);

        /* this really should be part of our one multi line mount message, but -
         * xfstests... */
        #[cfg(CONFIG_UNICODE)]
        if !self.cf_encoding.is_null() {
            crate::bch_info!(self, "Using encoding defined by superblock: utf8-{}.{}.{}",
                      BCH_FS_DEFAULT_UTF8_ENCODING >> 16,
                      (BCH_FS_DEFAULT_UTF8_ENCODING >> 8) & 0xff,
                      BCH_FS_DEFAULT_UTF8_ENCODING & 0xff);
        }

        if self.sb_lock().sb().initialized() {
            if !self.feature(SbFeature::new_extent_overwrite) {
                prt!(out, "feature new_extent_overwrite not set, filesystem no longer supported\n");
                return self.throw(bch_errcode::BCH_ERR_EINVAL_missing_new_extent_overwrite);
            }
        }

        fs_mi_field_upgrades(self);

        Ok(())
    }

    fn online(&self) -> Result<(), BchError> {
        #[cfg(kernel)]
        self.chardev_init()?;

        #[cfg(CONFIG_DEBUG_FS)]
        crate::debug::debug::debug_init(self);

        let mut name = Printbuf::new();
        if self.sb.multi_device.get() {
            prt!(&mut name, "{}", self.sb.user_uuid());
        } else {
            name.write_bytes(self.name().to_bytes());
        }
        let parent = &crate::init::module::BCACHEFS_KOBJ.0;
        if name.allocation_failure() || self.kobj.add(Some(parent), name.as_c_str()).is_err() {
            return self.throw(bch_errcode::BCH_ERR_sysfs_init_error);
        }

        let _state = self.state_lock_write();
        for ca in self.members() {
            if ca.sysfs_online().is_err() {
                return self.throw(bch_errcode::BCH_ERR_sysfs_init_error);
            }
        }
        Ok(())
    }
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

        let mut sbs = KVVec::with_capacity(devs.len())?;
        for path in devs {
            sbs.push(read_super(path, opts, Some(holder))?)?;
        }
        filter_dead(&mut sbs, opts, Some(out))?;
        Ok(sbs)
    }

    pub fn into_raw(self) -> core::ptr::NonNull<Fs> {
        core::mem::ManuallyDrop::new(self).0
    }

    /// Drop discards the error
    pub fn exit(self) -> i32 {
        ret_to_c(unsafe { exit(self.into_raw()) })
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

        sbs[0].set_compat_defaults()?;
        let fs_opts = crate::opts::fs_opts(sbs[0].sb().as_unvalidated(), opts, out)?;

        let fs = alloc(sbs, &fs_opts, self.holder.clone(), key, out)?;
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
        let _ = unsafe { exit(self.0) };
    }
}

impl Fs {
    pub fn stop(&self) -> Result<(), BchError> {
        use c::bch_fs_flags as Flag;
        let p = self.as_ptr();

        if !self.flags.swap(Flag::stopping, true) {
            self.ro_ref.stop();
            self.ro_ref_events.wake();

            if self.flags.get(Flag::started) {
                bch_verbose!(self, "shutting down");
            }

            self.state_lock_write().read_only();

            for i in 0..self.nr_devices() as usize {
                let ca = self.state_lock_write().devs()[i].get();
                if let Some(ca) = ca {
                    ca.io_ref_stop(READ);
                }
            }

            for ca in self.members() {
                ca.unlink();
            }

            self.kobj.del();

            #[cfg(CONFIG_DEBUG_FS)]
            crate::debug::debug::debug_exit(self);
            #[cfg(kernel)]
            self.chardev_exit();

            self.ro_ref.wait();

            /* btree prefetch might have kicked off reads in the background: */
            self.btree_flush_all_reads();

            unsafe { c::flush_work(&raw mut (*p).btree.interior_updates.work) };
        }

        if self.flags.get(Flag::emergency_ro) {
            return self.throw(bch_errcode::BCH_ERR_shutdown_with_emergency_ro);
        }
        if self.flags.get(Flag::error) {
            return self.throw(bch_errcode::BCH_ERR_shutdown_with_errors_unfixed);
        }
        if self.flags.get(Flag::errors_fixed) {
            return self.throw(bch_errcode::BCH_ERR_shutdown_with_errors_fixed);
        }
        Ok(())
    }
}

/// Stopped, its devices freed, and its kobject's last ref put: its release
/// frees it.
///
/// # Safety
/// @p is the caller's to exit: nothing uses it after.
unsafe fn exit(p: NonNull<Fs>) -> Result<(), BchError> {
    let fs = unsafe { p.as_ref() };
    let ret = fs.stop();

    crate::init::registry::deregister(fs);

    unsafe { c::closure_sync(&raw mut (*p.as_ptr()).cl) };

    for i in 0..fs.nr_devices() as usize {
        // freed outside state_lock: dev_free() drains ref_outer, whose holders
        // may be waiting on it
        let ca = fs.state_lock_write().devs()[i].take();
        // stopped: no readers
        if let Some(ca) = ca {
            dev_exit(ca);
        }
    }
    // their drops are a grace period on, and need what the filesystem frees
    crate::util::maybe_rcu_alloc::barrier();

    bch_verbose!(fs, "shutdown complete");

    unsafe { Kobject::put((&raw const (*p.as_ptr()).kobj).cast()) };
    ret
}

unsafe impl KobjOwner for Fs {
    const GROUPS: GroupList = FS_GROUPS.list();
    type Dir = FsDir;

    unsafe fn release(this: NonNull<Fs>) {
        unsafe { release(this.as_ptr()) }
    }
}

/// # Safety
/// @p's kobject's last ref is put.
unsafe fn release(p: *mut Fs) {
    let fs = unsafe { &*p };

    journal_keys_put_initial(fs);

    #[cfg(CONFIG_UNICODE)]
    unsafe { c::utf8_unload(fs.cf_encoding) };

    fs.reconcile_stop();
    fs.copygc_stop();
    fs.btree_write_buffer_stop();
    fs.btree.async_ops.free_pending();
    free_fsck_errs(fs);
    // each workqueue's destroy is its flush: before the exits free what their
    // work uses
    unsafe { fs.exit_late() };
    #[cfg(not(NO_BCACHEFS_FS))]
    unsafe { fs.vfs.inodes.exit() };
    reconcile_exit(fs);
    unsafe { c::journal::exit(&raw mut (*p).journal) };
    ec_exit(fs);
    fs.counters_recent.cancel_sync();
    unsafe {
        (*p).counters.now.free();
        btree_exit(p);
        fs.btree.cache.nodes.exit();
        fs.btree.key_cache.keys.exit();
        accounting_exit(fs);
    }
    crate::debug::async_objs::lists::async_obj_exit(fs);
    unsafe {
        fs.writes.exit();
        fs.disk_groups.free();
        crate::util::alloc::free_pinned(p);
    }
    #[cfg(kernel)]
    unsafe { c::module_put(&raw mut kernel::bindings::__this_module) };
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

    let mut dead = KVVec::from_elem(false, sbs.len())?;
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
        self.recovery_task.store(core::ptr::null_mut(), Relaxed);
        ret
    }

    fn bring_up(&self, err: &mut Printbuf) -> Result<(), BchError> {
        use c::bch_fs_flags as Flag;
        assert!(!self.flags.get(Flag::started));

        // the open skipped what a start needs: opt_version_init()
        if self.opts().will_not_start.get() != 0 {
            return self.throw(bch_errcode::BCH_ERR_EINVAL_will_not_start);
        }
        // superblock writes from here on are part of bringing it up
        self.flags.set(Flag::start_begun, true);

        {
            let state = self.state_lock_write();
            let rcu = read_lock();
            for ca in self.online_devs_rcu(&rcu) {
                if ca.mi.get().state == MemberState::rw {
                    state.dev_allocator_add(ca);
                }
            }
            drop(rcu);
            crate::alloc::capacity::recalc_capacity(self);
        }

        self.may_start(err)?;

        crate::data::reconcile::work::reconcile_init(self)?;
        crate::sb::counters::RecentCounters::queue(self);

        // now, so it's persisted before user-data accounting fires: a fresh
        // filesystem shut down before then would never record it, and its next
        // mount would find replicas missing from the superblock
        let _ = self.request_incompat_feature(MetadataVersion::no_sb_user_data_replicas);

        // allocated now if it may be needed: a mount failing on
        // kthread_create() is very annoying
        if crate::init::passes::go_rw_in_recovery(self) {
            self.state_lock_write().init_rw()?;
        }

        // some options can only be checked once started
        crate::opts::opts_hooks_pre_set(self)?;

        let initialized = self.sb_lock().sb().initialized();
        if initialized { crate::init::recovery::fs_recovery(self) } else { crate::init::recovery::fs_initialize(self) }?;

        crate::opts::opts_hooks_pre_set(self)?;

        self.flags.set(Flag::started, true);
        self.ro_ref_events.wake();

        let state = self.state_lock_write();
        if self.opts().read_only.get() != 0 {
            state.read_only();
        } else if !self.flags.get(Flag::rw) {
            state.read_write()?;
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
                   (ca.mi.get().state != MemberState::evacuating ||
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

/// While one is held, the filesystem can't finish going read-only
pub type WriteRef = RefGuard<EnumeratedRef>;

impl Fs {
    pub fn write_refs_to_text(&self, out: &mut Printbuf) {
        // SAFETY: bch2_write_refs names c->writes' kinds
        unsafe { self.writes.to_text(out, c::bch2_write_refs.as_ptr()) }
    }

    /// For work holding a write ref: going read-only drains it
    pub fn write_ref_wq(&self) -> &crate::util::workqueue::Queue {
        &self.write_ref_wq
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
        let (fs, wq) = unsafe { (BackRef::new(self), self.write_ref_wq()) };
        // SAFETY: what the future has is 'static, or the BackRef above's
        unsafe {
            crate::util::task::spawn_detached(crate::util::task::Spawn::Queue(wq), async move {
                let _w = w;
                make(fs).await
            })
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
                    $crate::util::workqueue::DelayedWork::new(
                        Self::work_fn,
                        const { $crate::util::ffi::c_str(concat!(stringify!($name), "\0")) },
                        ::kernel::static_lock_class!(),
                        const { $crate::util::ffi::c_str(concat!(stringify!($name), ".timer\0")) },
                        ::kernel::static_lock_class!())
                }
                #[cfg(not(kernel))]
                $crate::util::workqueue::DelayedWork::new(Self::work_fn)
            }

            $vis fn run($fs: &$crate::fs::Fs) $body

            /// # Safety
            /// @work is the field's.
            unsafe extern "C" fn work_fn(work: *mut $crate::c::work_struct) {
                let c = unsafe {
                    work.byte_sub(::core::mem::offset_of!($crate::c::delayed_work, work))
                        .byte_sub(::core::mem::offset_of!($crate::fs::Fs, $($field).+))
                        .cast::<$crate::fs::Fs>()
                };
                Self::run(unsafe { $crate::fs::Fs::borrow_raw(c) })
            }
        }
    };
}

#[cfg(kernel)]
pub(crate) type RoRef = RefGuard<Placed<AtomicRef<()>, crate::at!(Fs.ro_ref)>>;

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

/* We can reconstruct these btrees from information in other btrees */
pub fn btree_id_can_reconstruct(id: u32) -> bool {
    btree_id_is_alloc(id) ||
        matches!(
            Btree::from_raw(id),
            Some(Btree::snapshot_trees
                | Btree::deleted_inodes
                | Btree::reconcile_work
                | Btree::reconcile_hipri
                | Btree::reconcile_pending
                | Btree::reconcile_scan
                | Btree::subvolume_children)
        )
}

/*
 * We can reconstruct BTREE_ID_alloc, but reconstucting it from scratch is not
 * so cheap and OOMs on huge filesystems (until we have online
 * check_allocations)
 */
pub fn btree_id_recovers_from_scan(btree: Btree) -> bool {
    btree == Btree::alloc || !btree_id_can_reconstruct(btree.0)
}
