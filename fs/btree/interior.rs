// SPDX-License-Identifier: GPL-2.0

//! Interior updates - splits, merges, rewrites: their nodes allocated from a
//! reserve, linked into the parent, the old ones freed - and the btree roots,
//! as journal entries: what every journal write, and the clean section,
//! records of where each btree's root is.

#![allow(non_camel_case_types)]

use core::ffi::{c_int, c_uint};
use core::marker::PhantomData;
use core::mem::{size_of, ManuallyDrop};
use core::ops::Deref;
use core::ptr::{self, NonNull};
use cstruct_macros::{CStruct, Init, Late, c_const, c_extern, TypeInfo};
use crate::btree::iter::{BtreeTrans, CommitOpts};
use crate::btree::node::{BtreeNode, NodeContents};
use crate::btree::node::keys::KeysMut;
use crate::btree::bkey::{BkeyI, BkeyV, BKEY_U64S};
use crate::btree::interior_c::*;
use crate::btree::path::{LockedNode, PathIdx, Paths};
use crate::c;
use crate::data::keylist::PaddedKey;
use crate::errcode::{errptr_to_result, ret_to_result_void, BchError};
use crate::format::Bpos;
use crate::fs::Fs;
use crate::journal::entries::{EntryWriter, NoRoom};
use crate::locking::{ExclusiveGuard, Locked, RawLock};
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

/// What an interior update reserves for, and records: InteriorUpdate::start()'s.
#[derive(Default)]
pub(crate) struct StartOpts {
    /// The node at the first level splits, not just gets rewritten
    pub split:        bool,
    pub target:       u32,
    pub commit:       CommitOpts,
    pub write:        c::bch_write_flags,
    /// split_leaf()'s: the key that didn't fit, so btree_split() doesn't
    /// compact a leaf that still wouldn't have room for it
    pub new_key_u64s: u32,
    /// The nodes being replaced, where they aren't just @path's: a merge's
    pub range:        Option<(Bpos, Bpos)>,
}

/// An interior update in its foreground phase: reserved, in @trans. Dropped,
/// it's rolled back; done() hands it to its completion, which commits the new
/// nodes' pointers once they're written.
pub(crate) struct InteriorUpdate<'t> {
    trans: &'t BtreeTrans<'t>,
    raw:   NonNull<c::btree_update>,
}

/// A node from an interior update's reserve: intent and write locked, not yet
/// reachable.
pub(crate) struct NewNode<'u> {
    b:       NonNull<BtreeNode>,
    _update: PhantomData<&'u ()>,
}

impl Deref for NewNode<'_> {
    type Target = BtreeNode;

    fn deref(&self) -> &BtreeNode {
        unsafe { self.b.as_ref() }
    }
}

impl NewNode<'_> {
    pub(crate) fn locked(&self) -> LockedNode<'_, BtreeNode> {
        unsafe { LockedNode::new(self.b.as_ref()) }
    }

    pub(crate) fn contents(&self) -> &NodeContents {
        unsafe { self.b.as_ref().contents() }
    }

    pub(crate) fn contents_mut(&mut self) -> &mut NodeContents {
        unsafe { self.b.as_ref().contents_mut() }
    }

    pub(crate) fn keys_mut(&mut self) -> KeysMut<'_> {
        let b = unsafe { self.b.as_ref() };
        b.keys_mut(unsafe { b.contents_mut() })
    }
}

impl<'t> InteriorUpdate<'t> {
    /// An update for @path's node at @level, its reserve the worst case: every
    /// level above splitting, up to a new root.
    pub(crate) fn start(trans: &'t BtreeTrans<'t>, _paths: &mut Paths<'_>, path: PathIdx, level: usize,
                        opts: StartOpts) -> Result<Self, BchError> {
        let p = unsafe {
            bch2_btree_update_start(trans.raw(), path, level as u32, opts.split, opts.target,
                                    opts.commit.to_c(), opts.write)
        };
        let mut raw = errptr_to_result(NonNull::new(p).expect("bch2_btree_update_start(): an update or an error"))?;

        let as_ = unsafe { raw.as_mut() };
        as_.new_key_u64s = opts.new_key_u64s;
        if let Some((start, end)) = opts.range {
            as_.node_start = start;
            as_.node_end = end;
        }
        Ok(Self { trans, raw })
    }

    fn new_node(&self, b: *mut BtreeNode) -> NewNode<'_> {
        NewNode { b: NonNull::new(b).expect("a node from the reserve"), _update: PhantomData }
    }

    pub(crate) fn alloc_node(&self, _paths: &mut Paths<'_>, level: usize) -> NewNode<'_> {
        self.new_node(unsafe { bch2_btree_node_alloc(self.raw.as_ptr(), self.trans.raw(), level as u32) })
    }

    /// A copy of @b, compacted.
    pub(crate) fn alloc_replacement(&self, _paths: &mut Paths<'_>, b: &BtreeNode) -> NewNode<'_> {
        let b = ptr::from_ref(b).cast_mut();
        self.new_node(unsafe { bch2_btree_node_alloc_replacement(self.raw.as_ptr(), self.trans.raw(), b) })
    }

    pub(crate) fn alloc_root(&self, _paths: &mut Paths<'_>, level: u32) -> NewNode<'_> {
        self.new_node(unsafe { __btree_root_alloc(self.raw.as_ptr(), self.trans.raw(), level) })
    }

    /// A path to @n, holding it.
    pub(crate) fn take_path(&self, paths: &mut Paths<'_>, n: &NewNode<'_>) -> PathIdx {
        let pos = n.contents().key_i().k().p;
        let idx = self.trans.path_get_unlocked_mut(paths, Btree(n.c.btree_id.into()), n.c.level.into(),
                                                   pos, false, crate::ret_ip!());
        unsafe { btree_path_take_new_node(self.trans.raw(), &raw mut paths[idx], n.b.as_ptr()) };
        idx
    }

    fn push_parent_key(&self, k: &c::bkey_i) {
        let as_ = unsafe { &mut *self.raw.as_ptr() };
        let u64s = usize::from(k.k().u64s);
        let end = as_.inline_keys.as_mut_ptr_range().end;

        assert!(as_.parent_keys.keys_p == as_.inline_keys.as_mut_ptr() &&
                as_.parent_keys.top_p.wrapping_add(u64s) <= end,
                "an interior update's parent keys past its inline keys");
        unsafe {
            ptr::copy_nonoverlapping(ptr::from_ref(k).cast::<u64>(), as_.parent_keys.top_p, u64s);
            as_.parent_keys.top_p = as_.parent_keys.top_p.add(u64s);
        }
    }

    /// @n's pointer, its sectors_written stamped, added to what the parent
    /// gets.
    pub(crate) fn link(&self, n: &NewNode<'_>) {
        unsafe { bch2_btree_update_emit_new_node_key(self.raw.as_ptr(), n.b.as_ptr()) };
        self.push_parent_key(n.contents().key_i());
    }

    /// The pointer at @pos to a node at @level deleted from the parent, and
    /// the deletion journalled with the update.
    pub(crate) fn unlink(&self, pos: Bpos, level: usize) {
        let mut delete = c::bkey_i { k: c::bkey { p: pos, ..c::bkey::new() }, ..Default::default() };

        self.push_parent_key(unsafe { &*BkeyI::from_raw(&raw const delete) });
        unsafe { bch2_btree_update_add_key(&raw mut (*self.raw.as_ptr()).new_nodes, level as u32, &raw mut delete) };
    }

    /// What link() added, inserted into @path's node above @level - splitting
    /// it, and up, if it doesn't fit.
    pub(crate) fn insert_into_parent(&self, paths: &mut Paths<'_>, path: PathIdx,
                                     level: usize) -> Result<(), BchError> {
        let parent = paths[path].locked_btree_node(level + 1).expect("an interior update's node with no parent");
        let parent = ptr::from_ref(parent.node()).cast_mut();
        ret_to_result_void(unsafe {
            bch2_btree_insert_node(self.raw.as_ptr(), self.trans.raw(), path, parent,
                                   &raw mut (*self.raw.as_ptr()).parent_keys)
        })
    }

    /// @n made @path's btree's root, its sectors_written stamped.
    pub(crate) fn set_root(&self, paths: &mut Paths<'_>, path: PathIdx, n: &NewNode<'_>) {
        unsafe {
            bch2_btree_update_emit_new_node_key(self.raw.as_ptr(), n.b.as_ptr());
            bch2_btree_set_root(self.raw.as_ptr(), self.trans.raw(), &raw mut paths[path], n.b.as_ptr());
        }
    }

    /// @old - each held by its path - swapped for @new, linked in: the old
    /// nodes freed in memory before the new are visible to the transaction's
    /// other paths, so nothing relocks an old node and sees what the new one
    /// replaced. @new in the order they're written.
    pub(crate) fn replace(&self, paths: &mut Paths<'_>, old: &[(PathIdx, LockedNode<'_, BtreeNode>)],
                          new: &[NewNode<'_>]) {
        let as_ = self.raw.as_ptr();
        let trans = self.trans.raw();
        let ptr = |b: LockedNode<'_, BtreeNode>| ptr::from_ref(b.node()).cast_mut();

        for &(_, b) in old {
            unsafe { bch2_btree_interior_update_will_free_node(as_, ptr(b)) };
        }
        for n in new {
            unsafe { bch2_btree_update_write_new_node(as_, trans, n.b.as_ptr()) };
        }
        for &(idx, b) in old {
            unsafe { bch2_btree_node_free_inmem(trans, &raw mut paths[idx], ptr(b)) };
        }
        for n in new {
            self.trans.node_add(paths, n.locked());
        }
        for &(_, b) in old {
            self.trans.node_verify_not_in_iters(paths, &b);
        }
    }

    pub(crate) fn done(self, _paths: &mut Paths<'_>) {
        let this = ManuallyDrop::new(self);
        unsafe { bch2_btree_update_done(this.raw.as_ptr(), this.trans.raw()) }
    }
}

impl Drop for InteriorUpdate<'_> {
    fn drop(&mut self) {
        unsafe { bch2_btree_update_free(self.raw.as_ptr(), self.trans.raw(), true) }
    }
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

    pub fn btree_interior_updates_pending(&self) -> bool {
        let u = &self.btree.interior_updates;
        unsafe {
            u.lock.lock();
            let pending = !ptr::eq(u.list.next, &u.list);
            u.lock.unlock(());
            pending
        }
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
        w.push(c::bch_jset_entry_type::btree_root, id as u8, r.level, &[r.key.key().as_bytes()])?;
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
