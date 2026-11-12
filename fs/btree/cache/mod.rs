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
 * node stays put). Eviction (node_evict()) BUG_ONs on permanent
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
mod bufs;
mod get;
mod init;
mod pins;
mod reclaim;
mod state;

use bufs::{cache_node_struct_alloc, cached_node, new_node_lock, NodeBufs};
pub(crate) use alloc::UnhashedNode;
pub use bufs::{node_data_free, node_mem_alloc, node_mem_free, node_scratch_alloc, node_struct_free, Bounce};
pub(crate) use bufs::bounce_pool_alloc;
pub use pins::BtreeCachePins;
pub(crate) use state::NodeTable;

use crate::btree::bkey::{bkey_val_to_text, val_to_text, BkeySC, BkeyVal, BKEY_U64S};
use crate::data::keylist::PaddedKey;
use crate::btree::iter::BtreeTrans;
use crate::btree::node::bset::{bch2_btree_keys_stats, BsetStats};
use crate::btree::node::BtreeNode;
use crate::util::alloc::{kvfree_sized, kvzalloc};
use crate::util::place::{Contained, PlacedDrop};
use crate::util::shrinker::{Shrink, Shrinker};
use crate::util::sleep::Sleep;
use crate::c;
use crate::debug::trace::events;
use crate::errcode::{bch_errcode, BchError};
use crate::prt;
use crate::util::closure::{closure_wait, closure_wake_up};
use crate::util::sched::current;
use crate::fs::Fs;
use crate::util::vstructs::Head;
use core::mem::offset_of;
use core::ptr::NonNull;
use crate::util::ffi::c_char;
use crate::util::{HumanReadable, Printbuf, ToText};
use core::ffi::{c_int, c_uint};
use core::sync::atomic::AtomicU64;
use core::sync::atomic::Ordering::{Relaxed, Release, SeqCst};
use cstruct_macros::{c_const, c_export};
use crate::format::{Btree, BkeyType};

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
/// Nothing else has @c yet: btree_init_rw() is making it.
pub unsafe fn btree_evicted_size_init(c: *mut Fs) -> Result<(), BchError> {
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
        return fs.throw(bch_errcode::BCH_ERR_ENOMEM_fs_other_alloc);
    };
    unsafe {
        let e = &raw mut (*c).btree.evicted_size;
        (*e).entries = entries.as_ptr().cast();
        (*e).mask    = nr as u64 - 1;
    }
    Ok(())
}

/// # Safety
/// Nothing uses @c's evicted sizes after.
pub unsafe fn btree_evicted_size_exit(c: *mut Fs) {
    unsafe {
        let e = &raw mut (*c).btree.evicted_size;
        if let Some(p) = NonNull::new((*e).entries) {
            kvfree_sized(p.cast(), ((*e).mask as usize + 1) * size_of::<u64>());
        }
        (*e).entries = core::ptr::null_mut();
        (*e).mask    = 0;
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
            c::btree_node_cache_state::DIRTY
        } else {
            c::btree_node_cache_state::CLEAN
        }
    }
}

impl BtreeNode {
    pub fn wait_on_flag(&self, bit: c::btree_flags) {
        let m = 1 << bit as u32;
        let Ok(()) = self.flags.wait_until(Sleep::UNINTERRUPTIBLE.io(), |f| f.into_bits() & m == 0);
    }
}

impl Fs {
    /// Bytes of btree nodes cached.
    pub fn btree_cache_size(&self) -> u64 {
        let bc = &self.btree.cache;
        (bc.lock.nr_live() * self.opts.btree_node_size.get() as usize) as u64
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
            Shrinker::register(s, name.as_c_str(), seeks * mult, 0)
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

        self.btree.cache.lock.lock().published().nr_reserve.set(reserve);
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

pub fn btree_pos_to_text(out: &mut Printbuf, fs: &Fs, btree: Btree, level: c_uint, k: BkeySC<'_>) {
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
    btree_pos_to_text(out, fs, Btree(b.c.btree_id.into()), b.c.level.into(), k)
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
        prt_btree_cache_line(out, fs, "reserve:",   self.lock.nr_reserve.get());
        prt_btree_cache_line(out, fs, "freeable:",  self.lock.nr_freeable.get());
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

// ── For init.c ───────────────────────────────────────────────────────────

