// SPDX-License-Identifier: GPL-2.0

//! Static branches, for userspace: a key is a plain int, read - and nothing
//! here enables one, so a branch is never taken.

#![allow(non_camel_case_types)]

#[repr(C)]
#[derive(::pin_init::MaybeZeroable)]
pub struct static_key {
    pub v: i32,
}

#[repr(C)]
#[derive(::pin_init::MaybeZeroable)]
pub struct static_key_false {
    pub key: static_key,
}

#[path = "../jump_label.rs"]
mod common;
pub use common::StaticKeyFalse;

/// Whether the static key at field @field of @key, a static laid out as a
/// @keytyp, is enabled. Given only @key, a StaticKeyFalse static, it's safe.
///
/// # Safety
/// @key is a static, its @field a struct static_key_false - in an UnsafeCell, if
/// Rust defines it - that only the static key API writes.
#[macro_export]
macro_rules! static_branch_unlikely {
    ($key:path) => {{
        let _: &'static $crate::os::jump_label::StaticKeyFalse = &$key;
        // SAFETY: a StaticKeyFalse's key, which only the static key API writes
        unsafe { $crate::static_branch_unlikely!($key, $crate::os::jump_label::StaticKeyFalse, 0) }
    }};
    ($key:path, $keytyp:ty, $field:tt) => {{
        let k = ::core::ptr::addr_of!($key).cast::<u8>()
            .add(::core::mem::offset_of!($keytyp, $field)).cast::<$crate::c::static_key_false>();
        (*k).key.v != 0
    }};
}
