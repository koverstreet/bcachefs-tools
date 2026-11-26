#![allow(non_camel_case_types)]

pub mod async_ops;
pub mod bbpos;
pub mod bkey;
pub mod bkey_buf;
pub mod bkey_methods;
pub mod cache;
pub mod check_allocations;
pub mod check_topology;
pub mod commit;
pub mod compat;
pub mod cow_key;
pub mod for_each;
pub mod init;
pub mod interior;
pub mod iter;
pub mod journal_overlay;
pub mod key_cache;
pub mod lock_graph;
pub mod locking;
pub mod node;
pub mod node_scan;
pub mod path;
pub mod path_node;
pub mod read;
pub mod scrub;
pub mod shard;
pub mod trans;
pub mod trans_attempt;
pub mod traverse;
pub mod trigger;
pub mod update;
pub mod validate;
pub mod write;
pub mod write_buffer;

pub use bkey::BkeySC;
pub use iter::BtreeIter;
pub use iter::BtreeNodeIter;
pub use iter::BtreeTrans;
pub use iter::BtreeIterFlags;
use crate::btree::node_scan::NodeScan;
use crate::btree::node::sort::SortRun;
use crate::new_lock;
use crate::errcode::BchError;
use crate::format::{BkeyType, Btree, BtreeFlags, btree_node};
use crate::fs::Fs;
use crate::os::workqueue::{Workqueue, WqFlags};
use crate::util::alloc::Vec;
use crate::util::mempool::Mempool;
use crate::util::ratelimit::Ratelimit;
use crate::util::late::Late;
use cstruct_macros::{bch_enum, Init, Late, MaybeZeroable, placed, TypeInfo};
use pin_init::{pin_data, PinInit};
#[cfg(kernel)]
use kernel::sync::SetOnce;
#[cfg(not(kernel))]
use bcachefs_shim::sync::SetOnce;
#[cfg(feature = "std")]
use std::str::FromStr;
#[cfg(feature = "std")]
use crate::errcode::Errcode;
use crate::btree::cache::{bch_fs_btree_cache, btree_evicted_size};
use crate::btree::interior::{bch_fs_btree_interior_updates, bch_fs_btree_reserve_cache};
use crate::btree::key_cache::bch_fs_btree_key_cache;
use crate::btree::node::BtreeWriteType;
use crate::btree::trans::bch_fs_btree_trans;
use crate::btree::write::btree_write_stats;
use crate::btree::write_buffer::{bch_fs_btree_write_buffer, WbBtree};
use crate::journal::journal::journal_entry_res;
use crate::opts::bch_opts;

#[placed(Fs.btree)]
#[pin_data]
#[repr(C)]
#[derive(MaybeZeroable, TypeInfo, Init, Late)]
#[init_args(opts: &bch_opts)]
pub struct bch_fs_btree {
    #[init((btree_max_u64s(opts.btree_node_size) / 3) as u16)]
    pub foreground_merge_threshold: u16,

    #[pin]
    #[init(Mempool::new_in(crate::btree::cache::bounce_bufs(), 1, opts.btree_node_size as usize), ENOMEM_fs_other_alloc)]
    pub bounce_pool: Mempool<crate::btree::cache::BounceBufs>,

    pub write_stats: [btree_write_stats; BtreeWriteType::NR as usize],

    #[pin]
    #[init(fill_iter_pool(opts), ENOMEM_fs_other_alloc)]
    pub fill_iter: Mempool,
    #[pin]
    #[init(Workqueue::new(c"bcachefs_btree_read_complete",
                          WqFlags::HIGHPRI | WqFlags::FREEZABLE | WqFlags::MEM_RECLAIM | WqFlags::PERCPU, 512),
           ENOMEM_fs_other_alloc)]
    pub read_complete_wq: Workqueue,
    #[init(Ratelimit::new())]
    pub read_errors_soft: Ratelimit,
    #[init(Ratelimit::new())]
    pub read_errors_hard: Ratelimit,

    #[late(Workqueue::new(c"bcachefs_btree_write_complete",
                          WqFlags::HIGHPRI | WqFlags::FREEZABLE | WqFlags::MEM_RECLAIM | WqFlags::PERCPU, 1),
           ENOMEM_fs_other_alloc)]
    pub write_complete_wq: Late<Workqueue>,

    pub root_journal_res: journal_entry_res,

    #[pin]
    pub cache: bch_fs_btree_cache,
    #[init(btree_evicted_size { entries: SetOnce::new() })]
    pub evicted_size: btree_evicted_size,
    #[pin]
    pub key_cache: bch_fs_btree_key_cache,
    /*
     * One per BTREE_IS_write_buffer btree (indexed by BCH_WB_BTREE_*, see
     * bch_wb_btree_idx()). Each instance has its own intake/flushing
     * buffers and locks.
     */
    #[pin]
    pub write_buffer: [bch_fs_btree_write_buffer; WbBtree::NR as usize],
    /*
     * Per-btree flush_work runs on write_buffer_wq; once the sorted key
     * list crosses a threshold the flush parallelizes across CPUs by
     * queuing sub-shards on write_buffer_shard_wq and closure_sync()ing.
     * The two must be separate workqueues — the outer flush worker
     * blocks waiting for the inner shards to complete, so they can't
     * share a wq. WQ_MEM_RECLAIM on both because the flush sits in the
     * journal-reclaim path.
     *
     * write_buffer_wq is freezable: a flush does btree commits, and those
     * submit btree node writes from the flushing context - after a
     * hibernate snapshot that is a write the image doesn't know about.
     * The shard wq must NOT be: a flush in progress when the freeze starts
     * waits on its shards, and freeze_workqueues_busy() waits on the
     * flush. Shards are only queued by a running flush, so once the outer
     * wq is frozen no new ones appear.
     */
    #[late(Workqueue::new(c"bcachefs_wb_flush",
                          WqFlags::UNBOUND | WqFlags::MEM_RECLAIM | WqFlags::FREEZABLE, 0),
           ENOMEM_fs_other_alloc)]
    pub write_buffer_wq: Late<Workqueue>,
    #[late(Workqueue::new(c"bcachefs_wb_flush_shard", WqFlags::UNBOUND | WqFlags::MEM_RECLAIM, 0), ENOMEM_fs_other_alloc)]
    pub write_buffer_shard_wq: Late<Workqueue>,
    /*
     * Sync flushers (btree_write_buffer_flush_seq) wake the per-btree
     * flush_works and then wait here for the worker to drain pins past
     * their target seq. Waked from the flush worker after drain and from
     * journal_keys_to_write_buffer_end after pin drops.
     */
    pub write_buffer_flush_wait: crate::util::futex::EventCount,
    #[pin]
    pub trans: bch_fs_btree_trans,
    #[pin]
    #[init(bch_fs_btree_reserve_cache::init())]
    pub reserve_cache: bch_fs_btree_reserve_cache,
    #[pin]
    #[late]
    pub interior_updates: bch_fs_btree_interior_updates,
    #[pin]
    #[init(crate::btree::async_ops::AsyncOps::init())]
    #[late]
    pub(crate) async_ops: crate::btree::async_ops::AsyncOps,
    #[pin]
    #[init(new_lock!(Vec::new()))]
    pub(crate) node_scan: NodeScan,
}

/// What a node of @btree_node_size bytes has room for, past its header.
pub fn btree_max_u64s(btree_node_size: u32) -> usize {
    (btree_node_size as usize - size_of::<crate::util::vstructs::Head<btree_node>>()) / size_of::<u64>()
}

fn fill_iter_pool(opts: &bch_opts) -> impl PinInit<Mempool, BchError> {
    let btree_blocks = (opts.btree_node_size as usize >> 9) >> (opts.block_size as usize >> 9).ilog2();
    Mempool::new(1, (btree_blocks + 1) * 2 * size_of::<SortRun>())
}

/* Btree ID properties: */
#[bch_enum(open)]
#[repr(u32)]
pub enum BtreeNodeType {
    btree,
}

impl BtreeNodeType {
    /* Type of a key in btree @id at level @level: */
    pub const fn of(level: u32, btree: Btree) -> Self {
        if level != 0 { Self::btree } else { Self(btree.0 + 1) }
    }
}

impl Btree {
    pub const fn trigger_order(self) -> u8 {
        match self {
            Btree::alloc   => u8::MAX,
            Btree::stripes => u8::MAX - 1,
            _              => self.0 as u8,
        }
    }
}

impl Btree {
    /// The key types it may hold, a bit each - deleted, which every btree
    /// may, and its own: bch2_key_types_allowed[]. 0 for a btree this code
    /// doesn't know.
    pub const fn key_types(self) -> u64 {
        if self.0 < Self::NR {
            self.holds() | 1 << BkeyType::deleted.0
        } else {
            0
        }
    }

    /// Whether it's an extents btree with snapshots:
    /// btree_id_is_extents_snapshots().
    pub const fn is_extents_snapshots(self) -> bool {
        self.flags().contains(BtreeFlags::extents.union(BtreeFlags::snapshots))
    }

    /// Whether its updates go through the btree write buffer.
    pub const fn uses_write_buffer(self) -> bool {
        self.flags().contains(BtreeFlags::write_buffer)
    }

    /// Convert from raw u32. Returns None for unknown built-in btree IDs.
    pub fn from_raw(id: u32) -> Option<Self> {
        crate::BTREE_IDS_KNOWN.get(id as usize).copied()
    }
}

impl From<Btree> for u32 {
    fn from(id: Btree) -> u32 {
        id.0
    }
}

/// Get a btree ID name string.
#[cfg(feature = "std")]
pub fn btree_id_str(id: u32) -> String {
    Btree(id).to_string()
}

#[cfg(feature = "std")]
impl FromStr for Btree {
    type Err = BchError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        <Btree as crate::util::names::Names>::ENTRIES.iter().find(|&&(n, _)| n == s)
            .and_then(|&(_, v)| Btree::from_raw(v as u32))
            .ok_or(Errcode::EINVAL_parse_btree_id.into())
    }
}

#[cfg(feature = "std")]
impl FromStr for BkeyType {
    type Err = BchError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        crate::util::names::value_of(<Self as crate::util::names::Names>::ENTRIES, s)
            .map(|v| BkeyType(v as u32))
            .ok_or_else(|| Errcode::EINVAL_parse_bkey_type.into())
    }
}

