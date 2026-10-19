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
use core::mem::{offset_of, size_of};
use core::ops::ControlFlow;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, Ordering};
use core::marker::PhantomData;

use cstruct_macros::c_export;

use crate::btree::bkey::prelude::*;
use crate::btree::iter::{BtreeIterFlags, BtreeNodeIter};
use crate::btree::BtreeIter;
use crate::c;
use crate::errcode::BchError;
use crate::fs::Fs;
use crate::util::debugfs::{self, DebugFile, DebugOut};
use crate::util::eytzinger::Eytzinger0;
use crate::util::ffi::Opaque;
use crate::util::locking::{mutex_lock, srcu_read_lock};
use crate::util::{rcu, rhashtable, Printbuf};
use crate::{btree_trans, prt, SPOS_MAX};

fn may_iterate(fs: &Fs) -> bool {
    fs.flag(c::bch_fs_flags::BCH_FS_may_go_rw)
}

/* btrees/<btree>/ */

impl c::btree_debug {
    /// The filesystem it's one of the btree_debug[] of.
    fn fs(&self) -> &Fs {
        let offset = offset_of!(Fs, btree_debug) + self.id as usize * size_of::<Self>();
        // SAFETY: a bch_fs's btree_debug[id], which bch2_fs_debug_init() numbered
        unsafe { &*core::ptr::from_ref(self).byte_sub(offset).cast::<Fs>() }
    }
}

#[derive(Default)]
struct Keys {
    from: c::bpos,
}

impl DebugFile for Keys {
    type Obj = c::btree_debug;

    fn read(&mut self, bd: &c::btree_debug, out: &mut DebugOut<'_>) -> Result<(), BchError> {
        let fs = bd.fs();
        if !may_iterate(fs) {
            return Ok(());
        }

        let trans = btree_trans!(fs);
        let mut iter = BtreeIter::new(&*trans, bd.id, self.from,
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
    from:  c::bpos,
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
            let mut iter = BtreeNodeIter::new(&*trans, bd.id, self.from, 0, self.level,
                                              BtreeIterFlags::empty());
            let full = iter.for_each(&trans, |b| {
                prt!(out, "{}", b.to_text(fs));
                self.from = if b.key.k.p != SPOS_MAX { b.key.k.p.successor() } else { b.key.k.p };

                trans.unlock();
                Ok(out.flush()?.map_break(|()| true))
            })?;
            if full {
                return Ok(());
            }

            self.level += 1;
            self.from = c::bpos::default();
        }
        Ok(())
    }
}

/// bfloat-failed: for each key, where the bset search tree's lookup of it
/// failed - after each node, the first time it's reached.
#[derive(Default)]
struct BfloatFailed {
    from:      c::bpos,
    prev_node: c::bpos,
}

impl DebugFile for BfloatFailed {
    type Obj = c::btree_debug;

    fn read(&mut self, bd: &c::btree_debug, out: &mut DebugOut<'_>) -> Result<(), BchError> {
        let fs = bd.fs();
        if !may_iterate(fs) {
            return Ok(());
        }

        let trans = btree_trans!(fs);
        let mut iter = BtreeIter::new(&*trans, bd.id, self.from,
                                      BtreeIterFlags::PREFETCH | BtreeIterFlags::ALL_SNAPSHOTS);
        iter.for_each(&trans, |t, iter, _k| {
            // SAFETY: used before the unlock below
            if let Some(l) = unsafe { iter.leaf(t) } {
                let b = l.node();
                if b.key.k.p > self.prev_node {
                    prt!(out, "{}", b.to_text(fs));
                    self.prev_node = b.key.k.p;
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
    bucket: usize,
}

/// list_empty_careful(): whether @head is empty, read without its lock.
fn list_empty_careful(head: &c::list_head) -> bool {
    let head_ptr = core::ptr::from_ref(head).cast_mut();
    // SAFETY: a list's links, READ_ONCE()d as others change them
    let next = unsafe { core::ptr::read_volatile(&raw const head.next) };
    next == head_ptr && next == unsafe { core::ptr::read_volatile(&raw const head.prev) }
}

fn cached_btree_node_to_text(out: &mut Printbuf, fs: &Fs, b: &c::btree) {
    if out.as_raw().nr_tabstops == 0 {
        out.tabstop_push(36);
    }

    prt!(out, "{:016x} ", core::ptr::from_ref(b) as usize);
    unsafe { c::bch2_btree_id_level_to_text(out.as_raw(), c::btree_id(b.c.btree_id.into()), b.c.level.into()) };
    prt!(out, "\n");

    let mut out = out.indent(2);

    prt!(out, "{}\n", b.key_i().to_text(fs));

    prt!(out, "flags:\t");
    // SAFETY: the BTREE_FLAGS() names, NULL-terminated
    unsafe { out.prt_bitflags((&raw const c::bch2_btree_node_flags).cast(), b.flags as u64) };
    prt!(out, "\n");

    prt!(out, "pcpu read locks:\t{}\n",     !b.c.lock.readers.is_null() as u32);
    prt!(out, "written:\t{}\n",             b.written);
    prt!(out, "writes blocked:\t{}\n",      !list_empty_careful(&b.write_blocked) as u32);
    prt!(out, "will make reachable:\t{:x}\n", b.will_make_reachable);

    for w in &b.writes {
        prt!(out, "journal pin {:016x}:\t{}\n", core::ptr::from_ref(&w.journal) as usize, w.journal.seq);
    }

    prt!(out, "ob:\t{}\n", b.ob.nr);
}

impl DebugFile for CachedBtreeNodes {
    type Obj = Fs;

    fn read(&mut self, fs: &Fs, out: &mut DebugOut<'_>) -> Result<(), BchError> {
        out.print_each(|out| {
            let rcu = rcu::read_lock();
            // SAFETY: a struct btree isn't freed while the cache is up, only
            // reused, so a chain reaches btrees - if not the ones it did
            let table = unsafe {
                rhashtable::read::<c::btree>(&fs.btree.cache.table, offset_of!(c::btree, hash), &rcu)
            };
            if self.bucket >= table.nr_buckets() {
                return Ok(false);
            }

            let mut out = out.atomic();
            for b in table.bucket(self.bucket) {
                cached_btree_node_to_text(&mut out, fs, b);
            }
            self.bucket += 1;
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
        let _srcu = srcu_read_lock(&fs.btree.trans.barrier);

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
            let done = unsafe {
                c::bch2_journal_seq_pins_to_text(out.as_raw(), (&raw const fs.journal).cast_mut(),
                                                 &mut self.seq, core::ptr::null_mut())
            };
            self.seq += 1;
            Ok(!done)
        })
    }
}

/// By bch2_btree_transaction_fns[] index; 0, "(unknown)", is skipped.
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
            // SAFETY: a name, once registered, isn't changed
            let fns = unsafe { &*(&raw const c::bch2_btree_transaction_fns) };
            let Some(&name) = fns.get(self.idx).filter(|name| !name.is_null()) else {
                return Ok(false);
            };
            // SAFETY: registered names are __func__s, which live forever
            out.write_bytes(unsafe { CStr::from_ptr(name.cast()) }.to_bytes());
            prt!(out, ":\n");

            let s = &fs.btree.trans.stats[self.idx];
            let mut out = out.indent(2);
            let _lock = mutex_lock(&s.lock);

            prt!(out, "Max mem used: {}\n", s.max_mem);
            #[cfg(CONFIG_BCACHEFS_TRANS_KMALLOC_TRACE)]
            unsafe {
                c::bch2_trans_kmalloc_trace_to_text(out.indent(2).as_raw(),
                                                    (&raw const s.trans_kmalloc_trace).cast_mut());
            }

            prt!(out, "Transaction duration:\n");
            prt!(out.indent(2), "{}", s.duration);

            if cfg!(CONFIG_BCACHEFS_LOCK_TIME_STATS) {
                prt!(out, "Lock hold times:\n");
                prt!(out.indent(2), "{}", s.lock_hold_times);

                prt!(out, "Lock wait times:\n");
                prt!(out.indent(2), "{}", s.lock_wait_times);
            }

            if !s.max_paths_text.is_null() {
                prt!(out, "Maximum allocated btree paths ({}):\n", s.nr_max_paths);
                // SAFETY: a C string of its own, replaced under s->lock
                out.indent(2).write_bytes(unsafe { CStr::from_ptr(s.max_paths_text.cast()) }.to_bytes());
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
        let f = &fs.btree.node_scan;
        out.print_each(|out| {
            let _lock = mutex_lock(&f.lock);
            let nr = f.nodes.nr;
            if self.next >= nr {
                return Ok(false);
            }

            // the nodes are kept in eytzinger order, for searching
            let idx = Eytzinger0::<usize>::new(nr).from_inorder(self.next);
            unsafe { c::bch2_found_btree_node_to_text(out.as_raw(), fs.as_ptr(), f.nodes.data.add(idx)) };
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
            // btree locks, and fsck's error paths take msgs_lock under those
            let entry = {
                let _lock = mutex_lock(&fs.errors.msgs_lock);
                let paths = &fs.errors.damaged_paths;
                (self.next < paths.nr).then(|| unsafe { *paths.data.add(self.next) })
            };
            let Some(entry) = entry else {
                return Ok(false);
            };

            unsafe { c::bch2_fsck_damaged_path_to_text(out.as_raw(), trans.raw(), &entry) };
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
        unsafe { c::bch2_btree_updates_to_text(out.as_raw(), fs.as_ptr()) }
    }
}

enum WritePoints {}

impl PrintOnce for WritePoints {
    fn print(fs: &Fs, out: &mut Printbuf) {
        unsafe { c::bch2_write_points_to_text(out.as_raw(), fs.as_ptr()) }
    }
}

enum BtreeDeadlock {}

impl PrintOnce for BtreeDeadlock {
    fn print(fs: &Fs, out: &mut Printbuf) {
        let mut cursor = 0;
        let Ok(()) = fs.for_each_trans_from(&mut cursor, |_| true, |t| {
            let found = t.check_for_deadlock(&mut out.atomic());
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

#[c_export]
pub fn bch2_fs_debug_exit(c: &Opaque<c::bch_fs>) {
    if let Some(dir) = NonNull::new(Fs::from_c(c).fs_debug_dir) {
        debugfs::remove_recursive(dir);
    }
}

/// bcachefs/<fs>/: named by the external UUID if the filesystem has ever had
/// more than one device, by its one device otherwise.
#[c_export]
pub fn bch2_fs_debug_init(c: &Opaque<c::bch_fs>) {
    let Some(root) = NonNull::new(BCH_DEBUG.load(Ordering::Acquire)) else { return };
    let fs = Fs::from_c(c);

    let mut name = Printbuf::new();
    if fs.sb.multi_device {
        name.uuid(&fs.sb.user_uuid);
    } else {
        name.write_bytes(fs.name().to_bytes());
    }
    if name.allocation_failure() {
        return;
    }

    let dir = debugfs::create_dir(name.as_c_str(), Some(root));
    // SAFETY: filesystem init, before anything reads it
    unsafe { (*c.as_ptr()).fs_debug_dir = dentry_ptr(dir) };
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

    #[cfg(__KERNEL__)]
    crate::debug::async_objs::debugfs_init(c, dir);

    let btrees = debugfs::create_dir(c"btrees", Some(dir));
    unsafe { (*c.as_ptr()).btree_debug_dir = dentry_ptr(btrees) };
    let Some(btrees) = btrees else { return };

    for (id, bd) in fs.btree_debug.iter().enumerate() {
        // what btree_debug::fs() goes by
        unsafe { (*c.as_ptr()).btree_debug[id].id = id as u32 };

        let name = unsafe { CStr::from_ptr(c::bch2_btree_id_str(c::btree_id(id as u32)).cast()) };
        let Some(dir) = debugfs::create_dir(name, Some(btrees)) else { continue };

        unsafe {
            debugfs::create_file::<Keys>(c"keys", dir, bd);
            debugfs::create_file::<Formats>(c"formats", dir, bd);
            debugfs::create_file::<BfloatFailed>(c"bfloat-failed", dir, bd);
        }
    }
}

#[c_export]
pub fn bch2_debug_exit() {
    if let Some(root) = NonNull::new(BCH_DEBUG.load(Ordering::Acquire)) {
        debugfs::remove_recursive(root);
    }
}

#[c_export]
pub fn bch2_debug_init() -> c_int {
    BCH_DEBUG.store(dentry_ptr(debugfs::create_dir(c"bcachefs", None)), Ordering::Release);
    0
}
