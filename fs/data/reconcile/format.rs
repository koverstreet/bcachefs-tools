// SPDX-License-Identifier: GPL-2.0

//! The data types of data/reconcile/format.h, which is generated from this file: see
//! fs/types/lib.rs. Translated from the C by c2rs.

#![allow(non_camel_case_types)]

use cstruct_macros::{bitfield, c_const, c_enum, c_verbatim, c_xmacro, CStruct};
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

#[bitfield(u64)]
pub struct bch_extent_rebalance_v1_type_bits {
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
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_extent_rebalance_v1 {
    #[c_bitfield]
    pub type_bits: bch_extent_rebalance_v1_type_bits,
}
impl bch_extent_rebalance_v1 {
    pub fn type_(&self) -> u64 { let b = self.type_bits; b.type_() }
    pub fn set_type_(&mut self, v: u64) { let mut b = self.type_bits; b.set_type_(v); self.type_bits = b; }
    pub fn unused(&self) -> u64 { let b = self.type_bits; b.unused() }
    pub fn set_unused(&mut self, v: u64) { let mut b = self.type_bits; b.set_unused(v); self.type_bits = b; }
    pub fn promote_target_from_inode(&self) -> u64 { let b = self.type_bits; b.promote_target_from_inode() }
    pub fn set_promote_target_from_inode(&mut self, v: u64) { let mut b = self.type_bits; b.set_promote_target_from_inode(v); self.type_bits = b; }
    pub fn erasure_code_from_inode(&self) -> u64 { let b = self.type_bits; b.erasure_code_from_inode() }
    pub fn set_erasure_code_from_inode(&mut self, v: u64) { let mut b = self.type_bits; b.set_erasure_code_from_inode(v); self.type_bits = b; }
    pub fn data_checksum_from_inode(&self) -> u64 { let b = self.type_bits; b.data_checksum_from_inode() }
    pub fn set_data_checksum_from_inode(&mut self, v: u64) { let mut b = self.type_bits; b.set_data_checksum_from_inode(v); self.type_bits = b; }
    pub fn background_compression_from_inode(&self) -> u64 { let b = self.type_bits; b.background_compression_from_inode() }
    pub fn set_background_compression_from_inode(&mut self, v: u64) { let mut b = self.type_bits; b.set_background_compression_from_inode(v); self.type_bits = b; }
    pub fn data_replicas_from_inode(&self) -> u64 { let b = self.type_bits; b.data_replicas_from_inode() }
    pub fn set_data_replicas_from_inode(&mut self, v: u64) { let mut b = self.type_bits; b.set_data_replicas_from_inode(v); self.type_bits = b; }
    pub fn background_target_from_inode(&self) -> u64 { let b = self.type_bits; b.background_target_from_inode() }
    pub fn set_background_target_from_inode(&mut self, v: u64) { let mut b = self.type_bits; b.set_background_target_from_inode(v); self.type_bits = b; }
    pub fn promote_target(&self) -> u64 { let b = self.type_bits; b.promote_target() }
    pub fn set_promote_target(&mut self, v: u64) { let mut b = self.type_bits; b.set_promote_target(v); self.type_bits = b; }
    pub fn erasure_code(&self) -> u64 { let b = self.type_bits; b.erasure_code() }
    pub fn set_erasure_code(&mut self, v: u64) { let mut b = self.type_bits; b.set_erasure_code(v); self.type_bits = b; }
    pub fn data_checksum(&self) -> u64 { let b = self.type_bits; b.data_checksum() }
    pub fn set_data_checksum(&mut self, v: u64) { let mut b = self.type_bits; b.set_data_checksum(v); self.type_bits = b; }
    pub fn data_replicas(&self) -> u64 { let b = self.type_bits; b.data_replicas() }
    pub fn set_data_replicas(&mut self, v: u64) { let mut b = self.type_bits; b.set_data_replicas(v); self.type_bits = b; }
    pub fn background_compression(&self) -> u64 { let b = self.type_bits; b.background_compression() }
    pub fn set_background_compression(&mut self, v: u64) { let mut b = self.type_bits; b.set_background_compression(v); self.type_bits = b; }
    pub fn background_target(&self) -> u64 { let b = self.type_bits; b.background_target() }
    pub fn set_background_target(&mut self, v: u64) { let mut b = self.type_bits; b.set_background_target(v); self.type_bits = b; }
}

#[bitfield(u64)]
pub struct bch_extent_reconcile_type_bits {
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
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_extent_reconcile {
    #[c_bitfield]
    pub type_bits: bch_extent_reconcile_type_bits,
}
impl bch_extent_reconcile {
    pub fn type_(&self) -> u64 { let b = self.type_bits; b.type_() }
    pub fn set_type_(&mut self, v: u64) { let mut b = self.type_bits; b.set_type_(v); self.type_bits = b; }
    pub fn unused(&self) -> u64 { let b = self.type_bits; b.unused() }
    pub fn set_unused(&mut self, v: u64) { let mut b = self.type_bits; b.set_unused(v); self.type_bits = b; }
    pub fn ptrs_moving(&self) -> u64 { let b = self.type_bits; b.ptrs_moving() }
    pub fn set_ptrs_moving(&mut self, v: u64) { let mut b = self.type_bits; b.set_ptrs_moving(v); self.type_bits = b; }
    pub fn hipri(&self) -> u64 { let b = self.type_bits; b.hipri() }
    pub fn set_hipri(&mut self, v: u64) { let mut b = self.type_bits; b.set_hipri(v); self.type_bits = b; }
    pub fn pending(&self) -> u64 { let b = self.type_bits; b.pending() }
    pub fn set_pending(&mut self, v: u64) { let mut b = self.type_bits; b.set_pending(v); self.type_bits = b; }
    pub fn need_rb(&self) -> u64 { let b = self.type_bits; b.need_rb() }
    pub fn set_need_rb(&mut self, v: u64) { let mut b = self.type_bits; b.set_need_rb(v); self.type_bits = b; }
    pub fn data_replicas_from_inode(&self) -> u64 { let b = self.type_bits; b.data_replicas_from_inode() }
    pub fn set_data_replicas_from_inode(&mut self, v: u64) { let mut b = self.type_bits; b.set_data_replicas_from_inode(v); self.type_bits = b; }
    pub fn data_checksum_from_inode(&self) -> u64 { let b = self.type_bits; b.data_checksum_from_inode() }
    pub fn set_data_checksum_from_inode(&mut self, v: u64) { let mut b = self.type_bits; b.set_data_checksum_from_inode(v); self.type_bits = b; }
    pub fn erasure_code_from_inode(&self) -> u64 { let b = self.type_bits; b.erasure_code_from_inode() }
    pub fn set_erasure_code_from_inode(&mut self, v: u64) { let mut b = self.type_bits; b.set_erasure_code_from_inode(v); self.type_bits = b; }
    pub fn background_compression_from_inode(&self) -> u64 { let b = self.type_bits; b.background_compression_from_inode() }
    pub fn set_background_compression_from_inode(&mut self, v: u64) { let mut b = self.type_bits; b.set_background_compression_from_inode(v); self.type_bits = b; }
    pub fn background_target_from_inode(&self) -> u64 { let b = self.type_bits; b.background_target_from_inode() }
    pub fn set_background_target_from_inode(&mut self, v: u64) { let mut b = self.type_bits; b.set_background_target_from_inode(v); self.type_bits = b; }
    pub fn promote_target_from_inode(&self) -> u64 { let b = self.type_bits; b.promote_target_from_inode() }
    pub fn set_promote_target_from_inode(&mut self, v: u64) { let mut b = self.type_bits; b.set_promote_target_from_inode(v); self.type_bits = b; }
    pub fn data_replicas(&self) -> u64 { let b = self.type_bits; b.data_replicas() }
    pub fn set_data_replicas(&mut self, v: u64) { let mut b = self.type_bits; b.set_data_replicas(v); self.type_bits = b; }
    pub fn data_checksum(&self) -> u64 { let b = self.type_bits; b.data_checksum() }
    pub fn set_data_checksum(&mut self, v: u64) { let mut b = self.type_bits; b.set_data_checksum(v); self.type_bits = b; }
    pub fn erasure_code(&self) -> u64 { let b = self.type_bits; b.erasure_code() }
    pub fn set_erasure_code(&mut self, v: u64) { let mut b = self.type_bits; b.set_erasure_code(v); self.type_bits = b; }
    pub fn background_compression(&self) -> u64 { let b = self.type_bits; b.background_compression() }
    pub fn set_background_compression(&mut self, v: u64) { let mut b = self.type_bits; b.set_background_compression(v); self.type_bits = b; }
    pub fn background_target(&self) -> u64 { let b = self.type_bits; b.background_target() }
    pub fn set_background_target(&mut self, v: u64) { let mut b = self.type_bits; b.set_background_target(v); self.type_bits = b; }
    pub fn promote_target(&self) -> u64 { let b = self.type_bits; b.promote_target() }
    pub fn set_promote_target(&mut self, v: u64) { let mut b = self.type_bits; b.set_promote_target(v); self.type_bits = b; }
}

#[bitfield(u64)]
pub struct bch_extent_reconcile_bp_type_bits {
    #[bits(9)]
    pub type_: u64,
    #[bits(55)]
    pub idx: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct, TypeInfo)]
pub struct bch_extent_reconcile_bp {
    #[c_bitfield]
    pub type_bits: bch_extent_reconcile_bp_type_bits,
}
impl bch_extent_reconcile_bp {
    pub fn type_(&self) -> u64 { let b = self.type_bits; b.type_() }
    pub fn set_type_(&mut self, v: u64) { let mut b = self.type_bits; b.set_type_(v); self.type_bits = b; }
    pub fn idx(&self) -> u64 { let b = self.type_bits; b.idx() }
    pub fn set_idx(&mut self, v: u64) { let mut b = self.type_bits; b.set_idx(v); self.type_bits = b; }
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

macro_rules! __bch_reconcile_opts_0 {
    ([$($acc:tt)*] $(($n:tt)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[open]
            pub enum bch_reconcile_opts: u32 {
                $($acc)*
                $([<BCH_RECONCILE_ $n>],)*
            }
        }
    } };
}
BCH_RECONCILE_OPTS!(__bch_reconcile_opts_0 []);

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

macro_rules! __bch_reconcile_accounting_type_0 {
    ([$($acc:tt)*] $(($t:tt, $n:expr)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[open]
            pub enum bch_reconcile_accounting_type: u32 {
                $($acc)*
                $([<BCH_RECONCILE_ACCOUNTING_ $t>] = (($n) as u32),)*
                BCH_RECONCILE_ACCOUNTING_NR,
            }
        }
    } };
}
BCH_RECONCILE_ACCOUNTING!(__bch_reconcile_accounting_type_0 []);

c_xmacro! {
    RECONCILE_WORK_IDS(x) {
        (none),
        (hipri),
        (normal),
        (pending),
    }
}

macro_rules! __reconcile_work_id_0 {
    ([$($acc:tt)*] $(($t:tt)),* $(,)?) => { ::paste::paste! {
        c_enum! {
            #[open]
            pub enum reconcile_work_id: u32 {
                $($acc)*
                $([<RECONCILE_WORK_ $t>],)*
            }
        }
    } };
}
RECONCILE_WORK_IDS!(__reconcile_work_id_0 []);

c_verbatim!(r#"
__maybe_unused
static const enum btree_id reconcile_work_btree[] = {
	[RECONCILE_WORK_hipri]		= BTREE_ID_reconcile_hipri,
	[RECONCILE_WORK_normal]		= BTREE_ID_reconcile_work,
	[RECONCILE_WORK_pending]	= BTREE_ID_reconcile_pending,
};

__maybe_unused
static const enum btree_id reconcile_work_phys_btree[] = {
	[RECONCILE_WORK_hipri]		= BTREE_ID_reconcile_hipri_phys,
	[RECONCILE_WORK_normal]		= BTREE_ID_reconcile_work_phys,
};
"#);

c_const! {
    #[c_int]
    pub const RECONCILE_SCAN_COOKIE_device: u32 = 32;
}

c_const! {
    #[c_int]
    pub const RECONCILE_SCAN_COOKIE_stripes: u32 = 3;
}

c_const! {
    #[c_int]
    pub const RECONCILE_SCAN_COOKIE_pending: u32 = 2;
}

c_const! {
    #[c_int]
    pub const RECONCILE_SCAN_COOKIE_metadata: u32 = 1;
}

c_const! {
    #[c_int]
    pub const RECONCILE_SCAN_COOKIE_fs: u32 = 0;
}
