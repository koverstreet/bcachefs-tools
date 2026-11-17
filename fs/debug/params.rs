// SPDX-License-Identifier: GPL-2.0

//! Debug parameters, debug_params!(): each a static key, and a function, true
//! while it's on - a static branch, as C's static_branch_unlikely()
//! (os::jump_label). Every key is a module parameter, the debug-only ones only
//! with CONFIG_BCACHEFS_DEBUG.

use crate::os::jump_label::StaticKeyFalse;

macro_rules! debug_params {
    ($reg:meta; $(($name:ident, $($desc:literal)*)),* $(,)?) => { ::paste::paste! { $(
        static [<$name:upper>]: StaticKeyFalse = StaticKeyFalse::new();

        #[doc = concat!($($desc),*)]
        #[inline(always)]
        pub fn $name() -> bool {
            crate::static_branch_unlikely!([<$name:upper>])
        }

        #[cfg(kernel)]
        #[$reg]
        crate::module_param_static_key!($name, [<$name:upper>], concat!($($desc),*));
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
