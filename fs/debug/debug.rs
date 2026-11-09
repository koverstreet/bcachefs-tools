// SPDX-License-Identifier: GPL-2.0

//! /sys/kernel/debug/bcachefs/<fs>/: each file a DebugFile (util/debugfs.rs).
//!
//! The btree walks don't start until BCH_FS_may_go_rw: until then journal
//! replay is changing the journal keys' gap buffer, which iteration reads.
//! They unlock before each flush: copy_to_user() can fault, and the fault
//! path takes mmap_lock, then snapshots.create_lock in page_mkwrite - which
//! the buffered write path takes before btree locks.

use core::convert::Infallible;
use core::ffi::{c_int, CStr};
use core::ops::ControlFlow;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, Ordering};
use core::marker::PhantomData;

use crate::btree::bkey::prelude::*;
use crate::btree::iter::{BtreeIterFlags, BtreeNodeIter};
use crate::btree::BtreeIter;
use crate::btree::node::BtreeNode;
use crate::btree::trans::transaction_fn;
use crate::c;
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::util::debugfs::{self, DebugFile, DebugOut};
use crate::data_structures::eytzinger::Eytzinger0;
use crate::util::place::Contained;
use crate::util::{rcu, Printbuf};
use crate::{btree_trans, prt, SPOS_MAX};
use crate::format::{Bpos, Btree};

fn may_iterate(fs: &Fs) -> bool {
    fs.flags.get(c::bch_fs_flags::may_go_rw)
}

/* btrees/<btree>/ */

impl c::btree_debug {
    /// The filesystem it's one of the btree_debug[] of.
    fn fs(&self) -> &Fs {
        self.container()
    }
}

#[derive(Default)]
struct Keys {
    from: Bpos,
}

impl DebugFile for Keys {
    type Obj = c::btree_debug;

    fn read(&mut self, bd: &c::btree_debug, out: &mut DebugOut<'_>) -> Result<(), BchError> {
        let fs = bd.fs();
        if !may_iterate(fs) {
            return Ok(());
        }

        let trans = btree_trans!(fs);
        let mut iter = BtreeIter::new(&*trans, bd.id.get(), self.from,
                                      BtreeIterFlags::PREFETCH | BtreeIterFlags::ALL_SNAPSHOTS);
        iter.for_each(&trans, |t, iter, k| {
            prt!(out, "{}\n", k.to_text(fs));
            t.unlock();
            self.from = iter.pos().successor();
            out.flush()
        })
    }
}

/// formats: each node's format and key counts, level by level, from the
/// leaves up.
#[derive(Default)]
struct Formats {
    level: u32,
    from:  Bpos,
}

impl DebugFile for Formats {
    type Obj = c::btree_debug;

    fn read(&mut self, bd: &c::btree_debug, out: &mut DebugOut<'_>) -> Result<(), BchError> {
        let fs = bd.fs();
        if !may_iterate(fs) {
            return Ok(());
        }

        let trans = btree_trans!(fs);
        while self.level < c::BTREE_MAX_DEPTH as u32 {
            let mut iter = BtreeNodeIter::new(&*trans, bd.id.get(), self.from, 0, self.level,
                                              BtreeIterFlags::empty());
            let full = iter.for_each(&trans, |b| {
                prt!(out, "{}", b.to_text(fs));
                let end = b.contents().key.k.p;
                self.from = if end != SPOS_MAX { end.successor() } else { end };

                // C's drop_locks_do(): the next level's iterator is made
                // from this trans, and needs it locked
                trans.unlock();
                let flow = out.flush()?;
                trans.relock()?;
                Ok(flow.map_break(|()| true))
            })?;
            if full {
                return Ok(());
            }

            self.level += 1;
            self.from = Bpos::default();
        }
        Ok(())
    }
}

/// bfloat-failed: for each key, where the bset search tree's lookup of it
/// failed - after each node, the first time it's reached.
#[derive(Default)]
struct BfloatFailed {
    from:      Bpos,
    prev_node: Bpos,
}

impl DebugFile for BfloatFailed {
    type Obj = c::btree_debug;

    fn read(&mut self, bd: &c::btree_debug, out: &mut DebugOut<'_>) -> Result<(), BchError> {
        let fs = bd.fs();
        if !may_iterate(fs) {
            return Ok(());
        }

        let trans = btree_trans!(fs);
        let mut iter = BtreeIter::new(&*trans, bd.id.get(), self.from,
                                      BtreeIterFlags::PREFETCH | BtreeIterFlags::ALL_SNAPSHOTS);
        iter.for_each(&trans, |t, iter, _k| {
            // SAFETY: used before the unlock below
            if let Some(l) = unsafe { iter.leaf(t) } {
                let b = l.node();
                let end = b.contents().key.k.p;
                if end > self.prev_node {
                    prt!(out, "{}", b.to_text(fs));
                    self.prev_node = end;
                }
                l.bfloat_to_text(out);
            }

            t.unlock();
            self.from = iter.pos().successor();
            out.flush()
        })
    }
}

/* The filesystem's */

#[derive(Default)]
struct CachedBtreeNodes {
    /* the slot in bc->nodes to go on from */
    idx: usize,
}


fn cached_btree_node_to_text(out: &mut Printbuf, fs: &Fs, b: &BtreeNode) {
    if out.nr_tabstops == 0 {
        out.tabstop_push(36);
    }

    prt!(out, "{:016x} ", core::ptr::from_ref(b) as usize);
    prt!(out, "btree={} level={}", Btree(b.c.btree_id.into()), b.c.level);
    prt!(out, "\n");

    let mut out = out.indent(2);

    // XXX: unlocked, as C's is - a key update racing with this prints a torn
    // key; contents() wants the node lock held
    prt!(out, "{}\n", unsafe { b.contents() }.key_i().to_text(fs));

    prt!(out, "flags:\t{}\n", b.flags);

    prt!(out, "pcpu read locks:\t{}\n",     b.c.lock.six().readers.allocated() as u32);
    prt!(out, "written:\t{}\n",             b.written.get());
    prt!(out, "writes blocked:\t{}\n",
         u32::from(!unsafe { (*b.write_blocked.data_ptr()).is_empty() }));
    prt!(out, "will make reachable:\t{:x}\n", b.will_make_reachable.raw());

    for w in &b.writes {
        prt!(out, "journal pin {:016x}:\t{}\n", core::ptr::from_ref(&w.journal) as usize, w.journal.seq());
    }
}

impl DebugFile for CachedBtreeNodes {
    type Obj = Fs;

    fn read(&mut self, fs: &Fs, out: &mut DebugOut<'_>) -> Result<(), BchError> {
        out.print_each(|out| {
            let rcu = rcu::read_lock();
            // a struct btree isn't freed while the cache is up, only reused
            let next = fs.btree.cache.nodes.items_from(self.idx, &rcu)
                .map(|(i, b)| (i, unsafe { b.cast::<BtreeNode>().as_ref() }))
                .find(|(_, b)| b.hash_val() != 0);
            let Some((i, b)) = next else { return Ok(false) };

            cached_btree_node_to_text(out, fs, b);
            self.idx = i + 1;
            Ok(true)
        })
    }
}

/// The transactions holding the SRCU lock btree nodes are freed under - the
/// others have nothing locked to show. The cursor is the last one's address.
#[derive(Default)]
struct BtreeTransactions {
    cursor: usize,
}

impl DebugFile for BtreeTransactions {
    type Obj = Fs;

    fn read(&mut self, fs: &Fs, out: &mut DebugOut<'_>) -> Result<(), BchError> {
        let _srcu = fs.btree.trans.barrier.read_lock();

        fs.for_each_trans_from(&mut self.cursor, |t| t.srcu_held(), |t| {
            prt!(out, "{}", t);
            prt!(out, "backtrace:\n");
            t.backtrace_to_text(&mut out.indent(2));
            prt!(out, "\n");

            // its ref goes before the flush: that can fault, and wait
            drop(t);
            out.flush()
        })
    }
}

#[derive(Default)]
struct JournalPins {
    seq: u64,
}

impl DebugFile for JournalPins {
    type Obj = Fs;

    fn read(&mut self, fs: &Fs, out: &mut DebugOut<'_>) -> Result<(), BchError> {
        out.print_each(|out| {
            let done = fs.journal.seq_pins_to_text(out, &mut self.seq, None);
            self.seq += 1;
            Ok(!done)
        })
    }
}

/// By transaction_fn() index; 0, "(unknown)", is skipped.
struct TransactionStats {
    idx: usize,
}

impl Default for TransactionStats {
    fn default() -> Self {
        TransactionStats { idx: 1 }
    }
}

impl DebugFile for TransactionStats {
    type Obj = Fs;

    // btree_transaction_stats_open() didn't set may_vmalloc
    const MAY_VMALLOC: bool = false;

    fn read(&mut self, fs: &Fs, out: &mut DebugOut<'_>) -> Result<(), BchError> {
        out.print_each(|out| {
            let Some(name) = transaction_fn(self.idx) else {
                return Ok(false);
            };
            out.write_bytes(name.to_bytes());
            prt!(out, ":\n");

            let s = &fs.btree.trans.stats[self.idx];
            let mut out = out.indent(2);
            let locked = s.lock.lock();

            prt!(out, "Max mem used: {}\n", s.lock.max_mem.get());
            #[cfg(CONFIG_BCACHEFS_TRANS_KMALLOC_TRACE)]
            crate::btree::trans::kmalloc_trace_to_text(&mut out.indent(2), &locked.trans_kmalloc_trace);

            prt!(out, "Transaction duration:\n");
            prt!(out.indent(2), "{}", s.duration);

            if cfg!(CONFIG_BCACHEFS_LOCK_TIME_STATS) {
                prt!(out, "Lock hold times:\n");
                prt!(out.indent(2), "{}", s.lock_hold_times);

                prt!(out, "Lock wait times:\n");
                prt!(out.indent(2), "{}", s.lock_wait_times);
            }

            if !locked.max_paths_text.as_bytes().is_empty() {
                prt!(out, "Maximum allocated btree paths ({}):\n", s.lock.nr_max_paths.get());
                out.indent(2).write_bytes(locked.max_paths_text.as_bytes());
            }

            prt!(out, "\n");
            self.idx += 1;
            Ok(true)
        })
    }
}

#[derive(Default)]
struct BtreeNodeScan {
    next: usize,
}

impl DebugFile for BtreeNodeScan {
    type Obj = Fs;

    fn read(&mut self, fs: &Fs, out: &mut DebugOut<'_>) -> Result<(), BchError> {
        out.print_each(|out| {
            let nodes = fs.btree.node_scan.lock();
            if self.next >= nodes.len() {
                return Ok(false);
            }

            // the nodes are kept in eytzinger order, for searching
            nodes[Eytzinger0::<usize>::of(&nodes).from_inorder(self.next)].to_text(out, fs);
            self.next += 1;
            Ok(true)
        })
    }
}

/// fsck_damaged_paths: the full list of paths fsck found damage on - the
/// end-of-fsck summary prints only the first FSCK_DAMAGED_PATHS_PRINT, which
/// isn't enough when triaging a badly damaged filesystem. Interim, until the
/// damage btree lands.
#[derive(Default)]
struct FsckDamagedPaths {
    next: usize,
}

impl DebugFile for FsckDamagedPaths {
    type Obj = Fs;

    fn read(&mut self, fs: &Fs, out: &mut DebugOut<'_>) -> Result<(), BchError> {
        if !may_iterate(fs) {
            return Ok(());
        }

        let trans = btree_trans!(fs);
        out.print_each(|out| {
            // Copied under the lock, resolved outside it: resolving takes
            // btree locks, and fsck's error paths take the errors lock under those
            let entry = fs.errors.state.lock().damaged_paths.get(self.next).copied();
            let Some(entry) = entry else {
                return Ok(false);
            };

            crate::init::error::damaged_path_to_text(out, &trans, &entry);
            self.next += 1;

            // Before the flush: it relocks, via lockrestart_do()
            trans.unlock();
            Ok(true)
        })
    }
}

/// A file printed whole by its first read: bch2_simple_print()'s.
trait PrintOnce: 'static {
    fn print(fs: &Fs, out: &mut Printbuf);
}

struct Once<P>(bool, PhantomData<fn() -> P>);

impl<P> Default for Once<P> {
    fn default() -> Self {
        Once(false, PhantomData)
    }
}

impl<P: PrintOnce> DebugFile for Once<P> {
    type Obj = Fs;

    fn read(&mut self, fs: &Fs, out: &mut DebugOut<'_>) -> Result<(), BchError> {
        if !self.0 {
            P::print(fs, out);
            self.0 = true;
        }
        Ok(())
    }
}

enum BtreeUpdates {}

impl PrintOnce for BtreeUpdates {
    fn print(fs: &Fs, out: &mut Printbuf) {
        fs.btree_updates_to_text(out)
    }
}

enum WritePoints {}

impl PrintOnce for WritePoints {
    fn print(fs: &Fs, out: &mut Printbuf) {
        fs.write_points_to_text(out)
    }
}

enum BtreeDeadlock {}

impl PrintOnce for BtreeDeadlock {
    fn print(fs: &Fs, out: &mut Printbuf) {
        let mut cursor = 0;
        let Ok(()) = fs.for_each_trans_from(&mut cursor, |_| true, |t| {
            let found = t.check_for_deadlock(out);
            drop(t);
            Ok::<_, Infallible>(if found { ControlFlow::Break(()) } else { ControlFlow::Continue(()) })
        });
    }
}

/* Creating and removing them */

/// /sys/kernel/debug/bcachefs/, if debugfs is up.
static BCH_DEBUG: AtomicPtr<c::dentry> = AtomicPtr::new(core::ptr::null_mut());

fn dentry_ptr(d: Option<NonNull<c::dentry>>) -> *mut c::dentry {
    d.map_or(core::ptr::null_mut(), NonNull::as_ptr)
}

pub fn debug_exit(fs: &Fs) {
    if let Some(dir) = NonNull::new(fs.fs_debug_dir) {
        debugfs::remove_recursive(dir);
    }
}

/// bcachefs/<fs>/: named by the external UUID if the filesystem has ever had
/// more than one device, by its one device otherwise.
pub fn debug_init(fs: &Fs) {
    let Some(root) = NonNull::new(BCH_DEBUG.load(Ordering::Acquire)) else { return };

    let mut name = Printbuf::new();
    if fs.sb.multi_device.get() {
        prt!(&mut name, "{}", fs.sb.user_uuid());
    } else {
        name.write_bytes(fs.name().to_bytes());
    }
    if name.allocation_failure() {
        return;
    }

    let dir = debugfs::create_dir(name.as_c_str(), Some(root));
    // SAFETY: filesystem init, before anything reads it
    unsafe { (*fs.as_ptr()).fs_debug_dir = dentry_ptr(dir) };
    let Some(dir) = dir else { return };

    // SAFETY, for each file: the filesystem outlives them - bch2_fs_debug_exit()
    // removes them first
    unsafe {
        debugfs::create_file::<CachedBtreeNodes>(c"cached_btree_nodes", dir, fs);
        debugfs::create_file::<BtreeTransactions>(c"btree_transactions", dir, fs);
        debugfs::create_file::<JournalPins>(c"journal_pins", dir, fs);
        debugfs::create_file::<Once<BtreeUpdates>>(c"btree_updates", dir, fs);
        debugfs::create_file::<TransactionStats>(c"btree_transaction_stats", dir, fs);
        debugfs::create_file::<Once<BtreeDeadlock>>(c"btree_deadlock", dir, fs);
        debugfs::create_file::<BtreeNodeScan>(c"btree_node_scan", dir, fs);
        debugfs::create_file::<FsckDamagedPaths>(c"fsck_damaged_paths", dir, fs);
        debugfs::create_file::<Once<WritePoints>>(c"write_points", dir, fs);
    }

    crate::debug::async_objs::debugfs_init(fs, dir);

    let btrees = debugfs::create_dir(c"btrees", Some(dir));
    unsafe { (*fs.as_ptr()).btree_debug_dir = dentry_ptr(btrees) };
    let Some(btrees) = btrees else { return };

    for bd in fs.btree_debug.iter() {
        let name = unsafe { CStr::from_ptr(c::bch2_btree_id_str(Btree(bd.id.get())).cast()) };
        let Some(dir) = debugfs::create_dir(name, Some(btrees)) else { continue };

        unsafe {
            debugfs::create_file::<Keys>(c"keys", dir, bd);
            debugfs::create_file::<Formats>(c"formats", dir, bd);
            debugfs::create_file::<BfloatFailed>(c"bfloat-failed", dir, bd);
        }
    }
}

pub fn bch2_debug_exit() {
    if let Some(root) = NonNull::new(BCH_DEBUG.load(Ordering::Acquire)) {
        debugfs::remove_recursive(root);
    }
}

pub fn bch2_debug_init() -> c_int {
    BCH_DEBUG.store(dentry_ptr(debugfs::create_dir(c"bcachefs", None)), Ordering::Release);
    0
}
