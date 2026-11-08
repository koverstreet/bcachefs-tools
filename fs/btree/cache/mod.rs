// SPDX-License-Identifier: GPL-2.0

/* DOC(btree-node-cache)
 *
 * In-memory btree nodes are kept in a hash table indexed by their physical
 * on-disk pointer (not their logical position). This is because btree node
 * split and compact operations are copy-on-write: new nodes are allocated, the
 * parent is updated to point to them, and the old nodes are freed. Indexing by
 * physical pointer avoids the need to atomically update the hash table during
 * these operations.
 *
 * The `struct btree` objects themselves are never freed during normal operation
 * (only at shutdown), which means locks can be dropped and retaken without
 * reference counting. This enables the aggressive lock-dropping discipline
 * that keeps btree lock hold times bounded to in-memory operations: after
 * dropping a lock, a sequence number check determines whether the node has
 * changed and needs to be re-traversed.
 *
 * Btree roots are pinned in memory and accessed directly. All other nodes may
 * be evicted from the cache and reused for different on-disk nodes at any time
 * when unlocked, so after locking a node the caller must verify it is still
 * the expected node.
 *
 * Membership state machine
 * ------------------------
 *
 * A struct btree is in exactly one of five states at any time, recorded in
 * b->cache_state and maintained by bch2_btree_node_transition_state():
 *
 *   NONE      — off all lists, NOT hashed. Either kzalloc'd and not yet
 *               placed, transiently held by one thread mid-state-change,
 *               or claimed out of the cache by a caller for private use.
 *   FREED     — bc->freed_pcpu (level-0 pcpu-readers locks) or
 *               bc->freed_nonpcpu (interior nodes), NOT hashed, b->data NULL.
 *               struct btree shell pool: lock state preserved, data buffer
 *               released. Not counted (freed lists are unbounded scratch).
 *   FREEABLE  — bc->freeable, NOT hashed, b->data set. Buffer cache for hot
 *               alloc/free churn. Counted in bc->nr_freeable.
 *   CLEAN     — bc->live[btree_node_pinned(b)].clean, hashed, b->data set,
 *               BTREE_NODE_dirty clear. Findable by lookup; the shrinker
 *               walks this list to reclaim. Counted in
 *               bc->live_nr[pinned].nr_clean and bc->nr_by_btree[].
 *   DIRTY     — bc->live[btree_node_pinned(b)].dirty, hashed, b->data set,
 *               BTREE_NODE_dirty set. Counted in bc->live_nr[pinned].nr_dirty
 *               and bc->nr_by_btree[]. Not shrinker-eligible until written
 *               out and transitioned to CLEAN.
 *
 * Roots are CLEAN or DIRTY with BTREE_NODE_permanent set; the flag tells
 * the reclaim path to skip them. Roots are also tracked via
 * bc->roots_known[]/roots_extra (under bc->root_lock).
 *
 * All transitions go through one primitive:
 *
 *   bch2_btree_node_transition_state(c, b, target)
 *      Takes bc->lock, detaches from b's current state (undoing hash/list/
 *      counter bookkeeping) then attaches to @target. Idempotent for
 *      current==target. Transition to CLEAN/DIRTY can fail (hash table
 *      insert collision) on the un-hashed→hashed transition; on failure
 *      b->hash_val is reset and b ends up in FREEABLE. Internal callers
 *      already holding bc->lock use the static _locked variant.
 *
 * The data-buffer swap inside bch2_btree_node_mem_alloc requires splitting
 * the transition: transition_state(b2, NONE) captures b2's old-state
 * bookkeeping while data is still attached, then swap(data), then
 * transition_state(b2, FREED) lists b2 with its (now-NULL) data.
 *
 * One non-state-machine transition: any live state → same state +
 * BTREE_NODE_permanent (bch2_btree_set_root_inmem just sets the flag; the
 * node stays put). Eviction (bch2_btree_node_evict) BUG_ONs on permanent
 * nodes — there's no legitimate caller path.
 *
 * Lock discipline:
 *
 *   bc->lock         — list membership, hash table mutation, counters
 *   bc->root_lock    — roots_known[]/roots_extra slot
 *   b->c.lock        — per-node six_lock (read/intent/write)
 *   bc->alloc_lock   — cmpxchg'd task pointer; serialises cannibalize so
 *                      only one thread reclaims under memory pressure
 *
 * Hash table lookups are RCU-safe; lookups can race any LIVE↔NONE
 * transition and may briefly miss a node that's hash-removed mid-flight.
 * After locking a node returned by lookup, callers must recheck b->hash_val
 * against the expected key to detect reuse.
 */

#![allow(non_camel_case_types)]

mod alloc;
mod get;
mod init;
mod reclaim;
mod state;

pub(crate) use state::NodeTable;

use crate::btree::bkey::{bkey_val_to_text, val_to_text, BkeyI, BkeySC, BkeyVal, BKEY_U64S, POS_MIN, SPOS_MAX};
use crate::data::keylist::PaddedKey;
use crate::btree::iter::{BtreeIterFlags, BtreeNodeIter, BtreeTrans};
use crate::btree::locking::bch2_btree_lock_init;
use crate::btree::node::bset::{bch2_btree_keys_stats, BsetStats, BSET_CACHELINE};
use crate::btree::node::{BtreeNode, NodeContents};
use crate::btree::path::Paths;
use crate::locking::six::six_lock_exit;
use crate::locking::MemallocFlags;
use crate::util::alloc::{kfree, kvfree_sized, kvzalloc};
use crate::util::place::{Contained, PlacedDrop};
use crate::util::shrinker::{Shrink, Shrinker};
use crate::util::sleep::Sleep;
use crate::c;
use crate::debug::trace::events;
use crate::errcode::{bch_errcode, err_ptr, ret_to_c, BchError};
use crate::prt;
use crate::util::closure::{closure_wait, closure_wake_up};
use crate::util::sched::current;
use crate::fs::Fs;
use crate::util::vstructs::Head;
use core::alloc::Layout;
use core::mem::offset_of;
use core::ops::{ControlFlow, RangeInclusive};
use core::ptr::NonNull;
use crate::util::ffi::c_char;
use crate::util::kernel::{is_vmalloc_addr, totalram_bytes};
use crate::util::{HumanReadable, Printbuf, ToText};
use core::ffi::{c_int, c_uint, c_void};
use core::sync::atomic::AtomicU64;
use core::sync::atomic::Ordering::{Relaxed, Release, SeqCst};
use cstruct_macros::{c_const, c_export, c_extern};
use crate::format::{Btree, BkeyType};

c_extern! {
    pub fn bch2_fs_btree_init_rw(c: *mut Fs) -> c_int;
    pub fn bch2_fs_btree_exit(c: *mut Fs);
    pub fn bch2_fs_btree_init_early(c: *mut Fs);
    pub fn bch2_fs_btree_init(c: *mut Fs) -> c_int;
}

impl c::btree_evicted_size {
    fn pack(hash: u64, live_u64s: u16) -> u64 {
        ((hash & c::BTREE_EVICTED_SIZE_HASH_MASK) << 16) | u64::from(live_u64s)
    }

    fn slot(&self, hash: u64) -> Option<&AtomicU64> {
        (!self.entries.is_null()).then(|| unsafe { AtomicU64::from_ptr(self.entries.add((hash & self.mask) as usize)) })
    }

    /// An evicted node's live u64s, by its pointer's hash.
    pub fn record(&self, hash: u64, live_u64s: u16) {
        if let Some(e) = self.slot(hash) {
            e.store(Self::pack(hash, live_u64s), Relaxed);
        }
    }

    /// The live u64s recorded for @hash, if its slot still has them.
    pub fn lookup(&self, hash: u64) -> Option<u16> {
        let entry = self.slot(hash)?.load(Relaxed);
        ((entry >> 16) == (hash & c::BTREE_EVICTED_SIZE_HASH_MASK)).then_some(entry as u16)
    }
}

/// # Safety
/// Nothing else has @c yet: bch2_fs_btree_init() is making it.
#[c_export]
pub unsafe fn bch2_fs_btree_evicted_size_init(c: *mut Fs) -> c_int {
    let fs = unsafe { &*c };

    /*
     * 128K-entry floor (1 MiB), then add one bit (double the table) for
     * each four doublings of the maximum possible btree node count above
     * 4M nodes (~1 TiB at 256K nodes). Keeps the table modest on huge
     * filesystems: 1 TiB → 1 MiB, 1 PiB → 4 MiB, 256 PiB → 16 MiB.
     */
    let max_nodes = fs.capacity.capacity.get() / u64::from(fs.opts.btree_node_size.get() >> 9).max(1);
    let mut bits = 17;
    if max_nodes > 1 << 22 {
        bits += (max_nodes.ilog2() - 22) / 4;
    }
    let nr = 1usize << bits;

    let Ok(entries) = kvzalloc(nr * size_of::<u64>()) else {
        return ret_to_c(fs.throw::<()>(bch_errcode::BCH_ERR_ENOMEM_fs_other_alloc));
    };
    unsafe {
        let e = &raw mut (*c).btree.evicted_size;
        (*e).entries = entries.as_ptr().cast();
        (*e).mask    = nr as u64 - 1;
    }
    0
}

/// # Safety
/// Nothing uses @c's evicted sizes after.
#[c_export]
pub unsafe fn bch2_fs_btree_evicted_size_exit(c: *mut Fs) {
    unsafe {
        let e = &raw mut (*c).btree.evicted_size;
        if let Some(p) = NonNull::new((*e).entries) {
            kvfree_sized(p.cast(), ((*e).mask as usize + 1) * size_of::<u64>());
        }
        (*e).entries = core::ptr::null_mut();
        (*e).mask    = 0;
    }
}

/// Up to a node's size, to sort a node's keys into: kvmalloc()'d, or the
/// bounce pool's, so it never fails. Freed when dropped - whichever buffer it
/// holds then: swap() trades it for a node's, which it's allocated as.
pub struct Bounce<'f> {
    fs:      &'f Fs,
    p:       NonNull<u64>,
    bytes:   usize,
    mempool: bool,
}

impl<'f> Bounce<'f> {
    pub fn new(fs: &'f Fs, bytes: usize) -> Self {
        assert!(bytes <= fs.opts.btree_node_size.get() as usize,
                "a {bytes} byte bounce buffer, past the {} byte node size", fs.opts.btree_node_size.get());

        let _noio = MemallocFlags::noio();
        let gfp = c::__GFP_ACCOUNT | c::__GFP_RECLAIMABLE;
        let (p, mempool) = match NonNull::new(alloc_raw(Mem::Kvmalloc, bytes, c::GFP_NOWAIT | gfp)) {
            Some(p) => (p.cast(), false),
            None    => (fs.btree.bounce_pool.alloc_raw(c::GFP_NOIO | gfp).cast(), true),
        };
        Self { fs, p, bytes, mempool }
    }

    pub fn u64s(&mut self) -> &mut [u64] {
        unsafe { core::slice::from_raw_parts_mut(self.p.as_ptr(), self.bytes / size_of::<u64>()) }
    }

    pub fn as_bytes(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.p.as_ptr().cast(), self.bytes) }
    }

    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.p.as_ptr().cast(), self.bytes) }
    }

    /// The node's buffer this, and this the node's old one.
    pub fn swap(&mut self, b: &BtreeNode, contents: &mut NodeContents) {
        assert!(self.bytes == b.buf_bytes(), "swapping a {} byte bounce for a {} byte node buffer",
                self.bytes, b.buf_bytes());

        let old = core::mem::replace(&mut contents.data, self.p.as_ptr().cast());
        btree_node_buf_swap_account(self.fs, old.cast(), contents.data.cast());
        self.p = NonNull::new(old.cast()).expect("a node's buffer");
    }
}

unsafe impl Send for Bounce<'_> {}

impl core::ops::Deref for Bounce<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl core::ops::DerefMut for Bounce<'_> {
    fn deref_mut(&mut self) -> &mut [u8] {
        self.as_bytes_mut()
    }
}

// Its buffer, until it's dropped: swap() trades it, but isn't reached
// through the bytes
unsafe impl crate::util::bio::MapBuf for Bounce<'_> {
    fn as_raw(&self) -> (*mut u8, usize) {
        (self.p.as_ptr().cast(), self.bytes)
    }
}

impl Drop for Bounce<'_> {
    fn drop(&mut self) {
        if self.mempool {
            unsafe { self.fs.btree.bounce_pool.free_raw(self.p.cast()) }
        } else {
            unsafe { c::rust_kvfree(self.p.as_ptr().cast()) }
        }
    }
}

// ── Node buffers ─────────────────────────────────────────────────────────

crate::module_param! {
    static mm_avoid_compaction: bool = true;
    perm = 0o644, desc = "";
}

#[derive(Clone, Copy)]
enum Mem {
    Kmalloc,
    Vmalloc,
    Kvmalloc,
}

/// @bytes of @mem, not zeroed, with @gfp: kvfree()'s to take back.
/// Userspace's is one allocator, aligned as kmalloc() aligns.
fn alloc_raw(mem: Mem, bytes: usize, gfp: c::gfp_t) -> *mut c_void {
    #[cfg(kernel)]
    unsafe {
        use kernel::bindings::{krealloc_node_align, kvrealloc_node_align, NUMA_NO_NODE};

        let gfp = crate::util::alloc::current_gfp_context_raw(gfp);
        match mem {
            Mem::Kmalloc  => krealloc_node_align(core::ptr::null(), bytes, 1, gfp, NUMA_NO_NODE).cast(),
            Mem::Vmalloc  => c::__vmalloc_noprof(bytes as _, gfp).cast(),
            Mem::Kvmalloc => kvrealloc_node_align(core::ptr::null(), bytes, 1, gfp, NUMA_NO_NODE).cast(),
        }
    }
    #[cfg(not(kernel))]
    {
        let _ = (mem, gfp);
        crate::util::alloc::kvrealloc(core::ptr::null_mut(), 0, bytes)
            .map_or(core::ptr::null_mut(), |p| p.as_ptr().cast())
    }
}

/// # Safety
/// @l is valid to write, and on no list.
unsafe fn init_list_head(l: *mut c::list_head) {
    unsafe {
        (*l).next = l;
        (*l).prev = l;
    }
}

impl c::btree_node_bufs {
    pub fn new(fs: &Fs) -> Self {
        Self {
            data:       core::ptr::null_mut(),
            aux_data:   core::ptr::null_mut(),
            byte_order: fs.opts.btree_node_size.get().ilog2(),
        }
    }

    /// Its data and aux_data, those it doesn't have yet, with @gfp: data
    /// avoiding compaction if @avoid_compaction.
    pub fn alloc(&mut self, fs: &Fs, gfp: c::gfp_t, avoid_compaction: bool) -> Result<(), BchError> {
        /*
         * We probably ought to be using __GFP_RECLAIMABLE - but vmalloc barfs.
         *
         * Shrinkable memory accounting is fubar.
         */
        let gfp = gfp | c::__GFP_ACCOUNT;

        if self.data.is_null() {
            let bytes = 1usize << self.byte_order;

            if avoid_compaction && mm_avoid_compaction.get() {
                /*
                 * Cursed hack: mm doesn't know how to limit the amount of time
                 * we spend blocked on compaction, even if we specified a
                 * vmalloc fallback.
                 *
                 * So we have to do that ourselves: only try for a high order
                 * page allocation if we're GFP_NOWAIT, otherwise straight to
                 * vmalloc.
                 */
                let mem = if gfp & c::__GFP_RECLAIM != 0 { Mem::Vmalloc } else { Mem::Kmalloc };
                self.data = alloc_raw(mem, bytes, gfp);
            }
            /*
             * mm is cursed: vmalloc can fail for no sane reason, even on 64
             * bit machines, so - fall back to the page allocator if that
             * fails
             */
            if self.data.is_null() {
                self.data = alloc_raw(Mem::Kvmalloc, bytes, gfp);
            }
            if self.data.is_null() {
                return fs.throw(bch_errcode::BCH_ERR_ENOMEM_btree_node_mem_alloc);
            }
        }

        if self.aux_data.is_null() {
            let bytes = ((1usize << self.byte_order) / BSET_CACHELINE) * size_of::<u64>();
            self.aux_data = alloc_raw(Mem::Kvmalloc, bytes, gfp);
            if self.aux_data.is_null() {
                return fs.throw(bch_errcode::BCH_ERR_ENOMEM_btree_node_mem_alloc);
            }
        }

        Ok(())
    }

    pub fn free(&mut self) {
        unsafe {
            c::rust_kvfree(self.data);
            c::rust_kvfree(self.aux_data);
        }
        self.data = core::ptr::null_mut();
        self.aux_data = core::ptr::null_mut();
    }
}

/// A struct btree: its lock initialized and nothing else, with @gfp. Freed by
/// node_struct_free().
fn node_struct_alloc(fs: &Fs, pcpu_read_locks: bool, gfp: c::gfp_t) -> Option<NonNull<BtreeNode>> {
    let layout = Layout::new::<BtreeNode>();
    #[cfg(kernel)]
    let p = unsafe {
        use kernel::bindings::{krealloc_node_align, NUMA_NO_NODE};

        krealloc_node_align(core::ptr::null(), layout.size(), layout.align(),
                            crate::util::alloc::current_gfp_context_raw(gfp | c::__GFP_ZERO), NUMA_NO_NODE)
    };
    #[cfg(not(kernel))]
    let p = {
        let _ = gfp;
        unsafe { std::alloc::alloc_zeroed(layout) }
    };
    let b = NonNull::new(p.cast::<BtreeNode>())?;

    unsafe {
        let p = b.as_ptr();
        let k = &mut (*p).contents_mut().key.key_mut().k;
        *k = c::bkey::new();
        k.set_key_type(BkeyType::btree_ptr);
        init_list_head((&raw mut (*p).write_blocked).cast());
        (*p).byte_order = fs.opts.btree_node_size.get().ilog2() as u8;
        let flags = if pcpu_read_locks {
            c::six_lock_init_flags::SIX_LOCK_INIT_PCPU
        } else {
            c::six_lock_init_flags::empty()
        };
        bch2_btree_lock_init(&raw mut (*p).c, flags, gfp);
    }
    Some(b)
}

/// node_struct_alloc(), on bc->nodes, as every node the cache has is. Freed by
/// node_mem_free().
fn cache_node_struct_alloc(fs: &Fs, pcpu_read_locks: bool, gfp: c::gfp_t) -> Option<NonNull<BtreeNode>> {
    let slot = {
        let _gfp = MemallocFlags::for_gfp(gfp);
        fs.btree.cache.nodes.get_slot().ok()?
    };

    let b = node_struct_alloc(fs, pcpu_read_locks, gfp)?;
    unsafe { (*b.as_ptr()).nodes_idx = slot.set(b.cast()) };
    Some(b)
}

/// A node with its buffers, GFP_KERNEL - off the cache's lists, not hashed -
/// from @struct_alloc.
fn node_with_bufs(fs: &Fs, struct_alloc: fn(&Fs, bool, c::gfp_t) -> Option<NonNull<BtreeNode>>)
    -> Option<NonNull<BtreeNode>>
{
    let mut bufs = c::btree_node_bufs::new(fs);
    let b = bufs.alloc(fs, c::GFP_KERNEL, false).ok().and_then(|()| struct_alloc(fs, false, c::GFP_KERNEL));
    let Some(b) = b else {
        bufs.free();
        return None;
    };
    unsafe {
        (*b.as_ptr()).contents_mut().data = bufs.data.cast();
        (*b.as_ptr()).aux_data = bufs.aux_data;
    }
    Some(b)
}

/// A node for the cache, with its buffers. Freed by node_data_free() then
/// node_mem_free().
pub fn node_mem_alloc(fs: &Fs) -> Option<NonNull<BtreeNode>> {
    node_with_bufs(fs, cache_node_struct_alloc)
}

/// A node with its buffers for node scan to read into: not the cache's, so
/// not on bc->nodes, which walks the cache's nodes while they're live and
/// can't find one freed under it. Freed by node_data_free() then
/// node_struct_free().
pub fn node_scratch_alloc(fs: &Fs) -> Option<NonNull<BtreeNode>> {
    node_with_bufs(fs, node_struct_alloc)
}

/// # Safety
/// @b is ours alone, its write done.
pub unsafe fn node_data_free(b: NonNull<BtreeNode>) {
    let p = b.as_ptr();
    ebug_assert!(!unsafe { (*p).flags.write_in_flight() }, "freeing the buffers of a node with a write in flight");

    let mut bufs = unsafe {
        c::btree_node_bufs {
            data:       (*p).contents_mut().data.cast(),
            aux_data:   (*p).aux_data,
            byte_order: (*p).byte_order as u32,
        }
    };
    if bufs.data.is_null() {
        return;
    }

    /*
     * This should really be done in slub/vmalloc, but we're using the
     * kmalloc_large() path, so we're working around a slub bug by doing
     * this here:
     */
    #[cfg(kernel)]
    unsafe {
        use crate::util::kernel::PAGE_SIZE;

        c::mm_account_reclaimed_pages(((*p).buf_bytes() / PAGE_SIZE) as _);
        if !bufs.aux_data.is_null() {
            c::mm_account_reclaimed_pages(((*p).aux_data_bytes() / PAGE_SIZE) as _);
        }
    }

    unsafe {
        (*p).flags.set_just_written(false);
        (*p).contents_mut().data = core::ptr::null_mut();
        (*p).aux_data = core::ptr::null_mut();
    }
    bufs.free();
}

/// @b, a struct btree node_struct_alloc() made, freed: its buffers already
/// are.
///
/// # Safety
/// @b is ours alone, nothing uses it after, and it's on no list.
pub unsafe fn node_struct_free(b: NonNull<BtreeNode>) {
    unsafe {
        six_lock_exit((&raw mut (*b.as_ptr()).c.lock).cast());
        kfree(b.cast(), Layout::new::<BtreeNode>());
    }
}

/// A node of the cache's freed, off bc->nodes: its buffers already are.
///
/// # Safety
/// As node_struct_free() - and nothing walks bc->nodes, which could still
/// find it: the cache's are only freed at exit.
pub unsafe fn node_mem_free(fs: &Fs, b: NonNull<BtreeNode>) {
    unsafe {
        fs.btree.cache.nodes.remove((*b.as_ptr()).nodes_idx);
        node_struct_free(b);
    }
}

/// A node pointer's identity in the node cache - a v2's seq, a v1's first
/// pointer - read as C reads it, the bytes as they are: btree_ptr_hash_val().
/// 0 for a v1 with no pointer - only a fake root's key, a node's own: see
/// node_key_hash_val().
pub fn btree_ptr_hash_val<V: ?Sized + BkeyVal>(k: BkeySC<'_, V>) -> u64 {
    let offset = match k.k.key_type() {
        BkeyType::btree_ptr    => 0,
        BkeyType::btree_ptr_v2 => offset_of!(c::BtreePtrV2, repr.seq),
        _ => return 0,
    };
    k.val_bytes().get(offset..offset + 8)
        .map_or(0, |b| u64::from_ne_bytes(b.try_into().expect("8 bytes")))
}

/// btree_ptr_hash_val() of a node's own key, which has room past its end: a
/// v1's first value u64, there or not, as C reads it - a fake root's key has
/// no value, and its identity is the u64 past it.
pub fn node_key_hash_val<const U64S: usize>(k: &PaddedKey<U64S>) -> u64 {
    match k.key().k.key_type() {
        BkeyType::btree_ptr => k.u64s()[BKEY_U64S],
        _ => btree_ptr_hash_val(BkeySC::from(k.key())),
    }
}

c_const! {
    pub const BTREE_WRITE_IO_LIMIT: u32 = 64;
}

impl c::bch_fs_btree_cache {
    pub fn update_throttle(&self) {
        let (live, dirty) = (self.lock.nr_live(), self.lock.nr_dirty());
        let throttle = self.nr_in_flight_inner.load(Relaxed) > BTREE_WRITE_IO_LIMIT as isize ||
            (live != 0 && dirty > live * 3 / 4);

        if throttle != self.should_throttle.load(Relaxed) {
            self.should_throttle.store(throttle, Relaxed);
        }
    }

    /// Ask the shrinker to free @nr nodes, as memory pressure would. Not
    /// under bc->lock: the scan takes it.
    pub fn shrink(&self, nr: core::ffi::c_ulong) {
        let _ = self.shrinkers[0].scan(nr as usize);
    }
}

impl BtreeNode {
    /// The hashed (live) cache state derived from b's flag bits. A node
    /// belongs on live[].dirty whenever there's pending work - either
    /// BTREE_NODE_dirty (needs a write) or BTREE_NODE_write_in_flight (one is
    /// outstanding); otherwise it belongs on live[].clean.
    ///
    /// Use this anywhere you need to compute the right cache_state from the
    /// current bits - passing it to transition_state_locked() is a no-op when
    /// the node is already in the right state, so callers don't need "do we
    /// need to transition?" conditionals.
    pub fn live_state(&self) -> c::btree_node_cache_state {
        let f = self.flags.load();
        if f.dirty() || f.write_in_flight() {
            c::btree_node_cache_state::BTREE_NODE_CACHE_DIRTY
        } else {
            c::btree_node_cache_state::BTREE_NODE_CACHE_CLEAN
        }
    }
}

#[c_export]
pub fn btree_node_live_state(b: &BtreeNode) -> c::btree_node_cache_state {
    b.live_state()
}

pub fn bch2_btree_node_wait_on_flag(b: &BtreeNode, bit: c::btree_flags) {
    let m = 1 << bit as u32;
    let Ok(()) = b.flags.wait_until(Sleep::UNINTERRUPTIBLE.io(), |f| f.into_bits() & m == 0);
}

impl Fs {
    /// Bytes of btree nodes cached.
    pub fn btree_cache_size(&self) -> u64 {
        let bc = &self.btree.cache;
        (bc.lock.nr_live() * self.opts.btree_node_size.get() as usize) as u64
    }
}

/// A node's buffer swapped, @old for @new: the cache counts the vmalloc'd.
fn btree_node_buf_swap_account(fs: &Fs, old: *const c_void, new: *const c_void) {
    let delta = is_vmalloc_addr(new) as isize - is_vmalloc_addr(old) as isize;

    if delta != 0 {
        let mut lists = fs.btree.cache.lock.lock();
        lists.nr_vmalloc = lists.nr_vmalloc.wrapping_add_signed(delta);
    }
}

/// What's pinned stays in memory, shrinker-exempt, until this drops. There's one
/// set of pins, the filesystem's: pin() replaces what was.
#[must_use = "everything is unpinned as soon as this drops"]
pub struct BtreeCachePins<'f>(&'f Fs);

impl Fs {
    pub fn btree_cache_pins(&self) -> BtreeCachePins<'_> {
        BtreeCachePins(self)
    }
}

impl BtreeCachePins<'_> {
    /*
     * The memory the fsck passes may spend on pinned btree nodes. They prefetch
     * and pin a keyspace range so that the random-order lookups they're about to
     * make against it become cache hits, and stop once they've spent this much -
     * pinning is always best effort, the passes are correct either way.
     */
    pub fn budget(&self) -> u64 {
        totalram_bytes() as u64 * u64::from(self.0.opts.fsck_memory_usage_percent.get()) / 100
    }

    /*
     * Prefetch and pin [start, end]: the btrees of @interior_mask down to their
     * interior nodes, and of @leaf_mask down to their leaves. Replaces any
     * previously pinned window (there is only one), walks it with prefetch so
     * the nodes are faulted in with async parallel reads rather than synchronous
     * misses, and marks them shrinker-exempt - and nodes read into the cache
     * inside the window later are pinned as they come in.
     *
     * Best-effort: pinning stops at the budget, and the window ends there; the
     * return is where - @end if it all fit. Lookups beyond it still work,
     * they're just slow paths.
     */
    pub fn pin(&self, trans: &BtreeTrans<'_>, leaf_mask: u64, interior_mask: u64,
               range: RangeInclusive<c::bbpos>) -> Result<c::bbpos, BchError> {
        let fs = self.0;
        let (start, end) = range.into_inner();
        let mut mem_may_pin = self.budget() as i64;

        self.unpin();

        let interior_mask = interior_mask | leaf_mask;
        let mut pinned_end = end;
        {
            let mut lists = fs.btree.cache.lock.lock();
            lists.pinned_nodes_mask  = [leaf_mask, interior_mask];
            lists.pinned_nodes_start = start;
            lists.pinned_nodes_end   = end;
        }

        for btree in start.btree.0..=end.btree.0 {
            if interior_mask & (1 << btree) == 0 {
                continue;
            }

            let depth = if leaf_mask & (1 << btree) != 0 { 0 } else { 1 };
            let btree_start = if btree == start.btree.0 { start.pos } else { POS_MIN };
            let btree_end   = if btree == end.btree.0 { end.pos } else { SPOS_MAX };

            let mut iter = BtreeNodeIter::new(trans, btree, btree_start, 0, depth, BtreeIterFlags::PREFETCH);
            iter.for_each(trans, |b| {
                if b.contents().data().min_key > btree_end {
                    return Ok(ControlFlow::Break(()));
                }
                mem_may_pin -= b.buf_bytes() as i64;
                if mem_may_pin <= 0 {
                    pinned_end = c::bbpos { btree: Btree(btree), pos: b.contents().key.k.p };
                    fs.btree.cache.lock.lock().pinned_nodes_end = pinned_end;
                    return Ok(ControlFlow::Break(()));
                }
                fs.btree_node_pin(&b);
                Ok(ControlFlow::Continue(()))
            })?;

            if mem_may_pin <= 0 {
                break;
            }
        }

        Ok(pinned_end)
    }

    /// Outside any window.
    pub fn node(&self, b: &BtreeNode) {
        self.0.btree_node_pin(b)
    }

    pub fn unpin(&self) {
        let mut lists = self.0.btree.cache.lock.lock();
        lists.pinned_nodes_mask = [0; 2];

        let [unpinned, pinned] = &mut lists.live;
        for (from, to) in [(&mut pinned.clean, &mut unpinned.clean), (&mut pinned.dirty, &mut unpinned.dirty)] {
            let mut cur = from.cursor_mut();
            while let Some(b) = cur.remove_current() {
                b.flags.set_pinned(false);
                unsafe { to.push_back(b) };
            }
        }

        let nr = &lists.published().live_nr;
        nr[0].nr_clean.set(nr[0].nr_clean.get() + nr[1].nr_clean.get());
        nr[0].nr_dirty.set(nr[0].nr_dirty.get() + nr[1].nr_dirty.get());
        nr[1].nr_clean.set(0);
        nr[1].nr_dirty.set(0);
    }
}

impl Fs {
    pub(crate) fn btree_node_pin(&self, b: &BtreeNode) {
        let mut lists = self.btree.cache.lock.lock();
        let is_root = self.btree_id_root(b.c.btree_id.into()).is_some_and(|r| core::ptr::eq(r, b));

        if is_root || b.flags.pinned() {
            return;
        }
        b.flags.set_pinned(true);

        let dirty = match b.cache_state(&lists) {
            c::btree_node_cache_state::BTREE_NODE_CACHE_CLEAN => false,
            c::btree_node_cache_state::BTREE_NODE_CACHE_DIRTY => true,
            _ => return,
        };

        let [unpinned, pinned] = &mut lists.live;
        let (from, to) = if dirty {
            (&mut unpinned.dirty, &mut pinned.dirty)
        } else {
            (&mut unpinned.clean, &mut pinned.clean)
        };
        unsafe {
            from.remove(b);
            to.push_back(b);
        }

        let [n0, n1] = &lists.published().live_nr;
        let (n0, n1) = if dirty { (&n0.nr_dirty, &n1.nr_dirty) } else { (&n0.nr_clean, &n1.nr_clean) };
        n0.set(n0.get() - 1);
        n1.set(n1.get() + 1);
    }
}

impl Drop for BtreeCachePins<'_> {
    fn drop(&mut self) {
        self.unpin()
    }
}

// ── The cannibalize lock ─────────────────────────────────────────────────

/*
 * We can only have one thread cannibalizing other cached btree nodes at a time,
 * or we'll deadlock. We use an open coded mutex to ensure that, which a
 * cannibalize_bucket() will take. This means every time we unlock the root of
 * the btree, we need to release this lock if we have it held.
 */
impl BtreeTrans<'_> {
    /// The cannibalize lock, this task's - or, given @cl, @cl waiting for it:
    /// btree_cache_cannibalize_lock_blocked. Without @cl,
    /// ENOMEM_btree_cache_cannibalize_lock.
    pub fn btree_cache_cannibalize_lock(&self, cl: Option<NonNull<c::closure>>) -> Result<(), BchError> {
        let fs = self.fs();
        let ret = cannibalize_lock(fs, cl);
        if ret.is_ok() {
            self.set_btree_cache_cannibalize_locked(true);
            fs.event_inc_trace(events::btree_cache_cannibalize_lock, |out| prt!(out, "{}", self.fn_name()));
        } else {
            fs.event_inc_trace(events::btree_cache_cannibalize_lock_fail, |out| prt!(out, "{}", self.fn_name()));
        }
        ret
    }

    pub fn btree_cache_cannibalize_unlock(&self) {
        let fs = self.fs();
        let bc = &fs.btree.cache;

        if bc.alloc_lock.load(Relaxed) == current() {
            fs.event_inc_trace(events::btree_cache_cannibalize_unlock, |out| prt!(out, "{}", self.fn_name()));
            bc.alloc_lock.store(core::ptr::null_mut(), Release);
            closure_wake_up(&bc.alloc_wait);
        }
        self.set_btree_cache_cannibalize_locked(false);
    }
}

fn cannibalize_lock(fs: &Fs, cl: Option<NonNull<c::closure>>) -> Result<(), BchError> {
    let bc = &fs.btree.cache;
    let me = current();
    let take = || match bc.alloc_lock.compare_exchange(core::ptr::null_mut(), me, SeqCst, Relaxed) {
        Ok(_) => true,
        Err(old) => old == me,
    };

    if take() {
        return Ok(());
    }

    let Some(cl) = cl else {
        return fs.throw(bch_errcode::BCH_ERR_ENOMEM_btree_cache_cannibalize_lock);
    };

    unsafe { closure_wait(&bc.alloc_wait, cl) };

    /* Try again, after adding ourselves to waitlist */
    if take() {
        /* We raced */
        closure_wake_up(&bc.alloc_wait);
        return Ok(());
    }

    fs.throw(bch_errcode::BCH_ERR_btree_cache_cannibalize_lock_blocked)
}

// ── The shrinkers ────────────────────────────────────────────────────────

/// What a node costs to recreate, by live list: a pinned one, 4x.
const SEEKS_MULT: [c_int; 2] = [1, 4];

impl Shrink for c::btree_cache_shrinker {
    fn shrinker(&self) -> &Shrinker<Self> {
        &self.shrink
    }

    fn count(&self) -> usize {
        self.container().container().container().btree_cache_count(self.idx.get() as usize)
    }

    fn scan(&self, nr: usize) -> Option<usize> {
        self.container().container().container().btree_cache_scan(self.idx.get() as usize, nr)
    }
}

impl PlacedDrop for c::btree_cache_shrinker {
    unsafe fn placed_drop(&self) {
        unsafe { self.shrink.unregister() }
    }
}

impl c::bch_fs_btree_cache {
    pub fn shrinkers_register(&self, fs: &Fs) -> Result<(), BchError> {
        let seeks = c_int::from(fs.opts.btree_cache_shrinker_seeks.get());
        let lists = ["btree_cache", "btree_cache-pinned"].into_iter().zip(SEEKS_MULT);
        for (s, (list, mult)) in self.shrinkers.iter().zip(lists) {
            let mut name = Printbuf::new();
            prt!(&mut name, "{}-{list}", fs.name().to_str().unwrap_or("?"));
            Shrinker::register(s, name.as_c_str(), seeks * mult)
                .map_err(|_| fs.err(bch_errcode::BCH_ERR_ENOMEM_fs_btree_cache_init))?;
        }
        Ok(())
    }

    pub fn set_shrinker_seeks(&self, seeks: c_int) {
        for (s, mult) in self.shrinkers.iter().zip(SEEKS_MULT) {
            s.shrink.set_seeks(seeks * mult);
        }
    }
}

impl Fs {
    pub fn recalc_btree_reserve(&self) {
        let mut reserve = 16;

        if self.btree_id_root(0).is_none() {
            reserve += 8;
        }

        for i in 0..self.btree_id_nr_alive() {
            if let Some(b) = self.btree_id_root(i) {
                reserve += usize::from(b.c.level).min(1) * 8;
            }
        }

        self.btree.cache.lock.lock().nr_reserve = reserve;
    }
}

// ── Text ─────────────────────────────────────────────────────────────────

#[c_export]
pub fn bch2_btree_id_str(btree: Btree) -> *const c_char {	/* avoid */
    if btree.0 < Btree::NR {
        crate::opts::c_export::__bch2_btree_ids[btree.0 as usize]
    } else {
        c"(unknown)".as_ptr().cast()
    }
}

#[c_export]
pub fn bch2_btree_id_to_text(out: &mut Printbuf, btree: Btree) {
    btree.to_text(out)
}

#[c_export]
pub fn bch2_btree_id_level_to_text(out: &mut Printbuf, btree: Btree, level: c_uint) {
    prt!(out, "btree={btree} level={level}");
}

pub fn __bch2_btree_pos_to_text(out: &mut Printbuf, fs: &Fs, btree: Btree, level: c_uint, k: BkeySC<'_>) {
    prt!(out, "{btree} level {level}/");
    let root = fs.btree.cache.roots.roots_b.get(btree.0 as usize).map_or(0, |r| r.get());
    if root != 0 {
        prt!(out, "{}", root & ((1 << c::BTREE_ROOT_LEVEL_BITS) - 1));
    } else {
        prt!(out, "(unknown)");
    }
    out.newline();

    bkey_val_to_text(out, Some(fs), k);
}

#[c_export]
pub fn bch2_btree_pos_to_text(out: &mut Printbuf, fs: &Fs, b: &BtreeNode) {
    let k = BkeySC::from(unsafe { b.contents() }.key_i());
    __bch2_btree_pos_to_text(out, fs, Btree(b.c.btree_id.into()), b.c.level.into(), k)
}

#[cold]
pub fn btree_node_to_text(out: &mut Printbuf, fs: &Fs, b: &BtreeNode) {
    let mut stats = BsetStats::default();

    bch2_btree_keys_stats(b, &mut stats);

    let contents = unsafe { b.contents() };
    let data = contents.data();
    prt!(out, "l {} {} - {}:\n", b.c.level, data.min_key, data.max_key);
    prt!(out, "    ptrs: ");
    val_to_text(out, Some(fs), BkeySC::from(contents.key_i()));
    out.newline();

    prt!(out, "    format: ");
    contents.format.to_text(out);

    let nr = &contents.nr;
    prt!(out, "    unpack fn len: {}\n", contents.unpack_fn_len);
    prt!(out, "    bytes used {}/{} ({}% full)\n",
         usize::from(nr.live_u64s) * size_of::<u64>(),
         b.buf_bytes() - size_of::<Head<c::btree_node>>(),
         usize::from(nr.live_u64s) * 100 / fs.btree_max_u64s());
    prt!(out, "    sib u64s: {}, {} (merge threshold {})\n",
         contents.sib_u64s[0].get(), contents.sib_u64s[1].get(), fs.btree.foreground_merge_threshold);
    prt!(out, "    nr packed keys {}\n", nr.packed_keys);
    prt!(out, "    nr unpacked keys {}\n", nr.unpacked_keys);
    prt!(out, "    floats {}\n", stats.floats);
    prt!(out, "    failed unpacked {}\n", stats.failed);
}

fn prt_btree_cache_line(out: &mut Printbuf, fs: &Fs, label: &str, nr: usize) {
    prt!(out, "{label}\t{} ({nr})\n", HumanReadable(nr as u64 * u64::from(fs.opts.btree_node_size.get())));
}

impl ToText for c::bch_fs_btree_cache {
    #[cold]
    fn to_text(&self, out: &mut Printbuf) {
        let fs = self.container().container();
        // racy: read without bc->lock
        let l = unsafe { &*self.lock.data_ptr() };
        let live = &self.lock.live_nr;

        if out.nr_tabstops() == 0 {
            out.tabstop_push(32);
        }

        prt_btree_cache_line(out, fs, "live:",      live[0].nr_clean.get() + live[0].nr_dirty.get());
        prt_btree_cache_line(out, fs, "pinned:",    live[1].nr_clean.get() + live[1].nr_dirty.get());
        prt_btree_cache_line(out, fs, "vmalloc:",   l.nr_vmalloc);
        prt_btree_cache_line(out, fs, "reserve:",   l.nr_reserve);
        prt_btree_cache_line(out, fs, "freeable:",  l.nr_freeable);
        prt_btree_cache_line(out, fs, "dirty:",     live[0].nr_dirty.get() + live[1].nr_dirty.get());
        prt_btree_cache_line(out, fs, "in flight:", self.nr_in_flight.load(Relaxed) as usize);
        prt!(out, "cannibalize lock:\t{}\n", if self.alloc_lock.load(Relaxed).is_null() { "not held" } else { "held" });
        out.newline();

        for (i, &nr) in l.nr_by_btree.iter().enumerate() {
            prt!(out, "{}\t{} ({nr})\n", Btree(i as u32),
                 HumanReadable(nr as u64 * u64::from(fs.opts.btree_node_size.get())));
        }

        out.newline();
        prt!(out, "counters since mount:\n");
        prt!(out, "requested:\t{}\n", l.nr_requested);
        prt!(out, "freed:\t{}\n", l.nr_freed);
        prt!(out, "self reclaim:\t{}\n", self.nr_self_reclaim.load(Relaxed));
        prt!(out, "not freed:\n");

        for (i, &nr) in l.not_freed.iter().enumerate() {
            prt!(out, "  {}\t{nr}\n", c::bch_btree_cache_not_freed_reasons::from_bits(i as u32));
        }
    }
}

// ── For check.c and init.c ───────────────────────────────────────────────

#[c_export]
pub fn bch2_btree_node_transition_state(c: &Fs, b: &BtreeNode, target: c::btree_node_cache_state) -> c_int {
    ret_to_c(c.btree_node_transition_state(b, target))
}

#[c_export]
pub fn bch2_btree_node_set_permanent(c: &Fs, b: &BtreeNode, permanent: bool) {
    let _lists = c.btree.cache.lock.lock();
    b.flags.set_permanent(permanent);
}

/// # Safety
/// @k is a key, all of its u64s there.
#[c_export]
pub unsafe fn bch2_btree_node_get_noiter(trans: &BtreeTrans<'_>, #[lent(trans)] paths: &mut Paths<'_>,
                                         k: *const c::bkey_i, btree: Btree, level: c_uint,
                                         nofill: bool) -> *mut BtreeNode {
    let k = unsafe { &*BkeyI::from_raw(k) };
    match trans.btree_node_get_noiter(paths, k, btree, level as usize, nofill) {
        Ok(b)  => b.map_or(core::ptr::null_mut(), |b| core::ptr::from_ref(b).cast_mut()),
        Err(e) => err_ptr(e),
    }
}

/// # Safety
/// @k is a key, all of its u64s there.
#[c_export]
pub unsafe fn bch2_btree_node_evict(trans: &BtreeTrans<'_>, #[lent(trans)] paths: &mut Paths<'_>,
                                    k: *const c::bkey_i) {
    trans.btree_node_evict(paths, unsafe { &*BkeyI::from_raw(k) })
}

#[c_export]
pub fn bch2_fs_btree_cache_init(c: &Fs) -> c_int {
    ret_to_c(c.btree_cache_init())
}

/// # Safety
/// Nothing uses the cache after.
#[c_export]
pub unsafe fn bch2_fs_btree_cache_exit(c: &Fs) {
    unsafe { c.btree_cache_exit() }
}
