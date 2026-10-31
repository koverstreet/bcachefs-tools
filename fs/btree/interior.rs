// SPDX-License-Identifier: GPL-2.0

//! The btree roots, as journal entries: what every journal write, and the
//! clean section, records of where each btree's root is.

#![allow(non_camel_case_types)]

use core::ffi::{c_ulong, c_int, c_uint};
use core::mem::size_of;
use cstruct_macros::{c_export, CStruct, Init, c_const, c_extern, TypeInfo};
use zerocopy::IntoBytes;
use crate::c;
use crate::fs::Fs;
use crate::journal::entries::{EntryWriter, NoRoom};
use crate::locking::{Guard, Locked};
use crate::util::ffi::Opaque;
use crate::util::vstructs::{plain_bytes, Head};
use crate::new_lock;
use crate::types::c_default;
use crate::util::Printbuf;
use core::sync::atomic::AtomicI32;
use pin_init::pin_data;
use crate::format::Btree;

#[repr(C)]
#[derive(Default, CStruct)]
pub struct btree_alloc {
    pub ob: c::open_buckets,
    pub k: c::bkey_i,
    pub k_pad: [u64; c::BKEY_BTREE_PTR_VAL_U64s_MAX],
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
#[derive(CStruct, TypeInfo)]
pub struct bch_fs_btree_interior_updates {
    pub pool: c::mempool_t,
    pub list: c::list_head,
    pub unwritten: c::list_head,
    pub lock: c::mutex,
    pub commit_lock: c::mutex,
    pub wait: c::closure_waitlist,

    pub worker: *mut c::workqueue_struct,
    pub work: c::work_struct,
}
c_default!(bch_fs_btree_interior_updates);

#[repr(C)]
#[derive(CStruct, TypeInfo)]
pub struct bch_fs_btree_node_rewrites {
    pub list: c::list_head,
    pub pending: c::list_head,
    pub lock: c::spinlock_t,
    pub wait: c::closure_waitlist,
    pub nr: AtomicI32,
    pub table: c::rhashtable,
    pub table_init_done: bool,
    pub worker: *mut c::workqueue_struct,
}
c_default!(bch_fs_btree_node_rewrites);

c_extern! {
    pub fn bch2_btree_interior_updates_flush(c: *mut c::bch_fs) -> bool;
    pub fn bch2_btree_updates_to_text(out: *mut c::printbuf, c: *mut c::bch_fs);
    pub fn bch2_btree_node_update_key(trans: *mut c::btree_trans, iter: *mut c::btree_iter,
                                      b: *mut c::BtreeNode, new_key: *mut c::bkey_i,
                                      commit_flags: c_uint, skip_triggers: bool) -> c_int;
    pub fn bch2_btree_node_update_key_at_pos(trans: *mut c::btree_trans, btree: Btree, level: c_uint,
                                             new_key: *mut c::bkey_i) -> c_int;
    pub fn bch2_btree_node_rewrite(trans: *mut c::btree_trans, iter: *mut c::btree_iter, b: *mut c::BtreeNode,
                                   target: c_uint, flags: c::bch_trans_commit_flags,
                                   write_flags: c::bch_write_flags) -> c_int;
    pub fn bch2_btree_reserve_cache_to_text(out: *mut c::printbuf, c: *mut c::bch_fs);
    pub fn bch2_async_btree_op(c: *mut c::bch_fs, b: *mut c::BtreeNode, op: c::async_btree_op);
}

impl Fs {
    pub fn btree_reserve_cache_to_text(&self, out: &mut Printbuf) {
        unsafe { bch2_btree_reserve_cache_to_text(out.as_raw(), self.as_ptr()) }
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
        unsafe { c::bch2_queue_work(u.worker, (&raw const u.work).cast_mut()) };
    }
}

type RootsGuard<'a> = Guard<'a, c::mutex, c::BtreeRoots, c::BtreeRootsLive>;

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

/// @r's key: its bkey_i, and its value on into key_pad, as long as it says.
fn key_parts(r: &c::btree_root) -> [&[u8]; 2] {
    let bytes = usize::from(r.key.k.u64s) * size_of::<u64>();
    let head = plain_bytes(&r.key);
    [head, &r.key_pad.as_bytes()[..bytes - head.len()]]
}

/// An entry's bytes, for @r's key.
fn entry_bytes(r: &c::btree_root) -> usize {
    size_of::<Head<c::jset_entry>>() + usize::from(r.key.k.u64s) * size_of::<u64>()
}

fn push_roots(fs: &Fs, root_lock: &RootsGuard<'_>, skip: u64, w: &mut EntryWriter<'_>)
    -> Result<(), NoRoom>
{
    for (id, r) in roots(fs, root_lock, skip) {
        w.push(c::bch_jset_entry_type::BCH_JSET_ENTRY_btree_root, id as u8, r.level, &key_parts(r))?;
    }
    Ok(())
}

/// Each live btree root as a btree_root journal entry, but those in @skip.
pub fn btree_roots_to_journal_entries(fs: &Fs, w: &mut EntryWriter<'_>, skip: u64) -> Result<(), NoRoom> {
    push_roots(fs, &fs.btree.cache.roots.lock(), skip, w)
}

/// btree_roots_to_journal_entries(), for C's journal write: at @end, the end
/// of what's written, which it returns.
///
/// # Safety
/// There's room at @end for an entry for every live root.
#[c_export]
pub unsafe fn bch2_btree_roots_to_journal_entries(c: &Opaque<c::bch_fs>, end: *mut Head<c::jset_entry>,
                                                  skip: c_ulong) -> *mut Head<c::jset_entry> {
    let fs = Fs::from_c(c);
    let root_lock = fs.btree.cache.roots.lock();
    let bytes = roots(fs, &root_lock, skip.into()).map(|(_, r)| entry_bytes(r)).sum();

    // SAFETY: the caller's - room for these
    let mut w = EntryWriter::new(unsafe { core::slice::from_raw_parts_mut(end.cast(), bytes) });
    push_roots(fs, &root_lock, skip.into(), &mut w).expect("sized for them, under the same lock");
    // SAFETY: as long as what was written
    unsafe { end.byte_add(bytes) }
}
