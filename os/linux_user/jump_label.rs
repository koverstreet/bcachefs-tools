// SPDX-License-Identifier: GPL-2.0

//! Static branches, for userspace: a key is a plain int, read - and nothing
//! here enables one, so a branch is never taken.

/// Whether the static key at field @field of @key, a static laid out as a
/// @keytyp, is enabled.
///
/// # Safety
/// @key is a static, its @field a struct static_key_false - in an UnsafeCell, if
/// Rust defines it - that only the static key API writes.
#[macro_export]
macro_rules! static_branch_unlikely {
    ($key:path, $keytyp:ty, $field:ident) => {{
        let k = ::core::ptr::addr_of!($key).cast::<u8>()
            .add(::core::mem::offset_of!($keytyp, $field)).cast::<$crate::c::static_key_false>();
        (*k).key.v != 0
    }};
}
