// SPDX-License-Identifier: GPL-2.0

//! Debug parameters, debug_params!(): each a static key, C's bch2_<name>,
//! and a function, true while it's on - a static branch, as C's
//! static_branch_unlikely(&bch2_<name>) (util/jump_label.rs). Every key is a
//! module parameter, the debug-only ones only with CONFIG_BCACHEFS_DEBUG.

use crate::c;
use core::cell::UnsafeCell;

/// C's struct static_key_false, as the field static_branch_unlikely!() takes a
/// key from.
#[repr(transparent)]
pub struct Key {
    pub key: UnsafeCell<c::static_key_false>,
}

unsafe impl Sync for Key {}

impl Key {
    /// DEFINE_STATIC_KEY_FALSE()'s.
    const fn new() -> Self {
        Self { key: UnsafeCell::new(::pin_init::zeroed()) }
    }
}

#[cfg(kernel)]
mod param {
    use ::kernel::bindings::{kernel_param, kernel_param_ops, param_set_bool, static_key};
    use crate::util::ffi::c_char;
    use core::ffi::c_int;
    use core::sync::atomic::{AtomicI32, Ordering::Relaxed};

    fn count(key: *mut static_key) -> i32 {
        unsafe { AtomicI32::from_ptr(&raw mut (*key).enabled.counter) }.load(Relaxed)
    }

    #[cfg(CONFIG_JUMP_LABEL)]
    fn set_key(key: *mut static_key, v: bool) {
        unsafe {
            if v {
                ::kernel::bindings::static_key_enable(key)
            } else {
                ::kernel::bindings::static_key_disable(key)
            }
        }
    }

    #[cfg(not(CONFIG_JUMP_LABEL))]
    fn set_key(key: *mut static_key, v: bool) {
        let (to, from) = (i32::from(v), i32::from(!v));
        let enabled = count(key);
        if enabled != from {
            crate::warn_on_once!(enabled != to);
            return;
        }
        unsafe { AtomicI32::from_ptr(&raw mut (*key).enabled.counter) }.store(to, Relaxed);
    }

    unsafe extern "C" fn set(val: *const c_char, kp: *const kernel_param) -> c_int {
        /* Match bool exactly, by re-using it. */
        let key = unsafe { (*kp).__bindgen_anon_1.arg }.cast::<static_key>();
        let mut boolkp = unsafe { kp.read() };
        let mut v = false;

        boolkp.__bindgen_anon_1.arg = (&raw mut v).cast();

        let ret = unsafe { param_set_bool(val, &boolkp) };
        if ret != 0 {
            return ret;
        }
        set_key(key, v);
        0
    }

    unsafe extern "C" fn get(buffer: *mut c_char, kp: *const kernel_param) -> c_int {
        let key = unsafe { (*kp).__bindgen_anon_1.arg }.cast::<static_key>();
        let s = if count(key) > 0 { b"Y\n\0" } else { b"N\n\0" };
        unsafe { buffer.copy_from_nonoverlapping(s.as_ptr().cast(), s.len()) };
        2
    }

    pub(super) static STATIC_KEY_T: kernel_param_ops = kernel_param_ops {
        flags: ::kernel::bindings::KERNEL_PARAM_OPS_FL_NOARG,
        set:   Some(set),
        get:   Some(get),
        free:  None,
    };
}

macro_rules! debug_params {
    ($reg:meta; $(($name:ident, $($desc:literal)*)),* $(,)?) => { ::paste::paste! { $(
        #[doc = concat!($($desc),*)]
        #[inline(always)]
        pub fn $name() -> bool {
            unsafe { crate::static_branch_unlikely!([<bch2_ $name>], Key, key) }
        }

        #[unsafe(no_mangle)]
        #[allow(non_upper_case_globals)]
        pub static [<bch2_ $name>]: Key = Key::new();

        #[cfg(kernel)]
        #[$reg]
        crate::module_param_cb!($name, [<bch2_ $name>].key.get().cast(), &param::STATIC_KEY_T,
                                "static_key_t", 0o644, concat!($($desc),*));
    )* } };
}

/* Parameters that are useful for debugging, but should always be compiled in: */
debug_params! { cfg(all());
    (key_merging_disabled,
     "Disables merging of extents"),
    (btree_node_merging_disabled,
     "Disables merging of btree nodes"),
    (btree_gc_always_rewrite,
     "Causes mark and sweep to compact and rewrite every "
     "btree node it traverses"),
    (btree_gc_rewrite_disabled,
     "Disables rewriting of btree nodes during mark and sweep"),
    (btree_shrinker_disabled,
     "Disables the shrinker callback for the btree node cache"),
    (backpointers_no_use_write_buffer,
     "Don't use the write buffer for backpointers, enabling "
     "extra runtime checks"),
    (debug_check_btree_locking,
     "Enable additional asserts for btree locking"),
    (debug_check_iterators,
     "Enables extra verification for btree iterators"),
    (debug_check_bset_lookups,
     "Enables extra verification for bset lookups"),
    (debug_check_btree_accounting,
     "Verify btree accounting for keys within a node"),
    (debug_check_bkey_unpack,
     "Enables extra verification for bkey unpack"),
    (six_optimistic_spin_disabled,
     "Disables optimistic spinning on six locks: a waiter "
     "spinning while the lock's owner is running"),
}

/* Parameters that should only be compiled in debug mode: */
debug_params! { cfg(CONFIG_BCACHEFS_DEBUG);
    (journal_seq_verify,
     "Store the journal sequence number in the version "
     "number of every btree key, and verify that btree "
     "update ordering is preserved during recovery"),
    (inject_invalid_keys,
     "Store the journal sequence number in the version "
     "number of every btree key, and verify that btree "
     "update ordering is preserved during recovery"),
    (test_alloc_startup,
     "Force allocator startup to use the slowpath where it"
     "can't find enough free buckets without invalidating"
     "cached data"),
    (force_reconstruct_read,
     "Force reads to use the reconstruct path, when reading"
     "from erasure coded extents"),
    (test_restart_gc,
     "Test restarting mark and sweep gc when bucket gens change"),
}
