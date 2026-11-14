// SPDX-License-Identifier: GPL-2.0

//! /sys/fs/bcachefs/<uuid>/: the filesystem's sysfs files.
//!
//! The filesystem's directory is its kobject's, FsDir; its subdirectories are
//! named attribute groups of it (util/kobject.rs), so FsDir reads and writes
//! their files too, each told by the table its attribute is in. A device's
//! dir is DevDir, its kobject's. Files are one of:
//! - a table of Attrs - name, mode, what reading and writing do: the fs dir,
//!   internal/, a device's own files;
//! - a ListAttrs, a file per entry of an x-macro list: time_stats/,
//!   time_stats_json/, counters/;
//! - the options' own attributes, those of the options that apply: options/,
//!   and a device's options in its dir. opts.c is still C: opt_show() and
//!   opt_store() are over opts.rs's wrappers.
//!
//! internal/btree_trans_stats_json is a bin_attribute, apart from the rest.
//!
//! The fs dir and internal/ are one filesystem's two views, debugging under
//! internal/. A store to either is refused until the filesystem has started
//! (EPERM). Triggers whose work needs the filesystem writable hold a write ref
//! across it - writable(), EROFS without one; the rest -
//! wakeups, cache shrinks, the logged-op knob, work that takes its own ref -
//! need none.

use crate::btree::trans::transaction_fns;
use crate::fs::Dev;
use core::ffi::CStr;


use crate::accounting;
use crate::c;
use crate::errcode::{Errcode, BchError};
use crate::format::{CompressionType, DataType};
use crate::fs::{Fs, WriteRef};
use crate::init::passes;
use crate::locking::MemallocFlags;
use crate::opts::{self, OptChange, OptType};
use crate::sb::counters::COUNTERS;
use crate::sb::members;
use crate::prt;
use crate::util::ffi::c_char;
use crate::os::kernel::{READ, WRITE};
use crate::util::log::LogMsg;
use crate::util::time_stats::TimeStats;
use crate::util::refcount::TryRefcount;
use crate::util::kobject::{BinFile, BinFiles, Group, Groups, Kobject};
use crate::util::place::Placed;
use crate::util::printbuf::parse_human_u64;
use crate::util::sysfs::{self, parse_int, Attr, Dir, Files, ListAttrs};
use crate::util::{HumanReadable, Printbuf, ToText};

// ── the fs dir ──────────────────────────────────────────────────────────────

pub struct FsDir;

unsafe impl Dir for FsDir {
    type Obj = Fs;
    type Kobj = Placed<Kobject, crate::at!(Fs.kobj)>;

    fn show(c: &Fs, attr: &c::attribute, out: &mut Printbuf) -> Result<(), BchError> {
        if let Some(i) = TIME_STATS.index(attr) {
            c.times[i].to_text(out);
        } else if let Some(i) = TIME_STATS_JSON.index(attr) {
            c.times[i].json_to_text(out);
        } else if let Some(i) = COUNTER_FILES.index(attr) {
            counter_to_text(c, i, out);
        } else if let Some(id) = opt_from_attr(attr) {
            return opt_show(c, None, id, out);
        } else {
            return unsafe { sysfs::attr_show(c, attr, out) };
        }
        Ok(())
    }

    fn store(c: &Fs, attr: &c::attribute, s: &str) -> Result<(), BchError> {
        if let Some(i) = TIME_STATS.index(attr).or_else(|| TIME_STATS_JSON.index(attr)) {
            c.times[i].reset();
        } else if let Some(i) = COUNTER_FILES.index(attr) {
            c.counter_reset(i);
        } else if let Some(id) = opt_from_attr(attr) {
            return opt_store(c, None, id, s);
        } else {
            started(c)?;
            return unsafe { sysfs::attr_store(c, attr, s) };
        }
        Ok(())
    }
}

const FS: &[Attr<Fs>] = &[
    Attr::ro(c"minor",                  |c, out| writeln!(out, "{}", c.minor)),
    Attr::ro(c"btree_cache_size",       |c, out| prt!(out, "{}", HumanReadable(c.btree_cache_size()))),
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

// ── internal/ ───────────────────────────────────────────────────────────────

const INTERNAL: &[Attr<Fs>] = &[
    Attr::ro(c"flags",                  |c, out| c.flags.to_text(out)),
    Attr::ro(c"journal_debug",          |c, out| c.journal.to_text(out)),
    Attr::ro(c"journal_reclaim",        |c, out| c.journal_reclaim_to_text(out)),
    Attr::ro(c"btree_cache",            |c, out| c.btree.cache.to_text(out)),
    Attr::ro(c"btree_key_cache",        |c, out| c.btree.key_cache.to_text(out)),
    Attr::ro(c"btree_reserve_cache",    |c, out| c.btree_reserve_cache_to_text(out)),
    Attr::ro(c"btree_write_buffer",     |c, out| c.btree_write_buffer_to_text(out)),
    Attr::ro(c"new_stripes",            |c, out| c.new_stripes_to_text(out)),
    Attr::ro(c"open_buckets",           |c, out| c.open_buckets_to_text(out, None)),
    Attr::ro(c"open_buckets_partial",   |c, out| c.open_buckets_partial_to_text(out)),
    Attr::ro(c"discards",               |c, out| c.discards_to_text(out)),
    Attr::ro(c"nocow_lock_table",       |c, out| c.nocow_locks.to_text(out)),
    Attr::ro(c"replicas",               |c, out| c.replicas.read().to_text(out)),
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
        // Deletion stopping because it was told to isn't the write failing -
        // and cancelled's errno is EINTR, which a write must not return:
        match crate::snapshots::delete::delete_dead_snapshots_forced(c) {
            Err(e) if e.matches(Errcode::BCH_ERR_cancelled) => Ok(()),
            ret => ret,
        }
    })),
    Attr::wo(c"trigger_emergency_read_only", |c, _| writable(c, |c| {
        let mut msg = LogMsg::new(c);
        writeln!(msg, "shutdown by sysfs");
        c.emergency_read_only(&mut msg);
        Ok(())
    })),

    Attr::ro(c"gc_gens_pos",            |c, out| crate::prt!(out, "{}\n", c.gc_gens.pos.get())),
    Attr::ro(c"copy_gc_wait",           |c, out| c.copygc_wait_to_text(out)),
    Attr::ro(c"recent_counters",        |c, out| c.counters.recent_to_text(out)),
    Attr::ro(c"internal_uuid",          |c, out| prt!(out, "{}\n", c.sb.uuid())),
    Attr::ro(c"disk_groups",            |c, out| c.disk_groups_to_text(out)),
    Attr::ro(c"alloc_debug",            |c, out| {
        c.alloc_debug_to_text(out);
        c.fs_open_buckets_to_text(out);
    }),
    Attr::ro(c"usage_base",             usage_base_to_text),
];

// internal/btree_trans_stats_json, a bin_attribute: kernfs reads one a page a
// call, so the JSON is made only at offset 0 - kept in
// c->btree.trans.stats_json - and the pages read are of one whole.

/// Each btree transaction's name, up to the first not yet registered, and its
/// stats.
fn trans_stats(c: &Fs) -> impl Iterator<Item = (&'static [u8], &c::btree_transaction_stats)> {
    transaction_fns()
        .map(CStr::to_bytes)
        .zip(c.btree.trans.stats.iter())
}

fn trans_stats_json(c: &Fs, out: &mut Printbuf) {
    write!(out, "{{");
    for (i, (name, s)) in trans_stats(c).enumerate() {
        if i > 0 {
            write!(out, ",");
        }
        write!(out, "\"");
        out.write_bytes(name);
        write!(out, "\":{{");

        write!(out, "\"max_mem\":{},", s.lock.max_mem.get());

        write!(out, "\"duration\":");
        s.duration.json_to_text(out);

        if cfg!(CONFIG_BCACHEFS_LOCK_TIME_STATS) {
            write!(out, ",\"lock_hold_times\":");
            s.lock_hold_times.json_to_text(out);
            write!(out, ",\"lock_wait_times\":");
            s.lock_wait_times.json_to_text(out);
        }

        write!(out, "}}");
    }
    write!(out, "}}\n");
}

unsafe extern "C" fn trans_stats_json_read(_file: *mut c::file, kobj: *mut c::kobject,
                                           _attr: *const c::bin_attribute, buf: *mut c_char,
                                           off: c::loff_t, count: usize) -> isize {
    // SAFETY: the filesystem's kobject, which has this file
    let c = unsafe { sysfs::obj_of::<FsDir>(kobj) };

    let mut json = c.btree.trans.stats_json.lock();
    let out = &mut json.buf;

    if off == 0 {
        out.reset();
        trans_stats_json(c, out);
        if out.allocation_failure() {
            return -(c::ENOMEM as isize);
        }
    }

    let bytes = out.as_bytes();
    let Some(rest) = bytes.get(off as usize..).filter(|r| !r.is_empty()) else {
        return 0;
    };
    let n = count.min(rest.len());
    // SAFETY: kernfs's buffer, of @count bytes
    unsafe { core::ptr::copy_nonoverlapping(rest.as_ptr(), buf.cast::<u8>(), n) };
    n as isize
}

/// Any write resets every transaction's stats.
unsafe extern "C" fn trans_stats_json_write(_file: *mut c::file, kobj: *mut c::kobject,
                                            _attr: *const c::bin_attribute, _buf: *mut c_char,
                                            _off: c::loff_t, count: usize) -> isize {
    // SAFETY: the filesystem's kobject, which has this file
    let c = unsafe { sysfs::obj_of::<FsDir>(kobj) };

    for (_, s) in trans_stats(c) {
        let mut locked = s.lock.lock();
        s.duration.reset();
        s.lock_hold_times.reset();
        s.lock_wait_times.reset();

        locked.published().nr_max_paths.set(0);
        locked.published().max_mem.set(0);
        locked.max_paths_text = Printbuf::new();
    }
    count as isize
}

static TRANS_STATS_JSON: BinFile = BinFile({
    let mut a: c::bin_attribute = ::pin_init::zeroed();
    a.attr.name = c"btree_trans_stats_json".as_ptr().cast();
    a.attr.mode = 0o644;
    a.read = Some(trans_stats_json_read);
    a.write = Some(trans_stats_json_write);
    a
});

// ── time_stats/, time_stats_json/ ───────────────────────────────────────────
//
// A file per time stat, the same attributes in both; writing one resets the
// stat.

/// An x-macro entry's name, as a file's.
macro_rules! file_name {
    ($name:ident) => {
        match CStr::from_bytes_with_nul(concat!(stringify!($name), "\0").as_bytes()) {
            Ok(name) => name,
            Err(_)   => panic!(),
        }
    };
}

macro_rules! time_stat_names {
    ($(($name:ident $(, $($rest:tt)*)?)),* $(,)?) => {
        const TIME_STAT_NAMES: [&CStr; c::TimeStat::NR as usize] = [$(file_name!($name),)*];
    };
}
c::TimeStat_arms!(time_stat_names);

// Each its own table: a group's files are its kobject's, so which directory
// a file's in is which table its attribute is from
static TIME_STATS: ListAttrs<{ c::TimeStat::NR as usize }> = ListAttrs::new(&TIME_STAT_NAMES, 0o644);
static TIME_STATS_JSON: ListAttrs<{ c::TimeStat::NR as usize }> = ListAttrs::new(&TIME_STAT_NAMES, 0o644);

// ── counters/ ───────────────────────────────────────────────────────────────
//
// A file per BCH_PERSISTENT_COUNTERS() entry: the count since mount and
// since the filesystem was made; writing one resets it.

macro_rules! counter_names {
    ($(($name:ident $(, $($rest:tt)*)?)),* $(,)?) => {
        const COUNTER_NAMES: [&CStr; c::PersistentCounter::NR as usize] = [$(file_name!($name),)*];
    };
}
c::PersistentCounter_arms!(counter_names);

static COUNTER_FILES: ListAttrs<{ c::PersistentCounter::NR as usize }> = ListAttrs::new(&COUNTER_NAMES, 0o644);

fn counter_to_text(c: &Fs, i: usize, out: &mut Printbuf) {
    let count = c.counter_read(i);
    let since_mount = count.wrapping_sub(c.counters.mount(i));

    out.tabstop_push(32);
    if COUNTERS[i].is_sectors {
        prt!(out, "since mount:\t{}\nsince filesystem creation:\t{}\n",
             HumanReadable(since_mount << 9), HumanReadable(count << 9));
    } else {
        prt!(out, "since mount:\t{since_mount}\nsince filesystem creation:\t{count}\n");
    }
}

// ── options/ ────────────────────────────────────────────────────────────────
//
// An option's file is its attribute in OPT_FILES: options/ has the filesystem
// options', and a device's dir the device options', among its own files.

/// Each option's file, by OptId - which option a file is, from where
/// its attribute is: writable if it may be set at runtime.
static OPT_FILES: ListAttrs<{ opts::NR }> = ListAttrs::with_modes(&crate::cstr_names!(c::OptId), &{
    let mut modes = [0o444; opts::NR];
    let mut i = 0;
    while i < opts::NR {
        if c::bch_opts::TABLE[i].flags.bits() & c::OptFlags::OPT_RUNTIME.bits() != 0 {
            modes[i] = 0o644;
        }
        i += 1;
    }
    modes
});

/// The option @attr is the file of, if it's one.
fn opt_from_attr(attr: &c::attribute) -> Option<c::OptId> {
    OPT_FILES.index(attr).map(|i| c::OptId(i as u32))
}

fn opt_show(c: &Fs, ca: Option<&Dev>, id: c::OptId, out: &mut Printbuf)
    -> Result<(), BchError>
{
    let opt = id.opt();

    let sb = c.sb_lock();

    let v = match ca {
        Some(ca) if matches!(opt.ty, OptType::StrMember { .. }) => {
            // The value lives in the member, not a u64 - render it here:
            let Some(m) = members::sb_member(sb.sb(), ca.dev_idx as u32) else {
                return c.throw(Errcode::BCH_ERR_ENOENT_dev_idx_not_found);
            };
            out.write_bytes(opt.member_str(&m));
            out.write_bytes(b"\n");
            return Ok(());
        }
        Some(ca) => {
            if !(opt.has(c::OptFlags::OPT_DEVICE) && opt.member_field().is_some()) {
                return c.throw(Errcode::BCH_ERR_EINVAL_sysfs_opt_not_found);
            }
            opts::opt_from_sb(sb.sb().as_unvalidated(), id, Some(ca.dev_idx.into()))
        }
        None => {
            if !opt.has(c::OptFlags::OPT_FS) {
                return c.throw(Errcode::BCH_ERR_EINVAL_sysfs_opt_not_found);
            }
            c.opts.get(id)
        }
    };

    opts::opt_to_text(out, Some(c), None, opt, v, c::OPT_SHOW_FULL_LIST);
    out.write_bytes(b"\n");
    Ok(())
}

fn opt_store(c: &Fs, ca: Option<&Dev>, id: c::OptId, s: &str) -> Result<(), BchError> {
    let opt = id.opt();
    // strim()
    let val = s.trim_matches(|ch| matches!(ch, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r')).as_bytes();

    // We don't need c->writes for correctness, but it eliminates an
    // unsightly error message in the dmesg log when we're RO:
    let _ref = write_ref(c)?;
    let _noio = MemallocFlags::noio();
    let mut change = OptChange::new(c);

    let v = opts::opt_parse(Some(c), opt, Some(val), None)?;
    change.pre_set(ca, 0, id, v)?;

    if change.set(ca, opt, v, val)? {
        change.post_set(ca, 0, id, v);
    }
    Ok(())
}

/// Which options have a file: those of the filesystem's, @fs, or a device's
/// - but none that's hidden.
const fn opts_with_files(fs: bool) -> [bool; opts::NR] {
    let (hidden, fs_opt, dev_opt) =
        (c::OptFlags::OPT_HIDDEN.bits(), c::OptFlags::OPT_FS.bits(), c::OptFlags::OPT_DEVICE.bits());
    let mut which = [false; opts::NR];
    let mut i = 0;
    while i < opts::NR {
        let flags = c::bch_opts::TABLE[i].flags.bits();
        /*
         * For options that are both OPT_FS and OPT_DEVICE (currently only
         * @discard), expose only the per-device sysfs entry: the FS-level
         * entry would alias to c->opts but member-backed options have no
         * FS-wide superblock field, so writes via the FS path are silent
         * no-ops and reads can't disambiguate which scope is being shown.
         * Mount-time @-o discard= remains the only fs-scope handle.
         */
        which[i] = flags & hidden == 0 &&
            if fs { flags & fs_opt != 0 && flags & dev_opt == 0 } else { flags & dev_opt != 0 };
        i += 1;
    }
    which
}

const fn nr_true<const N: usize>(which: &[bool; N]) -> usize {
    let mut nr = 0;
    let mut i = 0;
    while i < N {
        nr += which[i] as usize;
        i += 1;
    }
    nr
}

const FS_OPTS: [bool; opts::NR] = opts_with_files(true);
const DEV_OPTS: [bool; opts::NR] = opts_with_files(false);

// ── the fs dir's groups ─────────────────────────────────────────────────────

static FS_FILES: Files<{ FS.len() + 1 }> = Files::new(FS);
static INTERNAL_FILES: Files<{ INTERNAL.len() + 1 }> = Files::new(INTERNAL);
static INTERNAL_BIN_FILES: BinFiles<2> = BinFiles::new([&TRANS_STATS_JSON]);
static OPTS_FILES: Files<{ nr_true(&FS_OPTS) + 1 }> = Files::with_list::<Fs, { opts::NR }>(&[], &OPT_FILES, &FS_OPTS);
#[cfg(not(CONFIG_BCACHEFS_NO_LATENCY_ACCT))]
static TIME_STATS_FILES: Files<{ c::TimeStat::NR as usize + 1 }> = Files::list(&TIME_STATS);
#[cfg(not(CONFIG_BCACHEFS_NO_LATENCY_ACCT))]
static TIME_STATS_JSON_FILES: Files<{ c::TimeStat::NR as usize + 1 }> = Files::list(&TIME_STATS_JSON);
static COUNTERS_FILES: Files<{ c::PersistentCounter::NR as usize + 1 }> = Files::list(&COUNTER_FILES);

static FS_GROUP: Group = Group::new(None, &FS_FILES);
static INTERNAL_GROUP: Group = Group::with_bin(Some(c"internal"), &INTERNAL_FILES, &INTERNAL_BIN_FILES);
static OPTS_GROUP: Group = Group::new(Some(c"options"), &OPTS_FILES);
#[cfg(not(CONFIG_BCACHEFS_NO_LATENCY_ACCT))]
static TIME_STATS_GROUP: Group = Group::new(Some(c"time_stats"), &TIME_STATS_FILES);
#[cfg(not(CONFIG_BCACHEFS_NO_LATENCY_ACCT))]
static TIME_STATS_JSON_GROUP: Group = Group::new(Some(c"time_stats_json"), &TIME_STATS_JSON_FILES);
static COUNTERS_GROUP: Group = Group::new(Some(c"counters"), &COUNTERS_FILES);

const FS_GROUPS_NR: usize = if cfg!(CONFIG_BCACHEFS_NO_LATENCY_ACCT) { 4 } else { 6 };

/// The filesystem's kobject's groups: its files, and its subdirectories
pub(crate) static FS_GROUPS: Groups<{ FS_GROUPS_NR + 1 }> = Groups::new([
    &FS_GROUP,
    &INTERNAL_GROUP,
    &OPTS_GROUP,
    #[cfg(not(CONFIG_BCACHEFS_NO_LATENCY_ACCT))]
    &TIME_STATS_GROUP,
    #[cfg(not(CONFIG_BCACHEFS_NO_LATENCY_ACCT))]
    &TIME_STATS_JSON_GROUP,
    &COUNTERS_GROUP,
]);

// ── a device's dir: /sys/fs/bcachefs/<uuid>/dev-N/ ───────────────────────────
//
// Its own files, and the device options' - added at runtime, as options/'s.

const DEV: &[Attr<Dev>] = &[
    Attr::ro(c"uuid",                   |ca, out| prt!(out, "{}\n", ca.uuid)),
    Attr::ro(c"first_bucket",           |ca, out| writeln!(out, "{}", ca.mi.get().first_bucket)),
    Attr::ro(c"nbuckets",               |ca, out| writeln!(out, "{}", ca.mi.get().nbuckets)),

    Attr::ro(c"has_data",               |ca, out| {
        ca.has_data().to_text(out);
        out.newline();
    }),
    Attr::ro(c"io_done",                io_done_to_text),
    Attr::ro(c"io_errors",              |ca, out| ca.io_errors_to_text(out)),
    Attr::wo(c"io_errors_reset",        |ca, _| Ok(ca.errors_reset())),

    Attr::ro(c"io_latency_read",        |ca, out| writeln!(out, "{}", ca.cur_latency(READ))),
    Attr::ro(c"io_latency_write",       |ca, out| writeln!(out, "{}", ca.cur_latency(WRITE))),
    Attr::ro(c"io_latency_stats_read",  |ca, out| ca.io_latency[READ].to_text(out)),
    Attr::ro(c"io_latency_stats_write", |ca, out| ca.io_latency[WRITE].to_text(out)),
    Attr::ro(c"io_latency_stats_read_json",  |ca, out| ca.io_latency[READ].json_to_text(out)),
    Attr::ro(c"io_latency_stats_write_json", |ca, out| ca.io_latency[WRITE].json_to_text(out)),
    #[cfg(not(CONFIG_BCACHEFS_NO_LATENCY_ACCT))]
    Attr::ro(c"congested",              |ca, out| ca.congested_to_text(out)),

    // debug:
    Attr::ro(c"alloc_debug",            |ca, out| ca.alloc_debug_to_text(out)),
    Attr::ro(c"open_buckets",           |ca, out| ca.fs().open_buckets_to_text(out, Some(ca))),
    Attr::ro(c"discards",               |ca, out| ca.fast_discards_to_text(out)),

];

/// Sectors read and written, by data type, as JSON.
#[cold]
fn io_done_to_text(ca: &Dev, out: &mut Printbuf) {
    let done = ca.io_done.sum();
    let nr = c::DataType::NR as usize;

    writeln!(out, "{{");
    for (rw, name) in ["read", "write"].into_iter().enumerate() {
        writeln!(out, "  \"{name}\": {{");
        for i in 1..nr {
            write!(out, "    \"");
            DataType(i as u32).to_text(out);
            writeln!(out, "\": {}{}", done.sectors[rw][i] << 9, if i < nr - 1 { "," } else { "" });
        }
        writeln!(out, "  }}{}", if rw == READ { "," } else { "" });
    }
    writeln!(out, "}}");
}

pub struct DevDir;

unsafe impl Dir for DevDir {
    type Obj = Dev;
    type Kobj = Placed<Kobject, crate::at!(Dev.kobj)>;

    fn show(ca: &Dev, attr: &c::attribute, out: &mut Printbuf) -> Result<(), BchError> {
        match opt_from_attr(attr) {
            Some(id) => opt_show(ca.fs(), Some(ca), id, out),
            // SAFETY: a device's files that aren't options' are DEV's
            None     => unsafe { sysfs::attr_show(ca, attr, out) },
        }
    }

    fn store(ca: &Dev, attr: &c::attribute, s: &str) -> Result<(), BchError> {
        match opt_from_attr(attr) {
            Some(id) => opt_store(ca.fs(), Some(ca), id, s),
            // SAFETY: as show()'s
            None     => unsafe { sysfs::attr_store(ca, attr, s) },
        }
    }
}

static DEV_FILES: Files<{ DEV.len() + nr_true(&DEV_OPTS) + 1 }> = Files::with_list(DEV, &OPT_FILES, &DEV_OPTS);

static DEV_GROUP: Group = Group::new(None, &DEV_FILES);

pub(crate) static DEV_GROUPS: Groups<2> = Groups::new([&DEV_GROUP]);

// ── helpers ─────────────────────────────────────────────────────────────────

fn started(c: &Fs) -> Result<(), BchError> {
    if c.flags.get(c::FsFlag::started) {
        Ok(())
    } else {
        Err(c::EPERM.into())
    }
}

fn write_ref(c: &Fs) -> Result<WriteRef, BchError> {
    c.writes.tryget_live()
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

    for t in (1..c::CompressionType::NR).map(CompressionType) {
        let v = accounting::read(c, accounting::Compression { type_: t.into() });

        prt!(out, "{}\t{}\r{}\r{}\r\n", t,
             HumanReadable(v.compressed << 9),
             HumanReadable(v.uncompressed << 9),
             HumanReadable((v.uncompressed << 9).checked_div(v.nr_extents).unwrap_or(0)));
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
