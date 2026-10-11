// SPDX-License-Identifier: GPL-2.0

#![allow(non_camel_case_types)]

use cstruct_macros::{bitfield, c_const, c_enum, c_xmacro, CStruct};

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

#[bitfield(u64)]
#[derive(CStruct)]
pub struct bch_extent_rebalance_v1 {
    type_:                             u6,
    unused:                            u3,

    promote_target_from_inode:         u1,
    erasure_code_from_inode:           u1,
    data_checksum_from_inode:          u1,
    background_compression_from_inode: u1,
    data_replicas_from_inode:          u1,
    background_target_from_inode:      u1,

    promote_target:                    u16,
    erasure_code:                      u1,
    data_checksum:                     u4,
    data_replicas:                     u4,
    background_compression:            u8, /* enum bch_compression_opt */
    background_target:                 u16,
}

#[bitfield(u64)]
#[derive(CStruct)]
pub struct bch_extent_reconcile {
    type_:                             u8,
    unused:                            u2,
    ptrs_moving:                       u5,
    hipri:                             u1,
    pending:                           u1,
    need_rb:                           u5,

    data_replicas_from_inode:          u1,
    data_checksum_from_inode:          u1,
    erasure_code_from_inode:           u1,
    background_compression_from_inode: u1,
    background_target_from_inode:      u1,
    promote_target_from_inode:         u1,

    data_replicas:                     u3,
    data_checksum:                     u4,
    erasure_code:                      u1,
    background_compression:            u8, /* enum bch_compression_opt */
    background_target:                 u10,
    promote_target:                    u10,
}

#[bitfield(u64)]
#[derive(CStruct)]
pub struct bch_extent_reconcile_bp {
    type_: u9,
    idx:   u55,
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
