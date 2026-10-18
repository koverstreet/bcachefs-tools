// SPDX-License-Identifier: GPL-2.0

//! Debug parameters, BCH_DEBUG_PARAMS(): each a function, true while its
//! static key is on - in the kernel a static branch, as C's
//! static_branch_unlikely(&bch2_<name>); in userspace, the key's value.
//! init/fs.c defines every key, and the debug-only ones' module parameters
//! only with CONFIG_BCACHEFS_DEBUG.

use crate::c;

/// C's struct static_key_false, as the field kernel::static_branch_unlikely!()
/// takes a key from.
#[repr(transparent)]
pub struct Key {
    pub key: c::static_key_false,
}

macro_rules! debug_params {
    ($(($name:ident, $($desc:literal)*)),* $(,)?) => { ::paste::paste! { $(
        #[doc = concat!($($desc),*)]
        #[inline(always)]
        pub fn $name() -> bool {
            extern "C" {
                static [<bch2_ $name>]: Key;
            }
            // SAFETY: a static key, which C defines and only C writes
            #[cfg(kernel)]
            unsafe { ::kernel::static_branch_unlikely!([<bch2_ $name>], Key, key) }
            #[cfg(not(kernel))]
            unsafe { core::ptr::read_volatile(&raw const [<bch2_ $name>].key.key.v) != 0 }
        }
    )* } };
}
c::BCH_DEBUG_PARAMS_ALWAYS!(debug_params);
c::BCH_DEBUG_PARAMS_DEBUG!(debug_params);
