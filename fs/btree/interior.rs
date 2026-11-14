// SPDX-License-Identifier: GPL-2.0

//! Interior updates - splits, merges, rewrites: their nodes allocated from a
//! reserve, linked into the parent, the old ones freed - and the btree roots,
//! as journal entries: what every journal write, and the clean section,
//! records of where each btree's root is.

#![allow(non_camel_case_types)]

use core::ffi::{c_uint, c_ulong};
use core::mem::{size_of, ManuallyDrop};
use core::ops::Deref;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicU32, AtomicU8, AtomicUsize, Ordering::{Acquire, AcqRel, Relaxed, SeqCst}};
use cstruct_macros::{placed, MaybeZeroable, Init, Late, TypeInfo};
use crate::alloc::capacity::DiskReservation;
use crate::alloc::foreground::OpenBucket;
use crate::btree::iter::{BtreeTrans, CommitOpts, TransAttempt, Watermark};
use crate::btree::node::BtreeNode;
use crate::btree::bkey::{BkeyI, BkeyV, BKEY_U64S};
use crate::btree::path::{LockedNode, PathIdx, Paths, Write};
use crate::c;
use crate::data::keylist::{Keylist, PaddedKey};
use crate::data_structures::array_vec::ArrayVec;
use crate::data_structures::list::ListLinks;
use crate::data_structures::ref_cell::RefCell;
use crate::errcode::{Errcode, BchError};
use crate::format::{Bpos, Btree};
use crate::fs::Fs;
use crate::journal::entries::{EntryWriter, NoRoom};
use crate::journal::pins::Pin;
use crate::locking::{Exclusive, ExclusiveGuard, Guard as _, Locked, LockedBy, SharedGuard};
use crate::new_lock;
use crate::types::{Fifo, Le64, Lent};
use crate::util::Printbuf;
use crate::util::alloc::{AllocBox, KVVec, Kmalloc};
use crate::util::backref::BackRef;
use crate::util::backtrace::prt_symbol;
use crate::util::closure::{closure_wait_event, closure_wake_up};
use crate::os::kernel::local_clock;
use crate::util::place::{Contained, Placed};
use crate::util::time_stats::TimeStats;
use crate::util::{late::Late, mempool::Mempool, workqueue::{run_raw, RawWork, Workqueue, WqFlags}};
use crate::prt;
use pin_init::{init, init_zeroed, pin_data};
use zerocopy::IntoBytes;

mod completion;
mod reserve;
mod root;
mod split_merge;
mod update_key;
pub(crate) use root::root_alloc_fake_trans;
pub(crate) use split_merge::{check_topology, check_topology_msg, foreground_maybe_merge, increase_depth, merge,
                             node_rewrite, split_leaf};

#[repr(C)]
#[derive(Default, MaybeZeroable)]
pub struct btree_alloc {
    pub ob: c::open_buckets,
    pub k: PaddedKey<{ BKEY_U64S + c::BKEY_BTREE_PTR_VAL_U64s_MAX }>,
}

/* Maximum number of nodes we might need to allocate atomically: */
pub const BTREE_RESERVE_MAX: u32 = c::BTREE_MAX_DEPTH as u32 + (c::BTREE_MAX_DEPTH as u32 - 1);

/* Size of the freelist we allocate btree nodes from: */
pub const BTREE_NODE_RESERVE: u32 = c::BTREE_RESERVE_MAX * 4;

/*
 * Cache of allocated btree nodes - if we allocate a btree node and don't use
 * it, if we free it that space can't be reused until going _all_ the way
 * through the allocator (which exposes us to a livelock when allocating btree
 * reserves fail halfway through) - instead, we can stick them here:
 */
#[repr(C)]
#[derive(MaybeZeroable)]
pub struct BtreeReserveCache {
    pub nr: c_uint,
    pub data: [c::btree_alloc; c::BTREE_NODE_RESERVE as usize * 2],
}

#[pin_data]
#[repr(C)]
#[derive(MaybeZeroable, TypeInfo, Init)]
pub struct bch_fs_btree_reserve_cache {
    #[pin]
    #[init(new_lock!(<- ::pin_init::init_zeroed(), (), "&c->btree.reserve_cache.lock"))]
    pub lock: Locked<c::mutex, BtreeReserveCache>,
}

const UPDATE_NODES_MAX: usize = (c::BTREE_MAX_DEPTH as usize - 2) * 2 + c::GC_MERGE_NODES as usize;

/// An update made visible - its new nodes' pointers in memory - by what: on
/// interior_updates.unwritten from then until it completes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /*
     * The update that made the new nodes visible was a regular update to an
     * existing interior node - @b. We can't write out the update to @b
     * until the new nodes we created are finished writing, so we block @b
     * from writing by putting this btree_interior update on the
     * @b->write_blocked list with @write_blocked_list:
     */
    Node(NonNull<BtreeNode>),
    Root,
    /* the node it updated freed, by the update it's now waiting on */
    Update,
}

/// A node's updates blocking its write, under interior_updates.lock.
pub(crate) type WriteBlocked = crate::list!(BtreeUpdate, write_blocked_list);

/// interior_updates.lock's.
#[derive(Default)]
pub(crate) struct Updates {
    /*
     * Every update from start to free: being here is the update's ref on the
     * filesystem
     */
    live:      KVVec<NonNull<BtreeUpdate>>,
    /*
     * Made visible, in the order they were: completed in that order. Each its
     * update's owner, from updated_node() or updated_root() to its free
     */
    unwritten: Fifo<UpdateBox>,
}

// The updates are reached through the lock, from any thread
unsafe impl Send for Updates {}

/// interior_updates.lock, held.
pub(crate) type UpdatesGuard<'a> = ExclusiveGuard<'a, c::mutex, Updates, (), Exclusive, UpdatesLockAt>;
/// commit_lock, held.
pub(crate) type CommitGuard<'a> = ExclusiveGuard<'a, c::mutex, (), (), Exclusive, CommitLockAt>;

/// The two locks' places, for what they cover to name.
pub(crate) type UpdatesLockAt = crate::at!(bch_fs_btree_interior_updates.lock);
pub(crate) type CommitLockAt = crate::at!(bch_fs_btree_interior_updates.commit_lock);

/// @b's updates blocking its write.
fn write_blocked<'a>(b: &'a BtreeNode, updates: &'a mut UpdatesGuard<'_>) -> &'a mut WriteBlocked {
    updates.get_mut_of(&b.write_blocked)
}

#[placed(c::bch_fs_btree.interior_updates)]
#[pin_data]
#[repr(C)]
#[derive(MaybeZeroable, TypeInfo, Init, Late)]
pub struct bch_fs_btree_interior_updates {
    #[late(Mempool::kmalloc(1, size_of::<BtreeUpdate>()), ENOMEM_btree_interior_update_pool_init)]
    pub pool: Late<Mempool<Kmalloc>>,
    #[pin]
    #[init(new_lock!(Updates::default(), (), "&c->btree.interior_updates.lock"))]
    lock: Placed<Locked<c::mutex, Updates>, UpdatesLockAt>,
    /*
     * btree_interior_update_commit_lock is needed for synchronization with
     * btree_node_update_key(): having the lock be at the filesystem level
     * sucks, we'll need to watch for contention
     */
    #[pin]
    #[init(new_lock!((), (), "&c->btree.interior_updates.commit_lock"))]
    pub(crate) commit_lock: Placed<Locked<c::mutex>, CommitLockAt>,
    pub wait: c::closure_waitlist,

    #[late(Workqueue::new(c"btree_update", WqFlags::UNBOUND | WqFlags::MEM_RECLAIM, 8), ENOMEM_btree_interior_update_worker_init)]
    pub worker: Late<Workqueue>,
    #[pin]
    #[init(crate::new_work!(run_raw::<bch_fs_btree_interior_updates>, "&c->btree.interior_updates.work"))]
    pub work: c::work_struct,
}

impl RawWork for bch_fs_btree_interior_updates {
    const WORK: usize = core::mem::offset_of!(bch_fs_btree_interior_updates, work);

    unsafe fn run(this: NonNull<Self>) {
        completion::worker(unsafe { this.as_ref() }.container().container())
    }
}

impl bch_fs_btree_interior_updates {
    fn kick(&self) {
        unsafe { self.worker.queue_work((&raw const self.work).cast_mut()) };
    }
}

type NodeKey = PaddedKey<{ BKEY_U64S + c::BKEY_BTREE_PTR_VAL_U64s_MAX }>;

fn node_key(b: LockedNode<'_, BtreeNode>) -> NodeKey {
    let mut key = NodeKey::default();
    key.set_key(b.contents().key_i());
    key
}

/// A node the update frees: its pointer, which the commit deletes, and its
/// seq, to tell it from whatever reclaim reuses its memory for.
struct OldNode {
    b:     NonNull<BtreeNode>,
    level: u8,
    seq:   Le64,
    key:   NodeKey,
}

impl OldNode {
    fn of(b: LockedNode<'_, BtreeNode>) -> Self {
        OldNode { b: NonNull::from(b.node()), level: b.c.level, seq: b.contents().data().keys.seq, key: node_key(b) }
    }
}

/// A pointer the update's commit journals.
struct NewPointer {
    level: u8,
    key:   NodeKey,
    to:    Pointee,
}

enum Pointee {
    /// A new node, which the commit makes reachable
    Node {
        b:               NonNull<BtreeNode>,
        root:            bool,
        /* the commit's: whether it changed the key, which the node's then given */
        update_node_key: bool,
    },
    /// A merge's: the pointer at the key's position, deleted
    Deleted,
}

impl NewPointer {
    fn node(fs: &Fs, b: LockedNode<'_, BtreeNode>) -> Self {
        NewPointer {
            level: b.c.level,
            key:   node_key(b),
            to:    Pointee::Node {
                b:               NonNull::from(b.node()),
                root:            fs.btree_id_root(b.c.btree_id.into()).is_some_and(|r| ptr::eq(r, b.node())),
                update_node_key: false,
            },
        }
    }

    fn node_ptr(&self) -> Option<NonNull<BtreeNode>> {
        match self.to {
            Pointee::Node { b, .. } => Some(b),
            Pointee::Deleted        => None,
        }
    }
}

type NewPointers = ArrayVec<NewPointer, UPDATE_NODES_MAX>;

/// The node an update starts at, as it was: for to_text().
struct StartNode {
    start:          Bpos,
    end:            Bpos,
    needed_rewrite: c::BtreeNodeRewriteReason,
    written:        u16,
    sectors:        u16,
    remaining:      u16,
}

type GcReadLock = SharedGuard<'static, c::rw_semaphore>;

/// gc.lock, held for read from start() until the update's done, or freed.
fn gc_read_lock(trans: &BtreeTrans<'_>, paths: &mut Paths<'_>, fs: &'static Fs) -> Result<GcReadLock, BchError> {
    if let Some(g) = fs.gc.lock.try_read() {
        return Ok(g);
    }

    trans.unlock_from(paths, crate::ret_ip!());
    let g = fs.gc.lock.read();
    trans.relock_from(paths)?;
    Ok(g)
}

/// A node in an update's reserve, intent and write locked, and the open
/// buckets its space is in: the update's once the node's written, or the
/// reserve cache's if it's not used.
struct Reserved {
    b:  NonNull<BtreeNode>,
    ob: c::open_buckets,
}

/// The update's own, in its foreground phase and then its completion's: never
/// both at once.
struct Owned {
    /*
     * Preallocated nodes we reserve when we start the update, leaf and
     * interior: alloc_node() takes them from here, to @consumed.
     */
    reserve:      [ArrayVec<Reserved, UPDATE_NODES_MAX>; 2],
    consumed:     ArrayVec<Reserved, UPDATE_NODES_MAX>,
    /* take_path()'s, put before the update lets go of its nodes */
    paths:        ArrayVec<PathIdx, UPDATE_NODES_MAX>,
    old_nodes:    ArrayVec<OldNode, UPDATE_NODES_MAX>,
    open_buckets: ArrayVec<OpenBucket, { UPDATE_NODES_MAX * c::BCH_REPLICAS_MAX as usize }>,
    gc_lock:      Option<GcReadLock>,

    /* Only here to reduce stack usage on recursive splits: */
    parent_keys:  ParentKeys,
}

/*
 * Enough room for btree_split's keys without realloc - btree node
 * pointers never have crc/compression info, so we only need to acount
 * for the pointers for three keys
 */
pub(super) type ParentKeys = Keylist<{ c::BKEY_BTREE_PTR_U64s_MAX * 3 }>;

/*
 * Tracks an in progress split/rewrite of a btree node and the update to the
 * parent node:
 *
 * When we split/rewrite a node, we do all the updates in memory without
 * waiting for any writes to complete - we allocate the new node(s) and update
 * the parent node, possibly recursively up to the root.
 *
 * The end result is that we have one or more new nodes being written -
 * possibly several, if there were multiple splits - and then a write (updating
 * an interior node) which will make all these new nodes visible.
 *
 * Additionally, as we split/rewrite nodes we free the old nodes - but the old
 * nodes can't be freed (their space on disk can't be reclaimed) until the
 * update to the interior node that makes the new node visible completes -
 * until then, the old nodes are still reachable on disk.
 *
 */
pub(crate) struct BtreeUpdate {
    fs:           BackRef<Fs>,
    start_time:   u64,
    ip_started:   c_ulong,
    flags:        CommitOpts,
    btree:        Btree,
    level_start:  u8,
    /* the first level the reserve doesn't cover: to_text() reads it as it changes */
    level_end:    AtomicU8,
    node:         StartNode,
    /*
     * size of the key that triggered split_leaf (0 if N/A) — drives the
     * split-vs-compact decision in btree_split() so we don't loop trying to
     * compact a leaf that can't fit the new key.
     */
    new_key_u64s: u32,

    /*
     * The foreground's, and each new node's first write's, until it's done:
     * the last put queues the update's completion
     */
    pending:      AtomicU32,
    /*
     * We may be freeing nodes that were dirty, and thus had journal entries
     * pinned: we need to transfer the oldest of those pins to the
     * btree_update operation, and release it when the new node(s)
     * are all persistent and reachable:
     */
    journal:      Pin,
    disk_res:     DiskReservation<'static>,
    owned:        RefCell<Owned>,
    /*
     * The update's alone until done(): btree_node_update_key() reaches it
     * through a new node, which it can't lock before done() unlocks them.
     * Then commit_lock's, under which it edits a new node's key here.
     */
    new_nodes:    LockedBy<NewPointers, (), CommitLockAt>,

    /* interior_updates.lock's: None until updated_node() or updated_root() */
    mode:               LockedBy<Option<Mode>, (), UpdatesLockAt>,
    write_blocked_list: ListLinks,
}

// owned is the foreground's, then the completion's; the rest is shared as
// each field says
unsafe impl Sync for BtreeUpdate {}

fn rewrite_reason(b: &BtreeNode) -> c::BtreeNodeRewriteReason {
    use c::BtreeNodeRewriteReason as r;
    if b.flags.need_rewrite_ptr_written_zero() {
        r::ptr_written_zero
    } else if b.flags.need_rewrite_error() {
        r::error
    } else if b.flags.need_rewrite() {
        r::unknown
    } else {
        r::none
    }
}

macro_rules! rewrite_reason_strs {
    ($(($name:ident $(, $($rest:tt)*)?)),* $(,)?) => {
        const REWRITE_REASON_STRS: &[&str] = &[$(stringify!($name),)*];
    };
}
c::BtreeNodeRewriteReason_arms!(rewrite_reason_strs);

/// An update, in an element of the interior updates' pool.
type UpdateBox = AllocBox<BtreeUpdate, &'static Mempool<Kmalloc>>;

impl BtreeUpdate {
    fn fs(&self) -> &Fs {
        &self.fs
    }

    fn level_end(&self) -> usize {
        self.level_end.load(Relaxed).into()
    }

    /// One of the refs pending put: the last queues the completion.
    fn pending_put(&self) {
        if self.pending.fetch_sub(1, AcqRel) == 1 {
            self.fs().btree.interior_updates.kick();
        }
    }

    fn nodes_written(&self) -> bool {
        self.pending.load(Acquire) == 0
    }

    fn mode(&self, updates: &UpdatesGuard<'_>) -> Option<Mode> {
        *updates.get_of(&self.mode)
    }

    fn set_mode(&self, updates: &mut UpdatesGuard<'_>, mode: Mode) {
        *updates.get_mut_of(&self.mode) = Some(mode)
    }

    /// # Safety
    /// The update holds its new nodes locked: before done().
    #[allow(clippy::mut_from_ref)]
    unsafe fn new_nodes(&self) -> &mut NewPointers {
        unsafe { self.new_nodes.protected_mut() }
    }

    fn new_nodes_locked<'a>(&'a self, commit_lock: &'a mut CommitGuard<'_>) -> &'a mut NewPointers {
        commit_lock.get_mut_of(&self.new_nodes)
    }

    /// @b's entry in the new nodes, its key made @k: btree_node_update_key()'s,
    /// on a node the update will make reachable.
    pub(crate) fn new_node_key_set(&self, commit_lock: &mut CommitGuard<'_>, b: &BtreeNode,
                                   k: impl BkeyV) {
        self.new_nodes_locked(commit_lock).iter_mut()
            .find(|n| n.node_ptr().is_some_and(|nb| ptr::eq(nb.as_ptr(), b)))
            .expect("a node its update will make reachable, in the update's new nodes")
            .key.set_key(k);
    }

    fn to_text(&self, out: &mut Printbuf, mode: Option<Mode>) {
        let n = &self.node;
        prt_symbol(out, self.ip_started);
        prt!(out, ": {} {} l={}-{} {} {}\n", self.flags, self.btree, self.level_start, self.level_end(),
             n.start, n.end);
        prt!(out, "written {}/{} u64s_remaining {} need_rewrite {}\n", n.written, n.sectors, n.remaining,
             REWRITE_REASON_STRS[n.needed_rewrite as usize]);

        let mode = match mode {
            None                => "none",
            Some(Mode::Node(_)) => "node",
            Some(Mode::Root)    => "root",
            Some(Mode::Update)  => "update",
        };
        let pending = self.pending.load(Relaxed);
        prt!(out, "mode={} nodes_written={} cl.remaining={} journal_seq={}\n",
             mode, u32::from(pending == 0), pending, self.journal.seq());
    }
}

/// A new node's: the update that will make it reachable, from write_new_node()
/// until the update's commit - tagged, bit 0, while the update holds a ref
/// on pending for the node's first write.
#[repr(transparent)]
#[derive(Default)]
pub struct WillMakeReachable(AtomicUsize);

impl WillMakeReachable {
    fn set(&self, update: NonNull<BtreeUpdate>) {
        assert!(self.0.load(Relaxed) == 0, "a new node another update will make reachable");
        self.0.store(1 | update.as_ptr() as usize, Relaxed);
    }

    /// The node's first write done: the update's ref for it put.
    pub(crate) fn first_write_done(&self) {
        if let Ok(v) = self.0.fetch_update(SeqCst, Relaxed, |v| (v & 1 != 0).then_some(v & !1)) {
            unsafe { &*((v & !1) as *const BtreeUpdate) }.pending_put();
        }
    }

    /// The update, if one will make the node reachable.
    pub(crate) fn update(&self) -> Option<NonNull<BtreeUpdate>> {
        NonNull::new((self.0.load(Relaxed) & !1) as *mut BtreeUpdate)
    }

    fn clear(&self, update: NonNull<BtreeUpdate>) {
        assert!(self.0.load(Relaxed) == update.as_ptr() as usize,
                "a new node another update will make reachable");
        self.0.store(0, Relaxed);
    }

    /// As it is, for a node's to_text().
    pub(crate) fn raw(&self) -> usize {
        self.0.load(Relaxed)
    }
}

/// What an interior update reserves for, and records: InteriorUpdate::start()'s.
#[derive(Default)]
pub(crate) struct StartOpts {
    /// The node at the first level splits, not just gets rewritten
    pub split:        bool,
    pub target:       u32,
    pub commit:       CommitOpts,
    pub write:        c::WriteFlags,
    /// split_leaf()'s: the key that didn't fit, so btree_split() doesn't
    /// compact a leaf that still wouldn't have room for it
    pub new_key_u64s: u32,
    /// The nodes being replaced, where they aren't just @path's: a merge's
    pub range:        Option<(Bpos, Bpos)>,
}

/// An interior update in its foreground phase: reserved, in attempt @t. Dropped,
/// it's rolled back; done() hands it to its completion, which commits the new
/// nodes' pointers once they're written.
pub(crate) struct InteriorUpdate<'t> {
    t:     &'t TransAttempt<'t, 't>,
    raw:   NonNull<BtreeUpdate>,
    /*
     * The update, until updated_node() or updated_root() makes it visible and
     * hands it to its entry on unwritten: our ref on pending keeps it there
     */
    owner: RefCell<Option<UpdateBox>>,
}

impl Deref for InteriorUpdate<'_> {
    type Target = BtreeUpdate;

    fn deref(&self) -> &BtreeUpdate {
        unsafe { self.raw.as_ref() }
    }
}

/// A node from an interior update's reserve: intent and write locked, not yet
/// reachable.
pub(crate) type NewNode<'u> = LockedNode<'u, BtreeNode, Write>;

impl<'t> InteriorUpdate<'t> {
    /// An update for @path's node at @level, its reserve the worst case: every
    /// level above splitting, up to a new root.
    pub(crate) fn start(t: &'t TransAttempt<'t, 't>, paths: &mut Paths<'_>, path: PathIdx, level: usize,
                        opts: StartOpts) -> Result<Self, BchError> {
        let ip = crate::ret_ip!();
        Self::start_in(t, paths, path, level, opts, ip)
    }

    fn start_in(t: &'t TransAttempt<'t, 't>, paths: &mut Paths<'_>, path: PathIdx, level_start: usize,
                StartOpts { mut split, target, mut commit, write, new_key_u64s, range }: StartOpts,
                ip_started: c_ulong) -> Result<Self, BchError>
    {
        use c::Watermark as wm;

        let trans = t.trans();
        let fs = trans.fs();
        let start_time = local_clock();
        let restart_count = trans.restart_count();

        trans.verify_paths(paths);
        assert!(paths[path].should_be_locked(), "an interior update on a path that isn't locked");

        let mut watermark = commit.get_watermark();
        if watermark == wm::stripe {
            watermark = wm::normal;
        }

        /*
         * Going read-only, every interior update is part of the final journal
         * flush, which can't finish until it does - and discards, the only
         * thing that would free buckets, stopped with writes. Below the
         * reclaim watermark a node allocation can wait on the btree reserve
         * forever (unmount hanging in wait_on_allocator()):
         */
        if fs.flags.get(c::FsFlag::going_ro) && (watermark as u32) < wm::reclaim as u32 {
            watermark = wm::reclaim;
        }
        commit = commit.watermark(Watermark::from(watermark));

        let journal_low = || fs.journal.flags.get(c::JournalFlag::low_on_space) ||
                              fs.journal.flags.get(c::JournalFlag::low_on_pin);
        if (watermark as u32) < wm::reclaim as u32 && journal_low() {
            if commit.to_c().contains(c::TransCommitFlags::journal_reclaim) {
                return Err(BchError::from(Errcode::journal_reclaim_would_deadlock));
            }

            /*
             * Or until the journal errors out: a halted journal stays low on
             * space for good, and the bch2_journal_error() check below
             * returns the error (tools#971 - rewrite workers queued after a
             * failed mount waited here forever, holding up bch2_fs_stop()):
             */
            trans.unlock_from(paths, crate::ret_ip!());
            trans.wait_event_in(paths, &fs.journal.async_wait, || !journal_low() || fs.journal.error().is_err());
            trans.relock_from(paths)?;
        }

        let mut nr_nodes = [0usize; 2];
        let mut level_end = level_start;
        loop {
            nr_nodes[usize::from(level_end != 0)] += 1 + usize::from(split);
            level_end += 1;

            trans.path_upgrade(paths, path, level_end + 1, crate::ret_ip!())?;

            let Some(b) = paths.held_btree_node(path, level_end) else {
                /* Allocating new root? */
                nr_nodes[1] += usize::from(split);
                level_end = c::BTREE_MAX_DEPTH as usize;
                break;
            };

            /*
             * Always check for space for two keys, even if we won't have to
             * split at prior level - it might have been a merge instead:
             */
            if b.insert_fits(b.contents(), c::BKEY_BTREE_PTR_U64s_MAX * 2) {
                break;
            }

            split = usize::from(b.contents().nr.live_u64s) > split_merge::split_threshold(fs);
        }

        // the update lives while the filesystem does: being on the live list is
        // its ref on it
        let fs_ref = unsafe { BackRef::new(fs).get() };

        let gc_lock = gc_read_lock(trans, paths, fs_ref)?;

        let node = {
            let b = paths.held_btree_node(path, level_start).expect("an interior update's node");
            let contents = b.contents();
            let data = contents.data();
            let (start, end) = range.unwrap_or((data.min_key, data.max_key));
            StartNode {
                start,
                end,
                needed_rewrite: rewrite_reason(&b),
                written:        b.written.get(),
                sectors:        (b.buf_bytes() >> 9) as u16,
                remaining:      b.u64s_remaining(contents.last_set().end_offset as usize) as u16,
            }
        };

        let btree = paths[path].btree_id();
        // made in place: the arrays are kilobytes
        let owner = AllocBox::new(&*fs_ref.btree.interior_updates.pool, init!(BtreeUpdate {
            fs:           unsafe { BackRef::new(fs) },
            start_time,
            ip_started,
            flags:        commit,
            btree,
            level_start:  level_start as u8,
            level_end:    AtomicU8::new(level_end as u8),
            node,
            new_key_u64s,
            pending:      AtomicU32::new(1),
            journal:      ::pin_init::zeroed(),
            disk_res:     DiskReservation::new(fs_ref),
            owned <- RefCell::init(init!(Owned {
                reserve      <- init_zeroed(),
                consumed     <- init_zeroed(),
                paths        <- init_zeroed(),
                old_nodes    <- init_zeroed(),
                open_buckets <- init_zeroed(),
                gc_lock:      Some(gc_lock),
                parent_keys:  Keylist::new(),
            })),
            new_nodes    <- init_zeroed(),
            mode:               LockedBy::new(None, ()),
            write_blocked_list: ListLinks::new(),
        }? BchError))?;
        let raw = owner.as_ptr();

        {
            let mut g = fs.btree.interior_updates.lock.lock();
            g.live.push_nofail(raw);
            /* each live update goes on unwritten once: updated_node()'s push can't fail */
            let more = g.live.len() - g.unwritten.len();
            g.unwritten.reserve_nofail(more);
        }
        let update = InteriorUpdate { t, raw, owner: RefCell::new(Some(owner)) };

        /*
         * We don't want to allocate if we're in an error state, that can cause
         * deadlock on emergency shutdown due to open buckets getting stuck in
         * the btree_reserve_cache after allocator shutdown has cleared it out.
         * This check needs to come after adding us to the btree_interior_update
         * list but before calling bch2_btree_reserve_get, to synchronize with
         * __bch2_fs_read_only().
         */
        let ret = fs.journal.error()
            .and_then(|()| update.reserve(paths, nr_nodes, target, commit, write))
            .and_then(|()| trans.relock_from(paths));
        if let Err(e) = ret {
            if crate::util::log::should_print_err(&e) &&
               !e.matches(c::ENOSPC) &&
               !e.matches(c::EROFS) &&
               e != BchError::from(Errcode::journal_reclaim_would_deadlock) &&
               e != BchError::from(Errcode::journal_shutdown) {
                crate::bch_err_ratelimited!(fs, "{}: error {}", crate::here_str!(), e.msg());
            }
            return Err(e);
        }

        trans.verify_not_restarted(restart_count);
        Ok(update)
    }

    pub(crate) fn t(&self) -> &'t TransAttempt<'t, 't> {
        self.t
    }

    pub(crate) fn trans(&self) -> &'t BtreeTrans<'t> {
        self.t.trans()
    }

    fn parent_keys_u64s(&self) -> usize {
        self.owned.borrow().parent_keys.u64s()
    }

    #[cfg(CONFIG_BCACHEFS_DEBUG)]
    fn verify_parent_keys_sorted(&self) {
        self.owned.borrow().parent_keys.verify_sorted()
    }

    fn new_key_u64s(&self) -> usize {
        self.new_key_u64s as usize
    }

    /// Whether it's been made visible, by an insert into an existing node or
    /// as the root.
    fn visible(&self) -> bool {
        self.owner.borrow().is_none()
    }

    /// The update, handed to unwritten: interior_updates.lock, still held.
    fn make_visible(&self, mode: Mode) -> UpdatesGuard<'_> {
        let update = self.owner.borrow_mut().take().expect("an interior update made visible twice");

        let mut g = self.fs().btree.interior_updates.lock.lock();
        update.set_mode(&mut g, mode);
        if g.unwritten.push_back(update).is_err() {
            panic!("unwritten full: start() reserves room for every live update");
        }
        g
    }

    fn to_text(&self, out: &mut Printbuf) {
        let g = self.fs().btree.interior_updates.lock.lock();
        (**self).to_text(out, self.mode(&g))
    }

    fn push_parent_key(&self, k: &BkeyI) {
        self.owned.borrow_mut().parent_keys.push_key(k)
    }

    /// @n's pointer, its sectors_written stamped, added to what the parent
    /// gets.
    pub(crate) fn link(&self, n: &mut NewNode<'_>) {
        self.emit_new_node_key(n);
        self.push_parent_key(n.contents().key_i());
    }

    /// The pointer at @pos to a node at @level deleted from the parent, and
    /// the deletion journalled with the update.
    pub(crate) fn unlink(&self, pos: Bpos, level: usize) {
        let delete = BkeyI::<c::Deleted>::at(pos);
        let delete = delete.as_ref();

        self.push_parent_key(delete);

        let mut key = NodeKey::default();
        key.set_key(delete);
        unsafe { self.new_nodes() }.push(NewPointer { level: level as u8, key, to: Pointee::Deleted });
    }

    /// The parent keys that go in @n inserted into it, @path's at its level.
    fn insert_parent_keys(&self, paths: &mut Paths<'_>, path: PathIdx, n: &mut NewNode<'_>) -> Result<(), BchError> {
        split_merge::split_insert_keys(self.t, paths, path, n, &mut self.owned.borrow_mut().parent_keys)
    }

    /// The parent keys that go in @b inserted into it from @path's iterator
    /// there: @b is the node @path holds at its level.
    fn insert_parent_keys_at(&self, paths: &mut Paths<'_>, path: PathIdx,
                             b: &mut LockedNode<'_, BtreeNode, Write>) -> Result<(), BchError> {
        let node_iter = paths[path].l[usize::from(b.c.level)].iter;
        split_merge::insert_keys_interior(self.t, paths, path, b, node_iter,
                                          &mut self.owned.borrow_mut().parent_keys)
    }

    /// Whether the parent keys delete the key at @pos.
    fn parent_key_deleted(&self, pos: Bpos) -> bool {
        self.owned.borrow().parent_keys.iter().any(|k| k.k().key_type().is_deleted() && k.k().p == pos)
    }

    /// What link() added, inserted into @path's node above @level - splitting
    /// it, and up, if it doesn't fit.
    pub(crate) fn insert_into_parent(&self, paths: &mut Paths<'_>, path: PathIdx,
                                     level: usize) -> Result<(), BchError> {
        let parent = paths.held_btree_node(path, level + 1).expect("an interior update's node with no parent");
        split_merge::insert_node(self, paths, path, parent)
    }

    /// @old - each held by its path - swapped for @new, linked in: the old
    /// nodes freed in memory before the new are visible to the transaction's
    /// other paths, so nothing relocks an old node and sees what the new one
    /// replaced. @new in the order they're written.
    pub(crate) fn replace(&self, paths: &mut Paths<'_>, old: &[(PathIdx, LockedNode<'_, BtreeNode>)],
                          new: &mut [NewNode<'_>]) {
        for &(_, b) in old {
            self.will_free_node(b);
        }
        for n in new.iter_mut() {
            self.write_new_node(n);
        }
        for &(idx, b) in old {
            self.node_free_inmem(paths, idx, b);
        }
        for n in new {
            self.trans().node_add(self.t, &mut paths.paths_and_updates(), n.held());
        }
        for &(_, b) in old {
            self.trans().node_verify_not_in_iters(paths, &b);
        }
    }

    /// The paths take_path() made, put: the last taken first.
    fn put_paths(&self, paths: &mut Paths<'_>) {
        let taken = core::mem::take(&mut self.owned.borrow_mut().paths);
        for idx in taken.into_iter().rev() {
            self.trans().path_unlock(paths, idx);
            self.trans().path_put(paths, idx, true);
        }
    }

    pub(crate) fn done(self, paths: &mut Paths<'_>) {
        assert!(self.visible(), "an interior update done without making its nodes visible");

        let this = ManuallyDrop::new(self);
        let fs = this.fs();
        let start_time = this.start_time;

        this.put_paths(paths);
        this.trans().unlock_write(paths);

        drop(this.owned.borrow_mut().gc_lock.take());
        this.reserve_put(this.trans(), paths, false);
        this.pending_put();

        fs.times[c::TimeStat::btree_interior_update_foreground as usize].update(start_time);
    }
}

impl Drop for InteriorUpdate<'_> {
    fn drop(&mut self) {
        let owner = self.owner.get_mut().take()
            .expect("an interior update rolled back after making its nodes visible");

        // the caller's borrow of the path table may be live, and idle: as C's
        let mut paths = unsafe { Paths::lent(self.trans()) };
        self.put_paths(&mut paths);
        self.trans().unlock_write(&mut paths);
        unsafe { update_free(self.raw, self.trans(), &mut paths, Some(owner)) }
    }
}

/// @update's resources released, and it freed - owned by @rollback, which
/// frees the nodes it took from its reserve too, or else by its entry on
/// unwritten, completing.
///
/// # Safety
/// @update is live, and nothing uses it after.
unsafe fn update_free(update: NonNull<BtreeUpdate>, trans: &BtreeTrans<'_>, paths: &mut Paths<'_>,
                      rollback: Option<UpdateBox>) {
    let u = unsafe { update.as_ref() };
    let fs = unsafe { u.fs.get() };

    drop(u.owned.borrow_mut().gc_lock.take());

    fs.journal.pin_drop(&u.journal);
    fs.journal.pin_flush(&u.journal);
    u.disk_res.put();
    u.reserve_put(trans, paths, rollback.is_some());

    fs.times[c::TimeStat::btree_interior_update_total as usize].update(u.start_time);

    let updates = &fs.btree.interior_updates;
    let mut g = updates.lock.lock();

    let owner = rollback.unwrap_or_else(|| {
        let front = g.unwritten.pop_front().expect("an update completed, on unwritten");
        assert!(front.as_ptr() == update, "an update completed out of order");
        front
    });
    g.live.retain(|p| *p != update);
    drop(owner);

    /*
     * Have to do the wakeup with btree_interior_update_lock still held,
     * since being on btree_interior_update_list is our ref on @c:
     */
    closure_wake_up(&updates.wait);
    drop(g);
}

impl Fs {
    pub fn btree_interior_updates_pending(&self) -> bool {
        !self.btree.interior_updates.lock.lock().live.is_empty()
    }

    /// Waits for the interior updates in flight to finish - with no btree
    /// locks held, as they may need them; whether there were any.
    pub fn btree_interior_updates_flush(&self) -> bool {
        let ret = self.btree_interior_updates_pending();
        if ret {
            closure_wait_event(&self.btree.interior_updates.wait, || !self.btree_interior_updates_pending());
        }
        ret
    }

    /// Run the interior update worker now.
    pub fn btree_interior_updates_kick(&self) {
        self.btree.interior_updates.kick()
    }

    pub fn btree_updates_to_text(&self, out: &mut Printbuf) {
        let g = self.btree.interior_updates.lock.lock();
        for &p in g.live.iter() {
            let u = unsafe { p.as_ref() };
            u.to_text(out, u.mode(&g));
        }
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
        let k = r.key.key();
        w.add_entry(c::JsetEntryType::btree_root, id as u8, r.level, k.k.u64s.into())?
            .as_mut_bytes().copy_from_slice(k.as_bytes());
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
