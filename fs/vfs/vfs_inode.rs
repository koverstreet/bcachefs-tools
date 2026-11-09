// SPDX-License-Identifier: GPL-2.0

//! The kernel's struct inode, for Rust: VfsInode, InodeInfo's @v. All Rust
//! reads and writes of the VFS inode are here.
//!
//! The VFS, update_after_write() and i_sectors_acct write its fields while
//! others read them, with no lock in common. Those read or written here
//! directly - mode, flags, generation, blocks, and reads of the ids and the
//! link count - are read and written whole, as READ_ONCE() and WRITE_ONCE(),
//! where C uses them plainly; the rest - the times, the size, writes of the
//! ids and link count - go through the kernel's helpers.
//!
//! Its reference count, i_count, is AlwaysRefCounted's - ihold(), iput() - so
//! an InodeRef, ARef<InodeInfo>, holds one.

use crate::c;
use crate::util::ffi::Opaque;
#[cfg(kernel)]
use {
    core::ffi::{c_int, c_long},
    core::ptr::{read_volatile, write_volatile, NonNull},
    crate::acl::{AclType, PosixAcl},
    crate::errcode::{ret_to_result_void, BchError},
    crate::util::kernel::loff,
    kernel::sync::aref::{ARef, AlwaysRefCounted},
    super::folio::Mapping,
    super::super_block::SuperBlock,
    crate::locking::{ExclusiveGuard, Locked},
};

/// A struct inode of ours - one InodeInfo's - referenced while this is
/// borrowed. Both builds lay it out; only the kernel's has a VFS.
#[repr(transparent)]
pub struct VfsInode(Opaque<c::inode>);

/// i_lock, held.
#[cfg(kernel)]
pub type ILocked<'a> = ExclusiveGuard<'a, c::spinlock_t>;

/// VfsInode's accessors that are a read or write and nothing else, as a
/// table: name() -> T, or name(T) to set, and how - a field read or written
/// whole, READ_ONCE() or WRITE_ONCE(), or the kernel helper that does it.
#[cfg(kernel)]
macro_rules! accessors {
    () => {};
    ($(#[$m:meta])* $name:ident() -> $t:ty = READ_ONCE($($f:ident).+); $($rest:tt)*) => {
        $(#[$m])* pub fn $name(&self) -> $t {
            unsafe { read_volatile(&raw const (*self.as_ptr()).$($f).+) }
        }
        accessors!($($rest)*);
    };
    ($(#[$m:meta])* $name:ident($t:ty) = WRITE_ONCE($($f:ident).+); $($rest:tt)*) => {
        $(#[$m])* pub fn $name(&self, v: $t) {
            unsafe { write_volatile(&raw mut (*self.as_ptr()).$($f).+, v) }
        }
        accessors!($($rest)*);
    };
    ($(#[$m:meta])* $name:ident() -> $t:ty = $helper:ident; $($rest:tt)*) => {
        $(#[$m])* pub fn $name(&self) -> $t {
            unsafe { c::$helper(self.as_ptr()) }
        }
        accessors!($($rest)*);
    };
    ($(#[$m:meta])* $name:ident($t:ty) = $helper:ident; $($rest:tt)*) => {
        $(#[$m])* pub fn $name(&self, v: $t) {
            unsafe { c::$helper(self.as_ptr(), v) };
        }
        accessors!($($rest)*);
    };
}

#[cfg(kernel)]
impl VfsInode {
    pub fn as_ptr(&self) -> *mut c::inode {
        self.0.as_ptr()
    }

    pub fn sb(&self) -> &SuperBlock {
        // SAFETY: set at allocation, to our superblock, which outlives it
        unsafe { &*(*self.as_ptr()).i_sb.cast::<SuperBlock>() }
    }

    accessors! {
        /// The user namespace its ids are stored in.
        user_ns() -> *mut c::user_namespace = i_user_ns;
        kuid() -> c::kuid_t                 = READ_ONCE(i_uid);
        kgid() -> c::kgid_t                 = READ_ONCE(i_gid);
        set_uid(u32)                        = i_uid_write;
        set_gid(u32)                        = i_gid_write;
        mode() -> c::umode_t                = READ_ONCE(i_mode);
        set_mode(c::umode_t)                = WRITE_ONCE(i_mode);
        generation() -> u32                 = READ_ONCE(i_generation);
        /// Changed under ei_quota_lock: hold it for a stable value.
        blocks() -> u64                     = READ_ONCE(i_blocks);
        set_blocks(u64)                     = WRITE_ONCE(i_blocks);
        nlink() -> u32                      = READ_ONCE(__bindgen_anon_1.i_nlink);
        set_nlink(u32)                      = set_nlink;
        /// Now, at the granularity its times are kept at.
        current_time() -> c::timespec64     = current_time;
        atime() -> c::timespec64            = inode_get_atime;
        mtime() -> c::timespec64            = inode_get_mtime;
        ctime() -> c::timespec64            = inode_get_ctime;
        set_atime(c::timespec64)            = inode_set_atime_to_ts;
        set_mtime(c::timespec64)            = inode_set_mtime_to_ts;
        set_ctime(c::timespec64)            = inode_set_ctime_to_ts;
    }

    /// i_size_read().
    pub fn size(&self) -> u64 {
        unsafe { c::i_size_read(self.as_ptr()) as u64 }
    }

    /// i_size_write().
    pub fn set_size(&self, size: u64) {
        unsafe { c::i_size_write(self.as_ptr(), loff(size)) }
    }

    // ── Ownership ────────────────────────────────────────────────────────

    /// @kuid as the inode stores it, a number in its user namespace.
    pub fn uid_of(&self, kuid: c::kuid_t) -> u32 {
        unsafe { c::from_kuid(self.user_ns(), kuid) }
    }

    pub fn gid_of(&self, kgid: c::kgid_t) -> u32 {
        unsafe { c::from_kgid(self.user_ns(), kgid) }
    }

    // ── Mode and flags ───────────────────────────────────────────────────

    pub fn is_dir(&self) -> bool {
        self.mode() as u32 & c::S_IFMT == c::S_IFDIR
    }

    pub fn is_reg(&self) -> bool {
        self.mode() as u32 & c::S_IFMT == c::S_IFREG
    }

    /// Set when it's made, and never changed.
    pub fn rdev(&self) -> c::dev_t {
        unsafe { (*self.as_ptr()).i_rdev }
    }

    pub fn flag(&self, flag: u32) -> bool {
        let flags = unsafe { read_volatile(&raw const (*self.as_ptr()).i_flags) };
        flags & flag != 0
    }

    /// A read-modify-write, not atomic, as C's |= and &= aren't: the VFS's
    /// own i_flags writes can race it, as they race C's.
    pub fn set_flag(&self, flag: u32, on: bool) {
        let p = unsafe { &raw mut (*self.as_ptr()).i_flags };
        unsafe {
            let flags = read_volatile(p);
            write_volatile(p, if on { flags | flag } else { flags & !flag });
        }
    }

    /// make_bad()'s.
    pub fn is_bad(&self) -> bool {
        unsafe { c::is_bad_inode(self.as_ptr()) }
    }

    /// i_rwsem held for write: inode_lock(), as the VFS takes it for a
    /// namespace operation or setattr.
    pub fn assert_locked(&self) {
        // SAFETY: c::rw_semaphore is the kernel's, i_rwsem one
        let l = unsafe { &*(&raw const (*self.as_ptr()).i_rwsem).cast::<c::rw_semaphore>() };
        Locked::from_c(l).assert_held();
    }

    // ── ACLs ─────────────────────────────────────────────────────────────

    /// None caches that there's none. The cache takes a reference of its own.
    pub fn set_cached_acl(&self, ty: AclType, acl: Option<&PosixAcl>) {
        let acl = acl.map_or(core::ptr::null_mut(), PosixAcl::as_raw);
        unsafe { c::set_cached_acl(self.as_ptr(), ty as c_int, acl) }
    }

    // ── The page cache ───────────────────────────────────────────────────

    pub fn mapping(&self) -> Mapping {
        unsafe { Mapping::from_raw((*self.as_ptr()).i_mapping) }
    }

    pub fn write_and_wait(&self) -> Result<(), BchError> {
        self.mapping().write_and_wait_range(0, u64::MAX)
    }

    /// Its page cache from @size on dropped: truncate_pagecache().
    pub fn truncate_pagecache(&self, size: u64) {
        unsafe { c::truncate_pagecache(self.as_ptr(), loff(size)) }
    }

    /// Its page cache from @start to @end, inclusive, dropped:
    /// truncate_pagecache_range().
    pub fn truncate_pagecache_range(&self, start: u64, end: u64) {
        unsafe { c::truncate_pagecache_range(self.as_ptr(), loff(start), loff(end)) }
    }

    /// i_size to @size, and the page cache past it dropped: truncate_setsize().
    pub fn truncate_setsize(&self, size: u64) {
        unsafe { c::truncate_setsize(self.as_ptr(), loff(size)) }
    }

    /// Its page cache gone, for good - the VFS evicting it:
    /// truncate_inode_pages_final().
    pub fn truncate_pages_final(&self) {
        unsafe { c::truncate_inode_pages_final(&raw mut (*self.as_ptr()).i_data) }
    }

    /// Whether it may be @size long - its rlimit and its filesystem's limit:
    /// inode_newsize_ok().
    pub fn newsize_ok(&self, size: u64) -> Result<(), BchError> {
        ret_to_result_void(unsafe { c::inode_newsize_ok(self.as_ptr(), loff(size)) })
    }

    /// i_rwsem, held for write until the guard's dropped: inode_lock().
    pub fn lock(&self) -> InodeLocked<'_> {
        unsafe { c::inode_lock(self.as_ptr()) };
        InodeLocked(self)
    }

    /// Its dirty metadata written, and waited on: sync_inode_metadata().
    pub fn sync_metadata(&self) -> Result<(), BchError> {
        ret_to_result_void(unsafe { c::sync_inode_metadata(self.as_ptr(), 1) })
    }

    /// Its direct IO in flight done: inode_dio_wait().
    pub fn dio_wait(&self) {
        unsafe { c::inode_dio_wait(self.as_ptr()) }
    }

    /// i_size raised to @size, if it's below: under i_lock, against the other
    /// extenders.
    pub fn size_extend(&self, size: u64) {
        let _guard = self.i_lock().lock();
        if size > self.size() {
            self.set_size(size);
        }
    }

    /// i_lock: the VFS's for its state and its hash, and the inode cache's
    /// for whether the inode's in it.
    pub fn i_lock(&self) -> &Locked<c::spinlock_t> {
        Locked::from_c(unsafe { &*(&raw const (*self.as_ptr()).i_lock).cast::<c::spinlock_t>() })
    }

    /// The I_* flags: inode_state_read(), which wants i_lock.
    pub fn state(&self, _i_lock: &ILocked<'_>) -> u32 {
        #[cfg(linux_ge = "6.19")]
        let s = unsafe { c::inode_state_read(self.as_ptr()) } as u32;
        #[cfg(not(linux_ge = "6.19"))]
        let s = unsafe { read_volatile(&raw const (*self.as_ptr()).i_state) } as u32;
        s
    }

    /// state(), without i_lock: inode_state_read_once() - as of a moment.
    pub fn state_once(&self) -> u32 {
        #[cfg(linux_ge = "6.19")]
        let s = unsafe { c::inode_state_read_once(self.as_ptr()) } as u32;
        #[cfg(not(linux_ge = "6.19"))]
        let s = unsafe { read_volatile(&raw const (*self.as_ptr()).i_state) } as u32;
        s
    }

    /// A reference, taken under i_lock: __iget() - or none, if the VFS is
    /// freeing it.
    pub fn iget(&self, i_lock: &ILocked<'_>) -> Option<ARef<VfsInode>> {
        if self.state(i_lock) & (c::I_FREEING | c::I_WILL_FREE) != 0 {
            return None;
        }
        unsafe {
            c::__iget(self.as_ptr());
            Some(ARef::from_raw(NonNull::from(self)))
        }
    }

    /// Hashed, for the VFS, which skips evicting from its hash what isn't -
    /// where it isn't in the VFS's hash but ours: inode_fake_hash().
    pub fn fake_hash(&self) {
        unsafe { c::inode_fake_hash(self.as_ptr()) }
    }

    /// fake_hash() undone, as its unhashing in ours is: whole, as the VFS
    /// reads it without our lock.
    pub fn unfake_hash(&self) {
        unsafe { write_volatile(&raw mut (*self.as_ptr()).i_hash.pprev, core::ptr::null_mut()) }
    }

    /// On its superblock's list of inodes: inode_sb_list_add().
    pub fn sb_list_add(&self) {
        unsafe { c::inode_sb_list_add(self.as_ptr()) }
    }

    /// Sleep - up to @timeout jiffies - for @inode's wake_new_bit(), if
    /// @still, asked once we're on the waitqueue, says it's coming: the
    /// kernel's own wait for an inode being freed, on its __I_NEW bit's. The
    /// waitqueue is found by @inode's address alone, so it needn't be live -
    /// nothing of it is read.
    pub fn wait_new_bit(inode: NonNull<VfsInode>, still: impl FnOnce() -> bool, timeout: c_long) {
        let mut wait: c::wait_bit_queue_entry = ::pin_init::zeroed();
        unsafe {
            let wq = c::inode_bit_waitqueue(&mut wait, inode.as_ptr().cast(), c::__I_NEW);
            c::prepare_to_wait(wq, &mut wait.wq_entry, c::TASK_UNINTERRUPTIBLE as c_int);
            if still() {
                c::schedule_timeout(timeout as _);
            }
            c::finish_wait(wq, &mut wait.wq_entry);
        }
    }

    /// Wake wait_new_bit()'s sleepers: inode_wake_up_bit().
    pub fn wake_new_bit(&self) {
        unsafe { c::inode_wake_up_bit(self.as_ptr(), c::__I_NEW) }
    }

    /// A reference, if the VFS isn't freeing it: igrab().
    pub fn igrab(&self) -> Option<ARef<VfsInode>> {
        // SAFETY: igrab()'s reference, if any, is ours
        NonNull::new(unsafe { c::igrab(self.as_ptr()) }).map(|p| unsafe { ARef::from_raw(p.cast()) })
    }

    /// Written back now - and waited for, if @sync: write_inode_now().
    pub fn write_now(&self, sync: bool) -> Result<(), BchError> {
        ret_to_result_void(unsafe { c::write_inode_now(self.as_ptr(), sync as c_int) })
    }

    // ── Eviction ─────────────────────────────────────────────────────────

    /// Evicted once it's unused, its dentries freed with it: d_mark_dontcache().
    pub fn mark_dontcache(&self) {
        unsafe { c::d_mark_dontcache(self.as_ptr()) }
    }

    /// Its unused dentries freed: d_prune_aliases().
    pub fn prune_aliases(&self) {
        unsafe { c::d_prune_aliases(self.as_ptr()) }
    }

    /// The end of evict_inode(): clear_inode().
    pub fn clear(&self) {
        unsafe { c::clear_inode(self.as_ptr()) }
    }

    // ── A new inode, its creator's alone: &mut ───────────────────────────

    /// What inode_init_once() makes of an allocation: its lists and locks.
    pub fn init_once() -> impl ::pin_init::PinInit<Self> {
        // SAFETY: inode_init_once() writes all of it
        unsafe {
            ::pin_init::pin_init_from_closure(|slot: *mut Self| {
                c::inode_init_once(slot.cast());
                Ok(())
            })
        }
    }

    /// The rest of it, for @sb: inode_init_always_gfp() - and its one
    /// reference, the caller's.
    pub fn init_always(&mut self, sb: &SuperBlock, gfp: c::gfp_t) -> Result<(), BchError> {
        ret_to_result_void(unsafe { c::inode_init_always_gfp(sb.as_ptr(), self.as_ptr(), gfp) })
    }

    /// Given up: make_bad_inode() - which evict doesn't delete.
    pub fn make_bad(&mut self) {
        unsafe { c::make_bad_inode(self.as_ptr()) }
    }

    pub fn set_ino(&mut self, ino: u64) {
        unsafe { (*self.as_ptr()).i_ino = ino as _ }
    }

    pub fn set_rdev(&mut self, rdev: c::dev_t) {
        unsafe { (*self.as_ptr()).i_rdev = rdev }
    }

    pub fn set_generation(&mut self, generation: u32) {
        unsafe { (*self.as_ptr()).i_generation = generation }
    }

    // codegen's operations structs, the inode's fields the kernel crate's: the
    // same structs
    pub fn set_ops(&mut self, iop: &'static c::inode_operations, fop: Option<&'static c::file_operations>) {
        unsafe {
            (*self.as_ptr()).i_op = core::ptr::from_ref(iop).cast();
            if let Some(fop) = fop {
                // in a union with free_inode
                (*self.as_ptr()).__bindgen_anon_3.i_fop = core::ptr::from_ref(fop).cast();
            }
        }
    }

    pub fn set_aops(&mut self, aops: &'static c::address_space_operations) {
        unsafe { (*(*self.as_ptr()).i_mapping).a_ops = core::ptr::from_ref(aops).cast() }
    }

    /// A device, fifo or socket's ops, from its mode and rdev:
    /// init_special_inode().
    pub fn init_special(&mut self) {
        unsafe { c::init_special_inode(self.as_ptr(), self.mode(), self.rdev()) }
    }

    /// Its page cache in lowmem: inode_nohighmem(), as a symlink's is.
    pub fn nohighmem(&mut self) {
        unsafe { c::inode_nohighmem(self.as_ptr()) }
    }

    /// Its page cache's folios at least @bytes: mapping_set_folio_min_order().
    pub fn set_folio_min_size(&mut self, bytes: usize) {
        let order = bytes.div_ceil(crate::util::kernel::PAGE_SIZE).next_power_of_two().trailing_zeros();
        unsafe { c::mapping_set_folio_min_order((*self.as_ptr()).i_mapping, order) }
    }

    /// The i_rwsems of @a and @b, in the VFS's order, held for write until the
    /// guard's dropped: lock_two_nondirectories().
    pub fn lock_two_nondirectories<'a>(a: &'a VfsInode, b: &'a VfsInode) -> TwoNondirsLocked<'a> {
        unsafe { c::lock_two_nondirectories(a.as_ptr(), b.as_ptr()) };
        TwoNondirsLocked(a, b)
    }
}

#[cfg(kernel)]
pub struct InodeLocked<'a>(&'a VfsInode);

#[cfg(kernel)]
impl Drop for InodeLocked<'_> {
    fn drop(&mut self) {
        unsafe { c::inode_unlock(self.0.as_ptr()) }
    }
}

#[cfg(kernel)]
pub struct TwoNondirsLocked<'a>(&'a VfsInode, &'a VfsInode);

#[cfg(kernel)]
impl Drop for TwoNondirsLocked<'_> {
    fn drop(&mut self) {
        unsafe { c::unlock_two_nondirectories(self.0.as_ptr(), self.1.as_ptr()) }
    }
}

// SAFETY: a borrowed VfsInode is referenced, as ihold() requires
#[cfg(kernel)]
unsafe impl AlwaysRefCounted for VfsInode {
    fn inc_ref(&self) {
        unsafe { c::ihold(self.as_ptr()) }
    }

    unsafe fn dec_ref(obj: NonNull<Self>) {
        unsafe { c::iput(obj.as_ptr().cast()) }
    }
}

// SAFETY: a struct inode, repr(transparent), its reference i_count's
#[cfg(kernel)]
unsafe impl crate::util::super_block::IsInode for VfsInode {}
