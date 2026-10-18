// SPDX-License-Identifier: GPL-2.0

//! /sys/fs/bcachefs/<uuid>/: the filesystem's sysfs files.
//!
//! Each directory is a Dir (util/sysfs.rs), exported to C as the foo_files[]
//! and foo_sysfs_ops that init's KTYPE()s name - the kobjects and their
//! lifetimes are init's. Its files are one of:
//! - a table of Attrs - name, mode, what reading and writing do: the fs dir,
//!   internal/.
//!
//! The fs dir and internal/ are one filesystem's two views, debugging under
//! internal/. A store to either is refused until the filesystem has started
//! (EPERM). Triggers whose work needs the filesystem writable hold a write ref
//! (BCH_WRITE_REF_sysfs) across it - writable(), EROFS without one; the rest -
//! wakeups, cache shrinks, the logged-op knob, work that takes its own ref -
//! need none.

use core::mem::offset_of;

use cstruct_macros::c_export;

use crate::accounting;
use crate::c;
use crate::errcode::BchError;
use crate::fs::{Fs, WriteRef};
use crate::init::passes;
use crate::opts::prt_compression_type;
use crate::util::kernel::{READ, WRITE};
use crate::util::log::LogMsg;
use crate::util::sysfs::{self, parse_human_u64, parse_int, Attr, Dir, Files};
use crate::util::{Printbuf, ToText};

// ── the fs dir ──────────────────────────────────────────────────────────────

struct FsDir;

unsafe impl Dir for FsDir {
    type Obj = Fs;
    const KOBJ: usize = offset_of!(Fs, kobj);

    fn may_store(c: &Fs) -> Result<(), BchError> {
        started(c)
    }
}

const FS: &[Attr<Fs>] = &[
    Attr::ro(c"minor",                  |c, out| writeln!(out, "{}", c.minor)),
    Attr::ro(c"btree_cache_size",       |c, out| out.human_readable_u64(c.btree_cache_size())),
    Attr::ro(c"btree_write_stats",      |c, out| c.btree_write_stats_to_text(out)),

    Attr::ro(c"reconcile_status",       |c, out| c.reconcile_status_to_text(out)),
    Attr::ro(c"reconcile_scan_pending", |c, out| c.reconcile_scan_pending_to_text(out)),
    Attr::ro(c"snapshot_delete_status", |c, out| c.snapshot_delete_status_to_text(out)),
    Attr::ro(c"recovery_status",        |c, out| passes::status_to_text(c, out)),

    Attr::ro(c"compression_stats",      compression_stats_to_text),
    Attr::ro(c"errors",                 |c, out| c.errors_to_text(out)),

    #[cfg(CONFIG_BCACHEFS_TESTS)]
    Attr::wo(c"perf_test",              perf_test),
];

#[c_export]
#[c("struct attribute *bch2_fs_files[]")]
pub static bch2_fs_files: Files<{ FS.len() + 1 }> = Files::new(FS);

#[c_export]
pub static bch2_fs_sysfs_ops: c::sysfs_ops = sysfs::ops::<FsDir>();

// ── internal/ ───────────────────────────────────────────────────────────────

struct InternalDir;

unsafe impl Dir for InternalDir {
    type Obj = Fs;
    const KOBJ: usize = offset_of!(Fs, internal);

    fn may_store(c: &Fs) -> Result<(), BchError> {
        started(c)
    }
}

const INTERNAL: &[Attr<Fs>] = &[
    Attr::ro(c"flags",                  |c, out| c.flags_to_text(out)),
    Attr::ro(c"journal_debug",          |c, out| c.journal.to_text(out)),
    Attr::ro(c"journal_reclaim",        |c, out| c.journal_reclaim_to_text(out)),
    Attr::ro(c"btree_cache",            |c, out| c.btree.cache.to_text(out)),
    Attr::ro(c"btree_key_cache",        |c, out| c.btree.key_cache.to_text(out)),
    Attr::ro(c"btree_reserve_cache",    |c, out| c.btree_reserve_cache_to_text(out)),
    Attr::ro(c"btree_write_buffer",     |c, out| c.btree_write_buffer_to_text(out)),
    Attr::ro(c"new_stripes",            |c, out| c.new_stripes_to_text(out)),
    Attr::ro(c"open_buckets",           |c, out| c.open_buckets_to_text(out, None)),
    Attr::ro(c"open_buckets_partial",   |c, out| c.open_buckets_partial_to_text(out)),
    Attr::ro(c"discards",               |c, out| c.discards_to_text(out, &c.discards.s)),
    Attr::ro(c"write_refs",             |c, out| c.write_refs_to_text(out)),
    Attr::ro(c"nocow_lock_table",       |c, out| c.nocow_locks.to_text(out)),
    Attr::ro(c"replicas",               |c, out| c.replicas.to_text(out)),
    Attr::ro(c"io_timers_read",         |c, out| c.io_clock[READ].to_text(out)),
    Attr::ro(c"io_timers_write",        |c, out| c.io_clock[WRITE].to_text(out)),

    // no write ref: wakeups, shrinks, work that takes its own
    Attr::wo(c"trigger_btree_updates",  |c, _| Ok(c.btree_interior_updates_kick())),
    Attr::wo(c"trigger_btree_cache_shrink",     |c, s| Ok(c.btree.cache.shrink(parse_int(s)?))),
    Attr::wo(c"trigger_btree_key_cache_shrink", |c, s| Ok(c.btree.key_cache.shrink(parse_int(s)?))),
    Attr::wo(c"trigger_discards",       |c, _| Ok(c.do_discards_async())),
    Attr::wo(c"trigger_invalidates",    |c, _| Ok(c.do_invalidates())),
    Attr::wo(c"trigger_freelist_wakeup", |c, _| Ok(c.alloc_wake_all())),
    Attr::wo(c"trigger_recalc_capacity", |c, _| Ok(c.state_lock_read().recalc_capacity())),
    Attr::wo(c"trigger_reconcile_wakeup", |c, _| Ok(c.reconcile_wakeup())),
    Attr::wo(c"trigger_reconcile_pending_wakeup", |c, _| c.reconcile_pending_wakeup()),
    Attr::rw(c"logged_op_fail_next",    |c, out| c.logged_op_fail_next_to_text(out),
                                        |c, s| c.logged_op_fail_next_set(s)),

    Attr::wo(c"trigger_journal_commit", |c, _| writable(c, Fs::journal_flush)),
    Attr::wo(c"trigger_journal_flush",  |c, _| writable(c, |c| {
        c.journal_flush_outstanding_pins();
        c.journal_meta()
    })),
    Attr::wo(c"trigger_journal_writes", |c, _| writable(c, |c| Ok(c.journal_do_writes()))),
    Attr::wo(c"trigger_btree_write_buffer_flush", |c, _| writable(c, write_buffer_flush)),
    Attr::wo(c"trigger_gc",             |c, _| writable(c, Fs::gc_gens)),
    Attr::wo(c"trigger_delete_dead_snapshots", |c, _| writable(c, |c| {
        // debug force: bypass auto_snapshot_deletion; serialize via run_lock
        let _run = passes::run_lock(c);
        crate::snapshots::delete::delete_dead_snapshots_forced(c)
    })),
    Attr::wo(c"trigger_emergency_read_only", |c, _| writable(c, |c| {
        let mut msg = LogMsg::new(c);
        writeln!(msg, "shutdown by sysfs");
        c.emergency_read_only(&mut msg);
        Ok(())
    })),

    Attr::ro(c"gc_gens_pos",            |c, out| writeln!(out, "{}", c.gc_gens.pos)),
    Attr::ro(c"copy_gc_wait",           |c, out| c.copygc_wait_to_text(out)),
    Attr::ro(c"moving_ctxts",           |c, out| c.moving_ctxts_to_text(out)),
    Attr::ro(c"recent_counters",        |c, out| c.counters.recent_to_text(out)),
    Attr::ro(c"internal_uuid",          |c, out| { out.uuid(&c.sb.uuid); out.newline() }),
    Attr::ro(c"disk_groups",            |c, out| c.disk_groups_to_text(out)),
    Attr::ro(c"alloc_debug",            |c, out| {
        c.alloc_debug_to_text(out);
        c.fs_open_buckets_to_text(out);
    }),
    Attr::ro(c"usage_base",             usage_base_to_text),
    Attr::ro(c"filldir64_specialization", |_, out| c::filldir64_specialization_to_text(out)),
];

#[c_export]
#[c("struct attribute *bch2_fs_internal_files[]")]
pub static bch2_fs_internal_files: Files<{ INTERNAL.len() + 1 }> = Files::new(INTERNAL);

#[c_export]
pub static bch2_fs_internal_sysfs_ops: c::sysfs_ops = sysfs::ops::<InternalDir>();

// ── helpers ─────────────────────────────────────────────────────────────────

fn started(c: &Fs) -> Result<(), BchError> {
    if c.flag(c::bch_fs_flags::BCH_FS_started) {
        Ok(())
    } else {
        Err(c::EPERM.into())
    }
}

fn write_ref(c: &Fs) -> Result<WriteRef<'_>, BchError> {
    c.write_ref_tryget(c::bch_write_ref::BCH_WRITE_REF_sysfs)
        .ok_or(c::EROFS.into())
}

/// @f under a write ref - EROFS if the filesystem is read-only.
fn writable(c: &Fs, f: impl FnOnce(&Fs) -> Result<(), BchError>) -> Result<(), BchError> {
    let _ref = write_ref(c)?;
    f(c)
}

fn write_buffer_flush(c: &Fs) -> Result<(), BchError> {
    crate::btree_trans!(c).write_buffer_flush_sync()
}

#[cold]
fn compression_stats_to_text(c: &Fs, out: &mut Printbuf) {
    out.tabstops(&[12, 16, 16, 24]);
    write!(out, "type\tcompressed\runcompressed\raverage extent size\r\n");

    for i in 1..c::BCH_COMPRESSION_TYPE_NR {
        let pos = c::disk_accounting_pos::from_arm(c::bch_acct_compression { type_: i as u8 });
        let mut v = [0u64; 3];
        accounting::mem_read(c, &pos, &mut v);
        let [nr_extents, sectors_uncompressed, sectors_compressed] = v;

        prt_compression_type(out, c::bch_compression_type(i));
        out.tab();

        out.human_readable_u64(sectors_compressed << 9);
        out.tab_rjust();

        out.human_readable_u64(sectors_uncompressed << 9);
        out.tab_rjust();

        out.human_readable_u64((sectors_uncompressed << 9).checked_div(nr_extents).unwrap_or(0));
        out.tab_rjust();
        out.newline();
    }
}

#[cold]
fn usage_base_to_text(c: &Fs, out: &mut Printbuf) {
    let b = c.usage_base();

    write!(out, "hidden:\t\t{}\n",   b.hidden);
    write!(out, "btree:\t\t{}\n",    b.btree);
    write!(out, "data:\t\t{}\n",     b.data);
    write!(out, "cached:\t{}\n",     b.cached);
    write!(out, "reserved:\t\t{}\n", b.reserved);
}

/// "test nr threads": nr may have a unit suffix.
#[cfg_attr(not(CONFIG_BCACHEFS_TESTS), allow(dead_code))]
fn perf_test(c: &Fs, s: &str) -> Result<(), BchError> {
    let _ref = write_ref(c)?;

    // as C's strsep(): each separator splits
    let mut words = s.split([' ', '\t', '\n']);
    let test = words.next().unwrap_or_default();
    let (Some(nr), Some(threads)) = (words.next(), words.next()) else {
        return Err(c::EINVAL.into());
    };
    let threads = parse_int(threads)?;
    let nr = parse_human_u64(nr)?;

    crate::debug::tests::btree_perf_test(c, test, nr, threads)
}
