// SPDX-License-Identifier: GPL-2.0

//! bcachefs Rust support — crate root.
//!
//! The Rust sources live alongside the C they bind, organized by subsystem
//! (`fs/btree/*.rs` next to `fs/btree/*.c`, …); this file is the crate root
//! that ties them together. The generated C bindings are namespaced as `c`;
//! the kernel-compat types they reference (bio, locks, `__le*`/`__u*`) are
//! resolved from `bcachefs-shim` in userspace or the `kernel` crate in-kernel.

#![cfg_attr(not(feature = "std"), no_std)]
// This crate is edition 2021, where unsafe ops inside unsafe fns don't need an
// inner unsafe block; the in-kernel build compiles us with -D
// unsafe-op-in-unsafe-fn (the kernel's policy), so opt back out crate-wide
// rather than re-styling the hand-written fs/ code and bindgen's output.
#![allow(unsafe_op_in_unsafe_fn)]
// The in-kernel build enables missing_docs; the generated bindings (bindgen's C
// enums + the codegen BkeyVal* enums) have undocumented variants. The kernel's
// own bindings crate allows it for the same reason.
#![allow(missing_docs)]
// Generated extern blocks reference kernel types bindgen flags as not FFI-safe
// (e.g. its_array); harmless, and allowed on the kernel's own bindings crate.
#![allow(improper_ctypes)]

#[cfg(kernel)]
const __LOG_PREFIX: &[u8] = b"bcachefs\0";

#[path = "alloc/accounting.rs"] pub mod accounting;
pub mod alloc {
    pub mod buckets;
}
pub mod btree;
pub mod debug {
    pub mod tests;
}
#[path = "fs/dirent.rs"]       pub mod dirent;
#[path = "init/fs.rs"]          pub mod fs;
#[path = "fs/inode.rs"]        pub mod inode;
#[path = "journal/read.rs"]     pub mod journal;
#[path = "fs/namei.rs"]        pub mod namei;
pub mod sb;
#[path = "fs/str_hash.rs"]     pub mod str_hash;
pub mod typeinfo;
pub mod util;
#[path = "fs/xattr.rs"]        pub mod xattr;
pub mod data {
    pub mod extents;
    pub mod io_misc;
}
pub mod errcode;
pub mod opts;

pub use paste::paste;

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
    #![allow(unnecessary_transmutes)]

    // Kernel types (bio, __u64, locks, …) that the fs/ bindings reference are
    // blocklisted by build.rs and resolved here. Userspace and local builds
    // source them from bcachefs-shim; the in-kernel build (`--cfg kernel`)
    // sources them from the kernel crate's raw bindings instead.
    #[cfg(not(kernel))]
    pub use bcachefs_shim::c::*;
    #[cfg(kernel)]
    pub use kernel::bindings::*;

    // Userspace #defines timespec64 → timespec (include/linux/time64.h), so
    // bindgen never emits a timespec64; alias it so fs/ can name the kernel's
    // real return type (timespec64) uniformly across both builds.
    #[cfg(not(kernel))]
    pub type timespec64 = timespec;

    // The generated bindings carry #[derive(TypeInfo)] on the bch_* family
    // (injected by codegen.rs); bring the derive macro into scope for them.
    use typeinfo_macros::TypeInfo;

    include!(concat!(env!("OUT_DIR"), "/bcachefs.rs"));

    crate::impl_darray!(bch_sb_handles, bch_sb_handle);

    use bitfield::bitfield;
    bitfield! {
        pub struct bch_scrypt_flags(u64);
        pub N, _: 15, 0;
        pub R, _: 31, 16;
        pub P, _: 47, 32;
    }
    bitfield! {
        pub struct bch_crypt_flags(u64);
        pub TYPE, _: 4, 0;
    }
    impl bch_sb_field_crypt {
        pub fn scrypt_flags(&self) -> Option<bch_scrypt_flags> {
            use core::convert::TryInto;
            match bch_kdf_types(bch_crypt_flags(self.flags).TYPE().try_into().ok()?) {
                bch_kdf_types::BCH_KDF_SCRYPT => Some(bch_scrypt_flags(self.kdf_flags)),
                _ => None,
            }
        }
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
            Self { magic: u64::from_le_bytes(*Self::MAGIC).to_le(), key }
        }

        pub fn is_encrypted(&self) -> bool {
            u64::from_le(self.magic) != u64::from_le_bytes(*Self::MAGIC)
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

    // In-kernel: no `zeroize` crate. Overwrite the bytes with volatile writes +
    // a compiler fence so the store can't be optimized away as dead — the wipe
    // is the point (same technique the `zeroize` crate uses internally).
    #[cfg(not(feature = "std"))]
    fn wipe<T>(v: &mut T) {
        let p = (v as *mut T).cast::<u8>();
        for i in 0..core::mem::size_of::<T>() {
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

    // Opaque kernel types the fs/ bindings reference but never deref. In-kernel
    // they come from kernel::bindings; userspace's shim doesn't provide them, so
    // define opaque stand-ins there.
    #[cfg(not(kernel))]
    pub enum rhash_lock_head {}
    #[cfg(not(kernel))]
    pub enum srcu_struct {}
}

#[allow(non_camel_case_types)]
pub type metadata_version = c::bcachefs_metadata_version;
#[allow(non_camel_case_types)]
pub type opt_id = c::bch_opt_id;
#[allow(non_camel_case_types)]
pub type btree_id = c::btree_id;

include!(concat!(env!("OUT_DIR"), "/newtype_enum_aliases_gen.rs"));
include!(concat!(env!("OUT_DIR"), "/btree_ids_gen.rs"));

// Position constructors/sentinels live with bkey; re-exported here for the
// `crate::POS_MIN` / `crate::SPOS_MAX` spelling used across the tree and the
// tools binary.
pub use btree::bkey::{pos, spos, POS_MAX, POS_MIN, SPOS_MAX};
#[cfg(feature = "std")]
pub use util::printbuf::printbuf_to_formatter;
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

impl c::bch_sb_field_type {
    pub fn bit(self) -> u32 {
        1u32 << self.0
    }
}

impl c::bch_data_type {
    pub fn bit(self) -> u64 {
        1u64 << self.0
    }
}

impl c::disk_accounting_type {
    pub fn bit(self) -> u32 {
        1u32 << self.0
    }
}

impl From<c::bch_data_type> for u32 {
    fn from(t: c::bch_data_type) -> u32 {
        t.0
    }
}

impl From<c::bch_compression_type> for u32 {
    fn from(t: c::bch_compression_type) -> u32 {
        t.0
    }
}

impl From<c::bcachefs_metadata_version> for u32 {
    fn from(v: c::bcachefs_metadata_version) -> u32 {
        v.0
    }
}

impl From<c::bch_opt_id> for u32 {
    fn from(id: c::bch_opt_id) -> u32 {
        id.0
    }
}

impl From<c::bch_bkey_type> for u32 {
    fn from(t: c::bch_bkey_type) -> u32 {
        t.0
    }
}

impl From<c::disk_accounting_type> for u32 {
    fn from(t: c::disk_accounting_type) -> u32 {
        t.0
    }
}

impl From<c::bch_reconcile_accounting_type> for u32 {
    fn from(t: c::bch_reconcile_accounting_type) -> u32 {
        t.0
    }
}
