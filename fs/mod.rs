// SPDX-License-Identifier: GPL-2.0

//! bcachefs Rust support — crate root.
//!
//! The Rust sources live alongside the C they bind, organized by subsystem
//! (`fs/btree/*.rs` next to `fs/btree/*.c`, …); this file is the crate root
//! that ties them together. The generated C bindings are namespaced as `c`;
//! the kernel-compat types they reference (bio, locks, `__le*`/`__u*`) are
//! resolved from `bcachefs-shim` in userspace or the `kernel` crate in-kernel.

// no_std is not declared here: both kernel builds pass -Zcrate-attr=no_std on
// the command line - kbuild's Rust rule does it for CONFIG_RUST=y, and
// Makefile.rust.vendor's mod.o rule does it for CONFIG_RUST=n. Declaring it
// here as well was an unused_attributes error on the first of those, fatal
// under CONFIG_WERROR, and no cfg separates the two paths (both set --cfg
// kernel). The userspace build is std and wants none of it.
//
// This crate is edition 2021, where unsafe ops inside unsafe fns don't need an
// inner unsafe block; the in-kernel build compiles us with -D
// unsafe-op-in-unsafe-fn (the kernel's policy), so opt back out crate-wide
// rather than re-styling the hand-written fs/ code and bindgen's output.
#![allow(unsafe_op_in_unsafe_fn)]
// The in-kernel build enables missing_docs; the generated bindings (bindgen's C
// enums + the codegen *Typed enums) have undocumented variants. The kernel's
// own bindings crate allows it for the same reason.
#![allow(missing_docs)]
// The kernel passes its whole rust_allowed_features list as a crate attribute,
// and we take it as given - matching what the kernel permits is the point. Not
// using every feature in it is not a defect in this crate, but under
// -D warnings (CONFIG_WERROR) unused_features makes it a build failure.
#![allow(unused_features)]
// The kernel build's generated extern blocks reference kernel types bindgen
// flags as not FFI-safe (e.g. its_array); harmless, and allowed on the
// kernel's own bindings crate. Userspace's are clean, so there it warns.
#![cfg_attr(kernel, allow(improper_ctypes))]
// util/rcu.rs's FreeAnytime, an auto trait, which its frees specialize on
// (rustc_attrs is #[rustc_specialization_trait]), and util/task's
// Context::ext(), how a poll finds its task - its ContextBuilder is
// local_waker's, too. And #[in_place]'s futures: what holds a task in place,
// or a pool sized for a task's box, says its future's type. And util/task's
// TaskBox names the future an async closure makes from its home
// (async_fn_traits, in the trait's unboxed_closures form). Unstable, as the
// kernel's own features are: the kernel build has RUSTC_BOOTSTRAP=1, and so
// does ours (.cargo/config.toml).
#![feature(auto_traits, negative_impls, min_specialization, rustc_attrs, context_ext, local_waker,
           type_alias_impl_trait, async_fn_traits, unboxed_closures)]
#![allow(internal_features)]
// A type's record says which type each name it writes is: type_name(), const.
// ret_ip!(), C's _RET_IP_ - see os/linux_kernel/kernel.rs.
#![cfg_attr(bch_return_address, feature(core_intrinsics))]
// A cfg the build doesn't declare is an error, not a warning: a misspelled
// CONFIG_ is code silently compiled out. And the kernel version checks,
// cfg(linux_ge = "X.Y"), are declared only by a kernel build: userspace has
// no kernel version, so one there - not inside #[cfg(kernel)] - is a mistake.
// (kbuild doesn't check cfgs; userspace's build.rs declares them.)
#![deny(unexpected_cfgs)]

#[cfg(kernel)]
const __LOG_PREFIX: &[u8] = b"bcachefs\0";

/// The kernel crate's concat_literals!(), for the asm the C preprocessor makes
/// from the kernel's headers (os/linux_kernel/*_asm.rs.S, Makefile.rust): what C's asm
/// macros expand to is string literals side by side, which Rust concatenates
/// only through concat!(). Here, before every module: the asm's macros are
/// expanded where they're used - static_branch_unlikely!()'s anywhere - and
/// that's where concat_literals!() is looked up.
#[cfg(kernel)]
#[allow(unused_macros)] // the architectures with no such asm
macro_rules! concat_literals {
    ($($s:literal)*) => { ::core::concat!($($s),*) };
}

/// EBUG_ON(), as an assertion: checked in CONFIG_BCACHEFS_DEBUG builds.
macro_rules! ebug_assert {
    ($($arg:tt)*) => {
        if cfg!(CONFIG_BCACHEFS_DEBUG) {
            assert!($($arg)*);
        }
    };
}

/// An invariant - of a node's format, its keys, its sets; of a buffer sized
/// for what's written to it: checked in CONFIG_BCACHEFS_DEBUG builds,
/// otherwise assumed, and the checks it implies optimized out - code that
/// breaks it is then undefined behaviour, as in C. Each is a precondition for
/// formal verification to prove.
/// Several are checked together, so the optimizer can merge their compares,
/// and assumed one by one - it doesn't split an assumed conjunction. A
/// condition is evaluated before it's assumed: its own arithmetic stays
/// checked, so it compares values the code computes anyway. The conditions
/// are the message, with the caller's location: a message's arguments would
/// cost the fastpaths they're on.
macro_rules! invariant {
    ($($cond:expr),+ $(,)?) => {
        if !cfg!(CONFIG_BCACHEFS_DEBUG) {
            $(unsafe { core::hint::assert_unchecked($cond) };)+
        } else if !($($cond)&&+) {
            $crate::invariant_failed(concat!($(stringify!($cond), "; "),+))
        }
    };
}

#[cold]
#[inline(never)]
#[track_caller]
pub(crate) fn invariant_failed(cond: &'static str) -> ! {
    panic!("invariant failed: {cond}")
}

#[path = "alloc/accounting.rs"] pub mod accounting;
pub mod alloc {
    pub mod accounting_mem;
    pub mod background;
    pub mod backpointers;
    pub mod bucket_alloc;
    pub mod bucket_set;
    pub mod buckets;
    pub mod capacity;
    pub mod check;
    pub mod check_backpointers;
    pub mod check_data;
    pub mod dev_select;
    pub mod discard;
    pub mod disk_groups;
    pub mod foreground;
    pub mod gc_gens;
    pub mod lru;
    pub mod replicas;
    pub mod sector_alloc;
    pub mod write_point;
}
pub mod btree;
pub mod debug {
    pub mod async_objs;
    #[cfg(CONFIG_DEBUG_FS)]
    #[allow(clippy::module_inception)]
    pub mod debug;
    pub mod params;
    pub mod sysfs;
    pub mod tests;
    pub mod trace;
}
#[path = "fs/acl.rs"]          pub mod acl;
#[path = "fs/check.rs"]        pub mod check;
#[path = "fs/check_dir_structure.rs"] pub mod check_dir_structure;
#[path = "fs/check_nlinks.rs"] pub mod check_nlinks;
#[path = "fs/check_dirents.rs"] pub mod check_dirents;
#[path = "fs/check_extents.rs"] pub mod check_extents;
#[path = "fs/check_inodes.rs"] pub mod check_inodes;
#[path = "fs/check_root.rs"]   pub mod check_root;
#[path = "fs/check_unreachable_inodes.rs"] pub mod check_unreachable_inodes;
#[path = "fs/check_xattrs.rs"] pub mod check_xattrs;
#[path = "fs/fix_reflink_p.rs"] pub mod fix_reflink_p;
#[path = "fs/dirent.rs"]       pub mod dirent;
#[path = "init/fs.rs"]          pub mod fs;
pub mod init {
    #[cfg(kernel)]
    #[path = "chardev.rs"] pub mod chardev;
    #[path = "damage.rs"] pub mod damage;
    #[path = "dev.rs"] pub mod dev;
    #[path = "error.rs"] pub mod error;
    #[path = "module.rs"] pub mod module;
    #[path = "passes.rs"] pub mod passes;
    #[path = "progress.rs"] pub mod progress;
    #[path = "recovery.rs"] pub mod recovery;
    #[path = "registry.rs"] pub mod registry;
    #[path = "rw.rs"] pub mod rw;
}
#[path = "fs/inode.rs"]        pub mod inode;
#[path = "fs/inode_opts.rs"]   pub mod inode_opts;
#[cfg(kernel)]
#[path = "fs/ioctl.rs"]        pub mod ioctl;
#[path = "fs/logged_ops.rs"]   pub mod logged_ops;
#[path = "fs/lostfound.rs"]    pub mod lostfound;
#[path = "journal/read.rs"]     pub mod journal;
#[path = "fs/namei.rs"]        pub mod namei;
#[path = "fs/quota.rs"]        pub mod quota;
pub mod sb;
pub mod snapshots {
    #[path = "check_snapshots.rs"] pub mod check_snapshots;
    #[path = "delete.rs"] pub mod delete;
    #[path = "snapshot.rs"] pub mod snapshot;
    #[path = "subvolume.rs"] pub mod subvolume;
}
#[path = "fs/str_hash.rs"]     pub mod str_hash;
/// The records types defined in Rust that C shares are written from.
#[path = "types/lib.rs"]       pub mod types;
pub mod cstructs;
pub mod data_structures;
pub mod locking;
#[cfg_attr(kernel, path = "../os/linux_kernel/mod.rs")]
#[cfg_attr(not(kernel), path = "../os/linux_user/mod.rs")]
pub mod os;
pub mod typeinfo;
pub mod util;
/// The VFS, for its Rust entry points - kernel only, as C's
/// #ifndef NO_BCACHEFS_FS - but for the struct inode type bch_inode_info
/// holds, which both builds lay out.
pub mod vfs {
    #[cfg(kernel)] pub mod attr;
    #[cfg(kernel)] pub mod buffered;
    #[cfg(kernel)] pub mod buffered_write;
    #[cfg(kernel)] pub mod dentry;
    #[cfg(kernel)] pub mod dir;
    #[cfg(kernel)] pub mod direct;
    #[cfg(kernel)] pub mod direct_write;
    #[cfg(kernel)] pub mod export;
    #[cfg(kernel)] pub mod fdm;
    #[cfg(kernel)] pub mod fiemap;
    #[cfg(kernel)] pub mod file;
    #[cfg(kernel)] pub mod folio;
    #[cfg(kernel)] pub mod folio_reservation;
    #[cfg(kernel)] pub mod fs_type;
    #[cfg(kernel)] pub mod iattr;
    #[cfg(kernel)] pub mod idmap;
    #[cfg(kernel)] pub mod inode;
    #[cfg(kernel)] pub mod inode_cache;
    #[cfg(kernel)] pub mod kiocb;
    #[cfg(kernel)] pub mod inode_ops;
    #[cfg(kernel)] pub mod io;
    #[cfg(kernel)] pub mod mount;
    #[cfg(kernel)] pub mod namei;
    #[cfg(kernel)] pub mod new_inode;
    #[cfg(kernel)] pub mod ops;
    #[cfg(kernel)] pub mod pagecache;
    #[cfg(kernel)] pub mod path;
    #[cfg(kernel)] pub mod status_fd;
    #[cfg(kernel)] pub mod super_block;
    #[cfg(kernel)] pub mod ioctl;
    #[cfg(kernel)] pub mod writeback;
    pub mod vfs_inode;
}
#[path = "fs/xattr.rs"]        pub mod xattr;
pub mod data {
    pub mod checksum;
    pub mod compress;
    pub mod copygc;
    pub mod durability;
    pub mod ec {
        pub mod buf;
        pub mod check;
        pub mod delete;
        pub mod head;
        pub mod io;
        pub mod open;
        pub mod repair;
        pub mod stripe;
        pub mod stripe_new;
        pub mod stripe_new_blocks;
        pub mod stripe_new_reuse;
        pub mod stripe_new_tasks;
        pub mod trigger;
        pub mod update;
    }
    pub mod extent_update;
    pub mod extents;
    pub mod extents_sb;
    pub mod inline_data;
    pub mod io_misc;
    pub mod keylist;
    pub mod migrate;
    pub mod move_data;
    pub mod moving;
    pub mod moving_update;
    pub mod nocow_locking;
    pub mod promote;
    pub mod ptrs;
    pub mod read;
    pub mod reconcile {
        pub mod check;
        pub mod needs_reconcile;
        pub mod plan;
        pub mod scan;
        pub mod trigger;
        pub mod work;
    }
    pub mod reflink;
    pub mod scrub_journal;
    pub mod update;
    pub mod update_feasible;
    pub mod update_task;
    pub mod update_index;
    pub mod write;
    pub mod write_extent;
    pub mod write_nocow;
}
pub mod errcode;
/// The on-disk format.
pub mod format;
pub mod opts;

pub use paste::paste;
use crate::format::{Btree, BtreeFlags, CompressionType, DataType, BkeyType, MetadataVersion};

pub mod c {
    #![allow(clippy::missing_safety_doc)]
    #![allow(clippy::too_many_arguments)]
    #![allow(clippy::transmute_int_to_bool)]
    #![allow(clippy::unnecessary_cast)]
    #![allow(clippy::useless_transmute)]
    #![allow(non_upper_case_globals)]
    #![allow(non_camel_case_types)]
    #![allow(non_snake_case)]
    #![allow(unused)]
    #![allow(unknown_lints)] // unnecessary_transmutes is rustc 1.88's
    #![allow(unnecessary_transmutes)]

    // The kernel's types and functions (bio, __u64, locks, …). Userspace
    // builds take them from bcachefs-shim; the in-kernel build (`--cfg
    // kernel`) from the kernel crate's raw bindings, and what those don't
    // cover - the CRCs, the vendored closures - from codegen.rs's.
    #[cfg(not(kernel))]
    pub use bcachefs_shim::c::*;
    // and what the shim's headers had, which the userspace port defines now
    #[cfg(not(kernel))]
    pub use crate::os::c::*;
    #[cfg(kernel)]
    pub use kernel::bindings::*;
    #[cfg(kernel)]
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

    // codegen.rs blocklists it: ours, whether kernel::bindings has it or not
    #[cfg(kernel)]
    #[repr(C)]
    pub struct trace_array {
        _unused: [u8; 0],
    }

    // A union bindgen can't make a Rust union - a member isn't Copy: the
    // closure's, holding a work_struct - is a struct of these markers over
    // its storage. Zeroed storage is each member zeroed: Zeroable if they
    // all are, which this leaves the derive to check. (The storage, from
    // bindgen 0.73 an __BindgenOpaqueArrayN, codegen.rs makes Zeroable.)
    //
    // SAFETY: a PhantomData - no bytes
    #[cfg(kernel)]
    unsafe impl<T: ::pin_init::Zeroable> ::pin_init::Zeroable for __BindgenUnionField<T> {}

    // The types defined in Rust that C shares. printbuf the shim has too -
    // ours; and C's locks, as the structs C shares hold them.
    pub use crate::cstructs::c::*;
    pub use crate::cstructs::c::{mutex, percpu_rw_semaphore, raw_spinlock_t, rw_semaphore, spinlock_t};

    // Userspace #defines timespec64 → timespec (include/linux/time64.h), so
    // bindgen never emits a timespec64; alias it so fs/ can name the kernel's
    // real return type (timespec64) uniformly across both builds.
    #[cfg(not(kernel))]
    pub type timespec64 = timespec;
    // Likewise list_head and the hlists, which are liburcu's
    // (include/linux/list.h): for the types defined in Rust that hold them.
    #[cfg(not(kernel))]
    pub type list_head = cds_list_head;
    #[cfg(not(kernel))]
    pub type hlist_head = cds_hlist_head;
    #[cfg(not(kernel))]
    pub type hlist_node = cds_hlist_node;
    // And the kernel's rcu_head, which is #define rcu_head callback_head: a
    // macro, which the kernel crate's bindings don't have.
    #[cfg(kernel)]
    pub type rcu_head = callback_head;
    // And file_kattr, struct fileattr before 6.17: fs.h #defines the one
    // name to the other, for C.
    #[cfg(kernel)]
    #[cfg(not(linux_ge = "6.17"))]
    pub type file_kattr = fileattr;
    // And system_dfl_wq, system_unbound_wq before 6.17.
    #[cfg(kernel)]
    #[cfg(not(linux_ge = "6.17"))]
    pub use self::system_unbound_wq as system_dfl_wq;

    impl bch_sb_field_crypt {
        pub fn key(&self) -> &bch_encrypted_key {
            &self.key
        }
    }

    // ── Encryption key material ─────────────────────────────────────
    //
    // bch_key/bch_encrypted_key hold the filesystem encryption key. They're
    // kept out of DERIVE_READD (see codegen.rs) so they derive neither Copy (a
    // Drop type can't be Copy) nor Debug (don't leak key bytes). The key is
    // wiped on drop — via the `zeroize` crate in userspace, and a hand-rolled
    // volatile memset in-kernel, which can't pull in the `zeroize` crate.

    impl Clone for bch_key {
        fn clone(&self) -> Self {
            Self { key: self.key }
        }
    }

    impl Clone for bch_encrypted_key {
        fn clone(&self) -> Self {
            Self { magic: self.magic, key: self.key.clone() }
        }
    }

    impl bch_encrypted_key {
        pub const MAGIC: &[u8; 8] = b"bch**key";

        /// Plaintext (unencrypted) key for the crypt field (remove-passphrase).
        pub fn new_unencrypted(key: bch_key) -> Self {
            Self { magic: u64::from_le_bytes(*Self::MAGIC).into(), key }
        }

        pub fn is_encrypted(&self) -> bool {
            self.magic.get() != u64::from_le_bytes(*Self::MAGIC)
        }

        pub fn into_key(self) -> bch_key {
            self.key.clone()
        }
    }

    #[cfg(feature = "std")]
    use zeroize::{Zeroize, ZeroizeOnDrop};

    #[cfg(feature = "std")]
    impl Zeroize for bch_key {
        fn zeroize(&mut self) { self.key.zeroize(); }
    }
    #[cfg(feature = "std")]
    impl Drop for bch_key {
        fn drop(&mut self) { self.zeroize(); }
    }
    #[cfg(feature = "std")]
    impl ZeroizeOnDrop for bch_key {}

    #[cfg(feature = "std")]
    impl Zeroize for bch_encrypted_key {
        fn zeroize(&mut self) {
            self.magic.zeroize();
            self.key.zeroize();
        }
    }
    #[cfg(feature = "std")]
    impl Drop for bch_encrypted_key {
        fn drop(&mut self) { self.zeroize(); }
    }
    #[cfg(feature = "std")]
    impl ZeroizeOnDrop for bch_encrypted_key {}

    // memzero_explicit(): volatile writes + a compiler fence, so the store
    // can't be optimized away as dead - the wipe is the point (the technique
    // the `zeroize` crate uses internally). The keys' Drop in-kernel, which
    // has no `zeroize`; and in both builds, key material that isn't a bch_key.
    pub(crate) fn wipe<T: ?Sized>(v: &mut T) {
        let len = core::mem::size_of_val(v);
        let p = (v as *mut T).cast::<u8>();
        for i in 0..len {
            unsafe { core::ptr::write_volatile(p.add(i), 0) };
        }
        core::sync::atomic::compiler_fence(core::sync::atomic::Ordering::SeqCst);
    }
    #[cfg(not(feature = "std"))]
    impl Drop for bch_key {
        fn drop(&mut self) { wipe(&mut self.key); }
    }
    #[cfg(not(feature = "std"))]
    impl Drop for bch_encrypted_key {
        fn drop(&mut self) { wipe(self); }
    }
}

pub use crate::c::OptId;

macro_rules! btree_ids_known {
    ($(($name:ident, $nr:literal, $($rest:tt)*)),* $(,)?) => {
        pub const BTREE_IDS_KNOWN: &[Btree] = &[$(Btree::$name,)*];
    };
}
c::Btree_arms!(btree_ids_known);

/// The btrees with any of @flags, a bit each, by id.
pub const fn btrees_with(flags: BtreeFlags) -> u64 {
    let (mut mask, mut i) = (0, 0);
    while i < Btree::NR {
        if Btree(i).flags().intersects(flags) {
            mask |= 1 << i;
        }
        i += 1;
    }
    mask
}

/// Btrees whose keys are snapshotted; C's btree_has_snapshots_mask.
pub const BTREE_HAS_SNAPSHOTS_MASK: u64 = btrees_with(BtreeFlags::snapshots);

/// Btrees of extents; C's btree_is_extents_mask.
pub const BTREE_IS_EXTENTS_MASK: u64 = btrees_with(BtreeFlags::extents);

/// Btrees whose keys point to data; C's btree_has_data_ptrs_mask.
pub const BTREE_HAS_DATA_PTRS_MASK: u64 = btrees_with(BtreeFlags::data);

impl Btree {
    /// Whether its keys are extents, covering ranges: btree_id_is_extents().
    pub const fn is_extents(self) -> bool {
        self.flags().contains(BtreeFlags::extents)
    }

    /// Whether its keys are snapshotted: btree_type_has_snapshots().
    pub const fn has_snapshots(self) -> bool {
        self.flags().contains(BtreeFlags::snapshots)
    }

    /// Whether its keys' positions have a snapshot field:
    /// btree_type_has_snapshot_field().
    pub const fn has_snapshot_field(self) -> bool {
        self.flags().intersects(BtreeFlags::snapshots.union(BtreeFlags::snapshot_field))
    }

    /// Whether its keys point to data: btree_type_has_data_ptrs().
    pub const fn has_data_ptrs(self) -> bool {
        self.flags().contains(BtreeFlags::data)
    }
}

/// The btrees in @mask, a bit per btree id.
pub fn btrees_in(mask: u64) -> impl Iterator<Item = Btree> {
    BTREE_IDS_KNOWN.iter().copied().filter(move |b| mask & (1u64 << b.0) != 0)
}

// Position constructors/sentinels live with bkey; re-exported here for the
// `crate::POS_MIN` / `crate::SPOS_MAX` spelling used across the tree and the
// tools binary.
pub use btree::bkey::{pos, spos, POS_MAX, POS_MIN, SPOS_MAX};
#[doc(hidden)]
pub use cstruct_macros::__prt;
#[cfg(feature = "std")]
pub use btree::bbpos::{bbpos_range_parse, BbposRange};

// ---------------------------------------------------------------------------
// path_to_cstr may not belong in fs/ at all; parked here for now, relocate later.

#[cfg(feature = "std")]
use std::ffi::CString;

/// Convert a filesystem path to a CString. Userspace-only — gated out of the
/// no_std (in-kernel) build, which has no std path/OsStr types.
#[cfg(feature = "std")]
pub fn path_to_cstr<P: AsRef<std::path::Path>>(p: P) -> CString {
    use std::os::unix::ffi::OsStrExt;
    CString::new(p.as_ref().as_os_str().as_bytes()).unwrap()
}

impl c::SbFieldType {
    pub fn bit(self) -> u32 {
        1u32 << self.0
    }
}

impl DataType {
    pub fn bit(self) -> u64 {
        1u64 << self.0
    }

    /// Free/empty data types — not counted as "used" space.
    pub fn is_empty(self) -> bool {
        self == DataType::free
            || self == DataType::need_gc_gens
            || self == DataType::need_discard
    }

    /// Internal/hidden data types — not user-visible (superblock, journal).
    pub fn is_hidden(self) -> bool {
        self == DataType::sb || self == DataType::journal
    }

    /// What a bucket holding it is: cached and stripe data are in user buckets.
    pub fn bucket_type(self) -> DataType {
        if self == DataType::cached || self == DataType::stripe { DataType::user } else { self }
    }

    /// Whether a bucket of this type can't hold a pointer of type @ptr.
    pub fn mismatches(self, ptr: DataType) -> bool {
        !self.is_empty() &&
            self != DataType::multiple &&
            self.bucket_type() != ptr.bucket_type()
    }

    /// Whether copygc moves data of this type.
    pub fn is_movable(self) -> bool {
        c::DATA_TYPES_MOVABLE.contains(self.into())
    }
}

impl c::DiskAccountingType {
    pub fn bit(self) -> u32 {
        1u32 << self.0
    }
}

impl From<DataType> for u32 {
    fn from(t: DataType) -> u32 {
        t.0
    }
}

impl From<CompressionType> for u32 {
    fn from(t: CompressionType) -> u32 {
        t.0
    }
}

impl From<MetadataVersion> for u32 {
    fn from(v: MetadataVersion) -> u32 {
        v.0
    }
}

impl From<c::OptId> for u32 {
    fn from(id: c::OptId) -> u32 {
        id.0
    }
}

impl From<BkeyType> for u32 {
    fn from(t: BkeyType) -> u32 {
        t.0
    }
}

impl From<c::DiskAccountingType> for u32 {
    fn from(t: c::DiskAccountingType) -> u32 {
        t.0
    }
}

// ── ARM32 Kernel EABI Division Helpers ──────────────────────────
//
// On 32-bit ARM, raw 64-bit integer division lowers to compiler-rt calls.
// Since the Linux kernel does not export these symbols to out-of-tree modules,
// we declare them globally using assembly stubs that call the kernel's real
// exported math functions. We restrict this to non-std (kernel) ARM32 builds.

#[cfg(all(target_arch = "arm", not(feature = "std")))]
core::arch::global_asm!(
    ".global __aeabi_uldivmod",
    ".type __aeabi_uldivmod, %function",
    "__aeabi_uldivmod:",
    "    .fnstart",
    // Push r8 specifically to push exactly 6 registers (24 bytes) total,
    // maintaining the AAPCS required 8-byte stack alignment for the C call.
    "    push {{r4, r5, r6, r7, r8, lr}}",
    "    .save {{r4, r5, r6, r7, r8, lr}}",
    "    mov r4, r0",
    "    mov r5, r1",
    "    mov r6, r2",
    "    mov r7, r3",
    "    bl div64_u64",
    "    umull r2, r3, r0, r6",
    "    mla r3, r0, r7, r3",
    "    mla r3, r1, r6, r3",
    "    subs r2, r4, r2",
    "    sbc r3, r5, r3",
    "    pop {{r4, r5, r6, r7, r8, pc}}",
    "    .fnend",
    ".size __aeabi_uldivmod, . - __aeabi_uldivmod"
);


#[cfg(all(target_arch = "arm", not(feature = "std")))]
core::arch::global_asm!(
    ".global __aeabi_ldivmod",
    ".type __aeabi_ldivmod, %function",
    "__aeabi_ldivmod:",
    "    .fnstart",
    // Push r8 specifically to push exactly 6 registers (24 bytes) total,
    // maintaining the AAPCS required 8-byte stack alignment for the C call.
    "    push {{r4, r5, r6, r7, r8, lr}}",
    "    .save {{r4, r5, r6, r7, r8, lr}}",
    "    mov r4, r0",
    "    mov r5, r1",
    "    mov r6, r2",
    "    mov r7, r3",
    "    bl div64_s64",
    "    umull r2, r3, r0, r6",
    "    mla r3, r0, r7, r3",
    "    mla r3, r1, r6, r3",
    "    subs r2, r4, r2",
    "    sbc r3, r5, r3",
    "    pop {{r4, r5, r6, r7, r8, pc}}",
    "    .fnend",
    ".size __aeabi_ldivmod, . - __aeabi_ldivmod"
);