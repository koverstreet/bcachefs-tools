// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use cstruct_macros::{bitfield, c_const, c_enum, c_xmacro, CStruct};
use nestify::nest;
use typeinfo_macros::TypeInfo;

/*
 * rebalance on disk data structures:
 *
 * extents will contain a bch_extent_reconcile if they have background
 * processing pending; additionally, indirect extents will always have a
 * bch_extent_reconcile if they had any io path options set on the inode, since
 * we don't (yet) have backpointers that would let us look up the "owning" inode
 * of an indirect extent to recover the io path options.
 *
 * We also have 4 btrees for keeping track of pending rebalance work:
 *
 * BTREE_ID_reconcile_scan:
 *   Inum 0:
 *     Holds "scan cookies", which are created on option change to indicate that
 *     new options need to be propagated to each extent; this happens before the
 *     actual data processing.
 *
 *     A scan cookie may be for the entire filesystem, a specific device, or a
 *     specific inode.
 *
 *   Inum 1:
 *     Btree nodes that need background processing cannot be tracked by the
 *     other rebalance btrees; instead they have backpointers
 *     (KEY_TYPE_backpointer) created here.
 *
 *     This has the added benefit that btree nodes will be processed before
 *     regular data, which is beneficial if e.g. we're recovering from data
 *     being degraded.
 *
 *  BTREE_ID_reconcile_work:
 *    The main "pending rebalance work" btree: it's a simple bitset btree where
 *    a set bit indicates that an an extent in BTREE_ID_extents or
 *    BTREE_ID_reflink needs to be processed.
 *
 *  BTREE_ID_reconcile_hipri:
 *    If bch_extent_reconcile.hipri is set, the extent will be tracked here
 *    instead of BTREE_ID_reconcile_work and processed ahead of extents in
 *    BTREE_ID_reconcile_work; this is so that we can evacuate failed devices
 *    before other work.
 *
 *  BTREE_ID_reconcile_pending:
 *    If we'd like to move an extent to a specific target, but can't because the
 *    target is full, we set bch_extent_reconcile.pending and switch to tracking
 *    it here; pending rebalance work is re-attempted on device resize, add, or
 *    label change.
 */

nest! {
    #[repr(C)]
    #[derive(Clone, Copy, Default, CStruct, TypeInfo)]
    pub struct bch_extent_rebalance_v1 {
        #[c_bitfield]
        #>[bitfield(u64)]
        pub bits: pub struct bch_extent_rebalance_v1_bits {
            #[bits(6)]
            pub type_: u64,
            #[bits(3)]
            pub unused: u64,

            #[bits(1)]
            pub promote_target_from_inode: u64,
            #[bits(1)]
            pub erasure_code_from_inode: u64,
            #[bits(1)]
            pub data_checksum_from_inode: u64,
            #[bits(1)]
            pub background_compression_from_inode: u64,
            #[bits(1)]
            pub data_replicas_from_inode: u64,
            #[bits(1)]
            pub background_target_from_inode: u64,

            #[bits(16)]
            pub promote_target: u64,
            #[bits(1)]
            pub erasure_code: u64,
            #[bits(4)]
            pub data_checksum: u64,
            #[bits(4)]
            pub data_replicas: u64,
            #[bits(8)]
            pub background_compression: u64, /* enum bch_compression_opt */
            #[bits(16)]
            pub background_target: u64,
        },
    }
}

nest! {
    #[repr(C)]
    #[derive(Clone, Copy, Default, CStruct, TypeInfo)]
    pub struct bch_extent_reconcile {
        #[c_bitfield]
        #>[bitfield(u64)]
        pub bits: pub struct bch_extent_reconcile_bits {
            #[bits(8)]
            pub type_: u64,
            #[bits(2)]
            pub unused: u64,
            #[bits(5)]
            pub ptrs_moving: u64,
            #[bits(1)]
            pub hipri: u64,
            #[bits(1)]
            pub pending: u64,
            #[bits(5)]
            pub need_rb: u64,

            #[bits(1)]
            pub data_replicas_from_inode: u64,
            #[bits(1)]
            pub data_checksum_from_inode: u64,
            #[bits(1)]
            pub erasure_code_from_inode: u64,
            #[bits(1)]
            pub background_compression_from_inode: u64,
            #[bits(1)]
            pub background_target_from_inode: u64,
            #[bits(1)]
            pub promote_target_from_inode: u64,

            #[bits(3)]
            pub data_replicas: u64,
            #[bits(4)]
            pub data_checksum: u64,
            #[bits(1)]
            pub erasure_code: u64,
            #[bits(8)]
            pub background_compression: u64, /* enum bch_compression_opt */
            #[bits(10)]
            pub background_target: u64,
            #[bits(10)]
            pub promote_target: u64,
        },
    }
}

nest! {
    #[repr(C)]
    #[derive(Clone, Copy, Default, CStruct, TypeInfo)]
    pub struct bch_extent_reconcile_bp {
        #[c_bitfield]
        #>[bitfield(u64)]
        pub bits: pub struct bch_extent_reconcile_bp_bits {
            #[bits(9)]
            pub type_: u64,
            #[bits(55)]
            pub idx: u64,
        },
    }
}

c_xmacro! {
    /* subset of BCH_INODE_OPTS */
    BCH_RECONCILE_OPTS(x) {
        (data_replicas),
        (data_checksum),
        (erasure_code),
        (background_compression),
        (background_target),
        (promote_target),
    }
}

c_enum! {
    #[open]
    pub enum bch_reconcile_opts: u32 {
        BCH_RECONCILE_OPTS(n) => BCH_RECONCILE_ ## n,
    }
}

c_xmacro! {
    BCH_RECONCILE_ACCOUNTING(x) {
        (replicas, 0),
        (checksum, 1),
        (erasure_code, 2),
        (compression, 3),
        (target, 4),
        (high_priority, 5),
        (pending, 6),
        (stripes, 7),
    }
}

c_enum! {
    #[open]
    pub enum bch_reconcile_accounting_type: u32 {
        BCH_RECONCILE_ACCOUNTING(t, n) => BCH_RECONCILE_ACCOUNTING_ ## t = n,
        BCH_RECONCILE_ACCOUNTING_NR,
    }
}

c_xmacro! {
    RECONCILE_WORK_IDS(x) {
        (none),
        (hipri),
        (normal),
        (pending),
    }
}

c_enum! {
    #[open]
    pub enum reconcile_work_id: u32 {
        RECONCILE_WORK_IDS(t) => RECONCILE_WORK_ ## t,
    }
}

c_const! {
    #[c_int]
    pub const RECONCILE_SCAN_COOKIE_device: u32 = 32;
    #[c_int]
    pub const RECONCILE_SCAN_COOKIE_stripes: u32 = 3;
    #[c_int]
    pub const RECONCILE_SCAN_COOKIE_pending: u32 = 2;
    #[c_int]
    pub const RECONCILE_SCAN_COOKIE_metadata: u32 = 1;
    #[c_int]
    pub const RECONCILE_SCAN_COOKIE_fs: u32 = 0;
}
