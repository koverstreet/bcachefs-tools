// SPDX-License-Identifier: GPL-2.0

//! /sys/kernel/debug/bcachefs/<fs>/: each file a DebugFile (os/debugfs.rs).
//!
//! The btree walks don't start until BCH_FS_may_go_rw: until then journal
//! replay is changing the journal keys' gap buffer, which iteration reads.
//! They unlock before each flush: copy_to_user() can fault, and the fault
//! path takes mmap_lock, then snapshots.create_lock in page_mkwrite - which
//! the buffered write path takes before btree locks.

use core::convert::Infallible;
use core::ffi::{c_int, c_uint, CStr};
use core::ops::ControlFlow;

use crate::btree::bkey::prelude::*;
use crate::btree::iter::{lockrestart_do, BtreeIterFlags, BtreeNodeIter};
use crate::btree::BtreeIter;
use crate::btree::node::BtreeNode;
use crate::btree::trans::transaction_fn;
use crate::c;
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::os::debugfs::{DebugFile, DebugOut, DebugRoot};
use crate::os::module::ModuleExit;
use crate::data_structures::eytzinger::Eytzinger0;
use crate::util::place::Contained;
use crate::util::task::{TaskList, TASKS};
use crate::util::{rcu, Printbuf};
use crate::{btree_trans, prt, SPOS_MAX};
use crate::format::{Bpos, Btree};
use cstruct_macros::{placed, Init, MaybeZeroable};
use pin_init::pin_data;

#[placed(Fs.btree_debug[], index = id: c_uint)]
#[pin_data]
#[repr(C)]
#[derive(MaybeZeroable, Init)]
pub struct btree_debug {}


fn may_iterate(fs: &Fs) -> bool {
    fs.flags.get(c::FsFlag::may_go_rw)
}

/* btrees/<btree>/ */

impl c::btree_debug {
    /// The filesystem it's one of the btree_debug[] of.
    fn fs(&self) -> &Fs {
        self.container()
    }
}

enum Keys {}

impl DebugFile for Keys {
    type Obj = c::btree_debug;

    async fn show(bd: &c::btree_debug, out: &mut DebugOut) -> Result<(), BchError> {
        let fs = bd.fs();
        if !may_iterate(fs) {
            return Ok(());
        }

        let mut from = Bpos::default();
        out.print_each(|out| {
            let trans = btree_trans!(fs);
            let mut iter = BtreeIter::new(&*trans, bd.id.get(), from,
                                          BtreeIterFlags::PREFETCH | BtreeIterFlags::ALL_SNAPSHOTS);
            iter.for_each(&trans, |t, iter, k| {
                prt!(out, "{}\n", k.to_text(fs));
                t.unlock();
                from = iter.pos().successor();
                Ok(out.try_flush()?.map_break(|()| true))
            })
        }).await
    }
}

/// formats: each node's format and key counts, level by level, from the
/// leaves up.
enum Formats {}

impl DebugFile for Formats {
    type Obj = c::btree_debug;

    async fn show(bd: &c::btree_debug, out: &mut DebugOut) -> Result<(), BchError> {
        let fs = bd.fs();
        if !may_iterate(fs) {
            return Ok(());
        }

        let mut level = 0;
        let mut from = Bpos::default();
        out.print_each(|out| {
            let trans = btree_trans!(fs);
            while level < c::BTREE_MAX_DEPTH as u32 {
                let mut iter = BtreeNodeIter::new(&*trans, bd.id.get(), from, 0, level,
                                                  BtreeIterFlags::empty());
                loop {
                    let step = lockrestart_do(&trans, |t| {
                        let end = {
                            let Some(n) = iter.peek(t)? else { return Ok(None) };
                            let b = n.node();
                            prt!(out, "{}", b.to_text(fs));
                            b.key().k.p
                        };
                        from = if end != SPOS_MAX { end.successor() } else { end };

                        // C's drop_locks_do(): the next level's iterator is made
                        // from this trans, and needs it locked
                        trans.unlock();
                        let flow = out.try_flush()?;
                        trans.relock()?;
                        Ok(Some((end, flow)))
                    })?;

                    let Some((end, flow)) = step else { break };
                    if flow.is_break() {
                        return Ok(true);
                    }
                    if end == SPOS_MAX {
                        break;
                    }
                    iter.iter().set_pos(end.successor());
                }

                level += 1;
                from = Bpos::default();
            }
            Ok(false)
        }).await
    }
}

/// bfloat-failed: for each key, where the bset search tree's lookup of it
/// failed - after each node, the first time it's reached.
enum BfloatFailed {}

impl DebugFile for BfloatFailed {
    type Obj = c::btree_debug;

    async fn show(bd: &c::btree_debug, out: &mut DebugOut) -> Result<(), BchError> {
        let fs = bd.fs();
        if !may_iterate(fs) {
            return Ok(());
        }

        let mut from = Bpos::default();
        let mut prev_node = Bpos::default();
        out.print_each(|out| {
            let trans = btree_trans!(fs);
            let mut iter = BtreeIter::new(&*trans, bd.id.get(), from,
                                          BtreeIterFlags::PREFETCH | BtreeIterFlags::ALL_SNAPSHOTS);
            iter.for_each(&trans, |t, iter, _k| {
                if let Some(mut l) = iter.node(t) {
                    let b = l.node();
                    let end = b.key().k.p;
                    if end > prev_node {
                        prt!(out, "{}", b.to_text(fs));
                        prev_node = end;
                    }
                    l.bfloat_to_text(out);
                }

                t.unlock();
                from = iter.pos().successor();
                Ok(out.try_flush()?.map_break(|()| true))
            })
        }).await
    }
}

/* The filesystem's */

enum CachedBtreeNodes {}


fn cached_btree_node_to_text(out: &mut Printbuf, fs: &Fs, b: &BtreeNode) {
    if out.nr_tabstops() == 0 {
        out.tabstop_push(36);
    }

    prt!(out, "{:016x} ", core::ptr::from_ref(b) as usize);
    prt!(out, "btree={} level={}", Btree(b.c.btree_id.get().into()), b.c.level.get());
    prt!(out, "\n");

    let mut out = out.indent(2);

    prt!(out, "{}\n", (&*b.key_peek()).to_text(fs));

    prt!(out, "flags:\t{}\n", b.flags);

    prt!(out, "pcpu read locks:\t{}\n",     b.c.lock.six().readers.allocated() as u32);
    prt!(out, "written:\t{}\n",             b.written.get());
    prt!(out, "writes blocked:\t{}\n",      u32::from(b.flags.write_blocked()));
    prt!(out, "will make reachable:\t{:x}\n", b.will_make_reachable.raw());

    for w in &b.writes {
        prt!(out, "journal pin {:016x}:\t{}\n", core::ptr::from_ref(w) as usize, w.seq());
    }
}

impl DebugFile for CachedBtreeNodes {
    type Obj = Fs;

    async fn show(fs: &Fs, out: &mut DebugOut) -> Result<(), BchError> {
        /* the slot in bc->nodes to go on from */
        let mut idx = 0;
        out.print_each(|out| {
            let rcu = rcu::read_lock();
            // a struct btree isn't freed while the cache is up, only reused
            let next = fs.btree.cache.nodes.items_from(idx, &rcu)
                .map(|(i, b)| (i, unsafe { b.cast::<BtreeNode>().as_ref() }))
                .find(|(_, b)| b.hash_val() != 0);
            let Some((i, b)) = next else { return Ok(false) };

            cached_btree_node_to_text(out, fs, b);
            idx = i + 1;
            Ok(true)
        }).await
    }
}

/// The transactions holding the SRCU lock btree nodes are freed under - the
/// others have nothing locked to show. The cursor is the last one's address.
enum BtreeTransactions {}

impl DebugFile for BtreeTransactions {
    type Obj = Fs;

    async fn show(fs: &Fs, out: &mut DebugOut) -> Result<(), BchError> {
        let mut cursor = 0;
        out.print_each(|out| {
            let _srcu = fs.btree.trans.barrier.read_lock();
            let mut full = false;

            fs.for_each_trans_from(&mut cursor, |t| t.srcu_held(), |t| {
                prt!(out, "{}", t);
                prt!(out, "backtrace:\n");
                t.backtrace_to_text(&mut out.indent(2));
                prt!(out, "\n");

                // its ref goes before the flush: that can fault, and wait
                drop(t);
                let flow = out.try_flush()?;
                full = flow.is_break();
                Ok::<_, BchError>(flow)
            })?;
            Ok(full)
        }).await
    }
}

enum JournalPins {}

impl DebugFile for JournalPins {
    type Obj = Fs;

    async fn show(fs: &Fs, out: &mut DebugOut) -> Result<(), BchError> {
        let mut seq = 0;
        out.print_each(|out| {
            let done = fs.journal.seq_pins_to_text(out, &mut seq, None);
            seq += 1;
            Ok(!done)
        }).await
    }
}

/// By transaction_fn() index; 0, "(unknown)", is skipped.
enum TransactionStats {}

impl DebugFile for TransactionStats {
    type Obj = Fs;

    async fn show(fs: &Fs, out: &mut DebugOut) -> Result<(), BchError> {
        let mut idx = 1;
        while let Some(name) = transaction_fn(idx) {
            out.write_bytes(name.to_bytes());
            prt!(out, ":\n");
            transaction_stats_to_text(&mut out.indent(2), fs, idx);
            idx += 1;
            out.flush().await?;
        }
        Ok(())
    }
}

fn transaction_stats_to_text(out: &mut Printbuf, fs: &Fs, idx: usize) {
    let s = &fs.btree.trans.stats[idx];
    let locked = s.lock.lock();

    prt!(out, "Max mem used: {}\n", s.lock.max_mem.get());
    #[cfg(CONFIG_BCACHEFS_TRANS_KMALLOC_TRACE)]
    crate::btree::trans::kmalloc_trace_to_text(&mut out.indent(2), &locked.trans_kmalloc_trace);

    let t = &fs.btree.trans;
    prt!(out, "Transaction duration:\n");
    prt!(out.indent(2), "{}", t.duration.get(idx));

    if cfg!(CONFIG_BCACHEFS_LOCK_TIME_STATS) {
        prt!(out, "Lock hold times:\n");
        prt!(out.indent(2), "{}", t.lock_hold_times.get(idx));

        prt!(out, "Lock wait times:\n");
        prt!(out.indent(2), "{}", t.lock_wait_times.get(idx));
    }

    if !locked.max_paths_text.as_bytes().is_empty() {
        prt!(out, "Maximum allocated btree paths ({}):\n", s.lock.nr_max_paths.get());
        out.indent(2).write_bytes(locked.max_paths_text.as_bytes());
    }

    prt!(out, "\n");
}

enum BtreeNodeScan {}

impl DebugFile for BtreeNodeScan {
    type Obj = Fs;

    async fn show(fs: &Fs, out: &mut DebugOut) -> Result<(), BchError> {
        let mut next = 0;
        out.print_each(|out| {
            let nodes = fs.btree.node_scan.lock();
            if next >= nodes.len() {
                return Ok(false);
            }

            // the nodes are kept in eytzinger order, for searching
            nodes[Eytzinger0::<usize>::of(&nodes).from_inorder(next)].to_text(out, fs);
            next += 1;
            Ok(true)
        }).await
    }
}

/// fsck_damaged_paths: the full list of paths fsck found damage on - the
/// end-of-fsck summary prints only the first FSCK_DAMAGED_PATHS_PRINT, which
/// isn't enough when triaging a badly damaged filesystem. Interim, until the
/// damage btree lands.
enum FsckDamagedPaths {}

impl DebugFile for FsckDamagedPaths {
    type Obj = Fs;

    async fn show(fs: &Fs, out: &mut DebugOut) -> Result<(), BchError> {
        if !may_iterate(fs) {
            return Ok(());
        }

        let mut next = 0;
        out.print_each(|out| {
            let trans = btree_trans!(fs);
            loop {
                // Copied under the lock, resolved outside it: resolving takes
                // btree locks, and fsck's error paths take the errors lock under those
                let entry = fs.errors.state.lock().damaged_paths.get(next).copied();
                let Some(entry) = entry else {
                    return Ok(false);
                };

                crate::init::error::damaged_path_to_text(out, &trans, &entry);
                next += 1;

                // Before the flush: it relocks, via lockrestart_do()
                trans.unlock();
                if out.try_flush()?.is_break() {
                    return Ok(true);
                }
            }
        }).await
    }
}

/* Printed whole, bch2_simple_print()'s */

enum BtreeUpdates {}

impl DebugFile for BtreeUpdates {
    type Obj = Fs;

    async fn show(fs: &Fs, out: &mut DebugOut) -> Result<(), BchError> {
        fs.btree_updates_to_text(out);
        Ok(())
    }
}

enum WritePoints {}

impl DebugFile for WritePoints {
    type Obj = Fs;

    async fn show(fs: &Fs, out: &mut DebugOut) -> Result<(), BchError> {
        fs.write_points_to_text(out);
        Ok(())
    }
}

enum BtreeDeadlock {}

impl DebugFile for BtreeDeadlock {
    type Obj = Fs;

    async fn show(fs: &Fs, out: &mut DebugOut) -> Result<(), BchError> {
        let mut cursor = 0;
        let Ok(()) = fs.for_each_trans_from(&mut cursor, |_| true, |t| {
            let found = t.check_for_deadlock(out);
            drop(t);
            Ok::<_, Infallible>(if found { ControlFlow::Break(()) } else { ControlFlow::Continue(()) })
        });
        Ok(())
    }
}

enum Tasks {}

impl DebugFile for Tasks {
    type Obj = TaskList;

    async fn show(tasks: &TaskList, out: &mut DebugOut) -> Result<(), BchError> {
        let mut next = 0;
        out.print_each(|out| {
            let rcu = rcu::read_lock();
            let Some((idx, task)) = tasks.items_from(next, &rcu).next() else { return Ok(false) };
            task.to_text(out);
            next = idx + 1;
            Ok(true)
        }).await
    }
}

/* Creating and removing them */

/// /sys/kernel/debug/bcachefs/, if debugfs is up.
static BCH_DEBUG: DebugRoot = DebugRoot::new();

pub fn debug_exit(fs: &Fs) {
    drop(unsafe { (*fs.as_ptr()).fs_debug_dir.take() });
}

/// bcachefs/<fs>/: named by the external UUID if the filesystem has ever had
/// more than one device, by its one device otherwise.
pub fn debug_init(fs: &Fs) {
    let mut name = Printbuf::new();
    if fs.sb.multi_device.get() {
        prt!(&mut name, "{}", fs.sb.user_uuid());
    } else {
        name.write_bytes(fs.name().to_bytes());
    }
    if name.allocation_failure() {
        return;
    }

    let Some(fs_dir) = BCH_DEBUG.subdir(name.as_c_str()) else { return };
    let dir = fs_dir.dir();

    // SAFETY, for each file: the filesystem outlives them - debug_exit()
    // removes them first
    unsafe {
        dir.file::<CachedBtreeNodes>(c"cached_btree_nodes", fs);
        dir.file::<BtreeTransactions>(c"btree_transactions", fs);
        dir.file::<JournalPins>(c"journal_pins", fs);
        dir.file::<BtreeUpdates>(c"btree_updates", fs);
        dir.file::<TransactionStats>(c"btree_transaction_stats", fs);
        dir.file::<BtreeDeadlock>(c"btree_deadlock", fs);
        dir.file::<BtreeNodeScan>(c"btree_node_scan", fs);
        dir.file::<FsckDamagedPaths>(c"fsck_damaged_paths", fs);
        dir.file::<WritePoints>(c"write_points", fs);
    }

    crate::debug::async_objs::debugfs_init(fs, dir);

    if let Some(btrees) = dir.subdir(c"btrees") {
        static NAMES: &[&CStr] = &crate::cstr_names!(Btree);
        for bd in fs.btree_debug.iter() {
            let name = NAMES.get(bd.id.get() as usize).copied().unwrap_or(c"(unknown)");
            let Some(dir) = btrees.subdir(name) else { continue };

            unsafe {
                dir.file::<Keys>(c"keys", bd);
                dir.file::<Formats>(c"formats", bd);
                dir.file::<BfloatFailed>(c"bfloat-failed", bd);
            }
        }
    }

    // SAFETY: filesystem init, before anything reads it
    unsafe { (*fs.as_ptr()).fs_debug_dir = Some(fs_dir) };
}

pub fn bch2_debug_exit(m: &ModuleExit) {
    BCH_DEBUG.exit(m)
}

pub fn bch2_debug_init() -> c_int {
    BCH_DEBUG.init(c"bcachefs");
    BCH_DEBUG.file::<Tasks>(c"tasks", &TASKS);
    0
}
