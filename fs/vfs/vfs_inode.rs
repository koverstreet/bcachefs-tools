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
    core::ffi::c_int,
    core::ptr::{read_volatile, write_volatile, NonNull},
    crate::acl::{AclType, PosixAcl},
    crate::errcode::{ret_to_result_void as ret_to_result, BchError},
    kernel::sync::aref::AlwaysRefCounted,
    super::super_block::SuperBlock,
    crate::locking::Lock,
};

/// A struct inode of ours - one InodeInfo's - referenced while this is
/// borrowed. Both builds lay it out; only the kernel's has a VFS.
#[repr(transparent)]
pub struct VfsInode(Opaque<c::inode>);

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
        nlink() -> u32                      = READ_ONCE(__bindgen_anon_1.i_nlink);
        set_nlink(u32)                      = set_nlink;
        size() -> i64                       = i_size_read;
        set_size(i64)                       = i_size_write;
        /// Now, at the granularity its times are kept at.
        current_time() -> c::timespec64     = current_time;
        atime() -> c::timespec64            = inode_get_atime;
        mtime() -> c::timespec64            = inode_get_mtime;
        ctime() -> c::timespec64            = inode_get_ctime;
        set_atime(c::timespec64)            = inode_set_atime_to_ts;
        set_mtime(c::timespec64)            = inode_set_mtime_to_ts;
        set_ctime(c::timespec64)            = inode_set_ctime_to_ts;
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

    /// i_rwsem held for write: inode_lock(), as the VFS takes it for a
    /// namespace operation or setattr.
    pub fn assert_locked(&self) {
        // SAFETY: c::rw_semaphore is the kernel's, i_rwsem one
        let l = unsafe { &*(&raw const (*self.as_ptr()).i_rwsem).cast::<c::rw_semaphore>() };
        Lock::from_c(l).assert_held();
    }

    // ── ACLs ─────────────────────────────────────────────────────────────

    /// None caches that there's none. The cache takes a reference of its own.
    pub fn set_cached_acl(&self, ty: AclType, acl: Option<&PosixAcl>) {
        let acl = acl.map_or(core::ptr::null_mut(), PosixAcl::as_raw);
        unsafe { c::set_cached_acl(self.as_ptr(), ty as c_int, acl) }
    }

    // ── The page cache ───────────────────────────────────────────────────

    pub fn write_and_wait(&self) -> Result<(), BchError> {
        ret_to_result(unsafe { c::rust_filemap_write_and_wait_range((*self.as_ptr()).i_mapping, 0, i64::MAX) })
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
