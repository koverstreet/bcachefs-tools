// SPDX-License-Identifier: GPL-2.0

//! The btree roots, as journal entries: what every journal write, and the
//! clean section, records of where each btree's root is.

#![allow(non_camel_case_types)]

use core::ffi::{c_int, c_uint};
use core::mem::size_of;
use cstruct_macros::{CStruct, Init, Late, c_const, c_extern, TypeInfo};
use crate::btree::iter::BtreeTrans;
use crate::btree::node::BtreeNode;
use crate::btree::bkey::BKEY_U64S;
use crate::c;
use crate::data::keylist::PaddedKey;
use crate::fs::Fs;
use crate::journal::entries::{EntryWriter, NoRoom};
use crate::locking::{ExclusiveGuard, Locked};
use crate::new_lock;
use crate::types::c_default;
use crate::util::Printbuf;
use crate::util::{late::Late, mempool::Mempool, workqueue::{Workqueue, WqFlags}};
use core::sync::atomic::AtomicI32;
use pin_init::pin_data;
use crate::format::Btree;

#[repr(C)]
#[derive(Default, CStruct)]
pub struct btree_alloc {
    pub ob: c::open_buckets,
    #[c_anon("struct { struct bkey_i k; u64 k_pad[BKEY_BTREE_PTR_VAL_U64s_MAX]; }")]
    pub k: PaddedKey<{ BKEY_U64S + c::BKEY_BTREE_PTR_VAL_U64s_MAX }>,
}

c_const! {
    /* Maximum number of nodes we might need to allocate atomically: */
    #[c_int]
    pub const BTREE_RESERVE_MAX: u32 = c::BTREE_MAX_DEPTH as u32 + (c::BTREE_MAX_DEPTH as u32 - 1);

    /* Size of the freelist we allocate btree nodes from: */
    #[c_int]
    pub const BTREE_NODE_RESERVE: u32 = c::BTREE_RESERVE_MAX * 4;
}

/*
 * Cache of allocated btree nodes - if we allocate a btree node and don't use
 * it, if we free it that space can't be reused until going _all_ the way
 * through the allocator (which exposes us to a livelock when allocating btree
 * reserves fail halfway through) - instead, we can stick them here:
 */
#[repr(C)]
#[derive(CStruct)]
pub struct BtreeReserveCache {
    pub nr: c_uint,
    #[c("struct btree_alloc data[BTREE_NODE_RESERVE * 2]")]
    pub data: [c::btree_alloc; c::BTREE_NODE_RESERVE as usize * 2],
}

#[pin_data]
#[repr(C)]
#[derive(CStruct, TypeInfo, Init)]
pub struct bch_fs_btree_reserve_cache {
    #[pin]
    #[init(new_lock!(<- ::pin_init::init_zeroed(), (), "&c->btree.reserve_cache.lock"))]
    #[c_lock("struct mutex lock")]
    pub lock: Locked<c::mutex, BtreeReserveCache>,
}

#[repr(C)]
#[derive(CStruct, TypeInfo, Late)]
pub struct bch_fs_btree_interior_updates {
    #[late(Mempool::kmalloc(1, size_of::<c::btree_update>()), ENOMEM_btree_interior_update_pool_init)]
    #[c("mempool_t pool")]
    pub pool: Late<Mempool>,
    pub list: c::list_head,
    pub unwritten: c::list_head,
    pub lock: c::mutex,
    pub commit_lock: c::mutex,
    pub wait: c::closure_waitlist,

    #[late(Workqueue::new(c"btree_update", WqFlags::UNBOUND | WqFlags::MEM_RECLAIM, 8), ENOMEM_btree_interior_update_worker_init)]
    #[c("struct workqueue_struct *worker")]
    pub worker: Late<Workqueue>,
    pub work: c::work_struct,
}
c_default!(bch_fs_btree_interior_updates);

#[repr(C)]
#[derive(CStruct, TypeInfo, Late)]
pub struct bch_fs_btree_node_rewrites {
    pub list: c::list_head,
    pub pending: c::list_head,
    pub lock: c::spinlock_t,
    pub wait: c::closure_waitlist,
    pub nr: AtomicI32,
    pub table: c::rhashtable,
    pub table_init_done: bool,
    #[late(Workqueue::new(c"btree_node_rewrite", WqFlags::PERCPU, 0), ENOMEM_btree_interior_update_worker_init)]
    #[c("struct workqueue_struct *worker")]
    pub worker: Late<Workqueue>,
}
c_default!(bch_fs_btree_node_rewrites);

c_extern! {
    pub fn bch2_btree_interior_updates_flush(c: *mut Fs) -> bool;
    pub fn bch2_do_pending_node_rewrites(c: *mut Fs);
    pub fn bch2_free_pending_node_rewrites(c: *mut Fs);
    pub fn bch2_btree_updates_to_text(out: *mut Printbuf, c: *mut Fs);
    pub fn bch2_btree_node_update_key(trans: *mut BtreeTrans<'_>, iter: *mut c::btree_iter,
                                      b: *mut BtreeNode, new_key: *mut c::bkey_i,
                                      commit_flags: c_uint, skip_triggers: bool) -> c_int;
    pub fn bch2_btree_node_update_key_at_pos(trans: *mut BtreeTrans<'_>, btree: Btree, level: c_uint,
                                             new_key: *mut c::bkey_i) -> c_int;
    pub fn bch2_btree_node_rewrite(trans: *mut BtreeTrans<'_>, iter: *mut c::btree_iter, b: *mut BtreeNode,
                                   target: c_uint, flags: c::bch_trans_commit_flags,
                                   write_flags: c::bch_write_flags) -> c_int;
    pub fn bch2_btree_reserve_cache_to_text(out: *mut Printbuf, c: *mut Fs);
    pub fn bch2_async_btree_op(c: *mut Fs, b: *mut BtreeNode, op: c::async_btree_op);
    pub fn bch2_btree_root_alloc_fake(c: *mut Fs, btree: Btree, level: c_uint);
    pub fn bch2_async_btree_node_rewrites_flush(c: *mut Fs);
}

impl Fs {
    pub fn btree_reserve_cache_to_text(&self, out: &mut Printbuf) {
        unsafe { bch2_btree_reserve_cache_to_text(out.as_raw(), self.as_ptr()) }
    }

    pub fn btree_root_alloc_fake(&self, btree: Btree, level: u32) {
        unsafe { bch2_btree_root_alloc_fake(self.as_ptr(), btree, level) }
    }

    pub fn async_btree_node_rewrites_flush(&self) {
        unsafe { bch2_async_btree_node_rewrites_flush(self.as_ptr()) }
    }

    /// Waits for the interior updates in flight to finish - with no btree
    /// locks held, as they may need them; whether there were any.
    pub fn btree_interior_updates_flush(&self) -> bool {
        // SAFETY: a live filesystem
        unsafe { bch2_btree_interior_updates_flush(self.as_ptr()) }
    }

    /// Run the interior update worker now.
    pub fn btree_interior_updates_kick(&self) {
        let u = &self.btree.interior_updates;
        unsafe { u.worker.queue_work((&raw const u.work).cast_mut()) };
    }
}

type RootsGuard<'a> = ExclusiveGuard<'a, c::mutex, c::BtreeRoots, c::BtreeRootsLive>;

/// The live roots, but those whose bit is set in @skip - a btree past bit 63
/// can't be. Under root_lock, which owns their on-disk fields: key, level,
/// alive.
fn roots<'a>(fs: &'a Fs, root_lock: &'a RootsGuard<'_>, skip: u64)
    -> impl Iterator<Item = (u32, &'a c::btree_root)>
{
    (0..fs.btree_id_nr_alive())
        .filter(move |&id| id >= u64::BITS || skip & (1 << id) == 0)
        .filter_map(move |id| root_lock.get(id).filter(|r| r.alive != 0).map(|r| (id, r)))
}

fn push_roots(fs: &Fs, root_lock: &RootsGuard<'_>, skip: u64, w: &mut EntryWriter<'_>)
    -> Result<(), NoRoom>
{
    for (id, r) in roots(fs, root_lock, skip) {
        w.push(c::bch_jset_entry_type::BCH_JSET_ENTRY_btree_root, id as u8, r.level, &[r.key.key().as_bytes()])?;
    }
    Ok(())
}

/// Each live btree root as a btree_root journal entry, but those in @skip.
pub fn btree_roots_to_journal_entries(fs: &Fs, w: &mut EntryWriter<'_>, skip: u64) -> Result<(), NoRoom> {
    push_roots(fs, &fs.btree.cache.roots.lock(), skip, w)
}

/// What every journal entry reserves for @nr btrees' roots, each at its
/// largest: root_journal_res.
pub(crate) fn roots_journal_u64s(nr: u32) -> c_uint {
    nr * (c::JSET_KEYS_U64s + c::BKEY_BTREE_PTR_U64s_MAX) as c_uint
}
