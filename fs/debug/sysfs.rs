// SPDX-License-Identifier: GPL-2.0

//! /sys/fs/bcachefs/<uuid>/: the filesystem's sysfs files.
//!
//! Each directory is a Dir (util/sysfs.rs), exported to C as the foo_files[]
//! and foo_sysfs_ops that init's KTYPE()s name - the kobjects and their
//! lifetimes are init's. Its files are one of:
//! - a table of Attrs - name, mode, what reading and writing do: the fs dir,
//!   internal/, a device's own files;
//! - a ListAttrs, a file per entry of an x-macro list: time_stats/,
//!   time_stats_json/, counters/;
//! - the options' own attributes, added at runtime by
//!   bch2_opts_create_sysfs_files(): options/, and a device's options in its
//!   dir. opts.c is still C: opt_show() and opt_store() are over opts.rs's
//!   wrappers.
//!
//! internal/btree_trans_stats_json is a bin_attribute, apart from the rest.
//!
//! The fs dir and internal/ are one filesystem's two views, debugging under
//! internal/. A store to either is refused until the filesystem has started
//! (EPERM). Triggers whose work needs the filesystem writable hold a write ref
//! (BCH_WRITE_REF_sysfs) across it - writable(), EROFS without one; the rest -
//! wakeups, cache shrinks, the logged-op knob, work that takes its own ref -
//! need none.

use core::ffi::{c_int, c_uint, CStr};
use core::mem::offset_of;
use core::sync::atomic::{AtomicPtr, Ordering};

use cstruct_macros::c_export;

use crate::accounting;
use crate::c;
use crate::errcode::{bch_errcode, BchError};
use crate::fs::{Fs, WriteRef};
use crate::init::passes;
use crate::locking::{Lock, MemallocFlags};
use crate::opts::{self, prt_compression_type, prt_data_type, OptChange};
use crate::sb::io::COUNTERS;
use crate::sb::members;
use crate::util::ffi::{c_char, with_cstr};
use crate::util::kernel::{READ, WRITE};
use crate::util::log::LogMsg;
use crate::util::sysfs::{self, parse_human_u64, parse_int, Attr, Dir, Files, ListAttrs};
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
            Err(e) if e.matches(bch_errcode::BCH_ERR_cancelled) => Ok(()),
            ret => ret,
        }
    })),
    Attr::wo(c"trigger_emergency_read_only", |c, _| writable(c, |c| {
        let mut msg = LogMsg::new(c);
        writeln!(msg, "shutdown by sysfs");
        c.emergency_read_only(&mut msg);
        Ok(())
    })),

    Attr::ro(c"gc_gens_pos",            |c, out| crate::prt!(out, "{}\n", c.gc_gens.pos)),
    Attr::ro(c"copy_gc_wait",           |c, out| c.copygc_wait_to_text(out)),
    Attr::ro(c"moving_ctxts",           |c, out| c.moving_ctxts_to_text(out)),
    Attr::ro(c"recent_counters",        |c, out| c.counters.recent_to_text(out)),
    Attr::ro(c"internal_uuid",          |c, out| { out.uuid(&c.sb.uuid()); out.newline() }),
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

// internal/btree_trans_stats_json, a bin_attribute: kernfs reads one a page a
// call, so the JSON is made only at offset 0 - kept in
// c->btree.trans.stats_json_buf - and the pages read are of one whole.

/// Each btree transaction's name - bch2_btree_transaction_fns[], up to the
/// first not yet registered - and its stats.
fn trans_stats(c: &Fs) -> impl Iterator<Item = (&[u8], &c::btree_transaction_stats)> {
    // SAFETY: a slot is written - set once, by bch2_trans_get_fn_idx() - as
    // we read: a word, read as one
    let fns = unsafe {
        &*(&raw const c::bch2_btree_transaction_fns)
            .cast::<[AtomicPtr<c_char>; c::BCH_TRANSACTIONS_NR as usize]>()
    };
    fns.iter()
        .map(|f| f.load(Ordering::Relaxed))
        .take_while(|f| !f.is_null())
        // SAFETY: a registered name is a static string
        .map(|f| unsafe { CStr::from_ptr(f.cast()) }.to_bytes())
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

        write!(out, "\"max_mem\":{},", s.max_mem);

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
    // SAFETY: internal/'s kobject, which has this file
    let c = unsafe { sysfs::obj_of::<InternalDir>(kobj) };

    let _lock = Lock::from_c(&c.btree.trans.stats_json_lock).lock();
    // SAFETY: the buffer is the lock's
    let out = unsafe { Printbuf::borrow_raw(&raw mut (*c.as_ptr()).btree.trans.stats_json_buf) };

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
    // SAFETY: internal/'s kobject, which has this file
    let c = unsafe { sysfs::obj_of::<InternalDir>(kobj) };

    for (_, s) in trans_stats(c) {
        let _lock = Lock::from_c(&s.lock).lock();
        s.duration.reset();
        s.lock_hold_times.reset();
        s.lock_wait_times.reset();

        // SAFETY: these are the stats' lock's
        unsafe {
            let s = core::ptr::from_ref(s).cast_mut();
            (*s).nr_max_paths = 0;
            (*s).max_mem = 0;
            c::rust_kfree((*s).max_paths_text.cast());
            (*s).max_paths_text = core::ptr::null_mut();
        }
    }
    count as isize
}

#[c_export]
pub static bin_attr_btree_trans_stats_json: c::bin_attribute = {
    // SAFETY: the rest of a bin_attribute is NULL or zero
    let mut a: c::bin_attribute = unsafe { core::mem::zeroed() };
    a.attr.name = c"btree_trans_stats_json".as_ptr().cast();
    a.attr.mode = 0o644;
    a.read = Some(trans_stats_json_read);
    a.write = Some(trans_stats_json_write);
    a
};

// ── time_stats/, time_stats_json/ ───────────────────────────────────────────
//
// A file per BCH_TIME_STATS() entry, the same attributes in both; writing
// one resets the stat.

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
        const TIME_STAT_NAMES: [&CStr; c::BCH_TIME_STAT_NR as usize] = [$(file_name!($name),)*];
    };
}
c::BCH_TIME_STATS!(time_stat_names);

static TIME_STATS: ListAttrs<{ c::BCH_TIME_STAT_NR as usize }> = ListAttrs::new(&TIME_STAT_NAMES, 0o644);

fn time_stat<'a>(c: &'a Fs, attr: &c::attribute) -> Result<&'a c::bch2_time_stats, BchError> {
    TIME_STATS.index(attr)
        .map(|i| &c.times[i])
        .ok_or(c::EINVAL.into())
}

/// time_stats/, or as JSON time_stats_json/.
struct TimeStatsDir<const JSON: bool>;

unsafe impl<const JSON: bool> Dir for TimeStatsDir<JSON> {
    type Obj = Fs;
    const KOBJ: usize = if JSON { offset_of!(Fs, time_stats_json) } else { offset_of!(Fs, time_stats) };

    fn show(c: &Fs, attr: &c::attribute, out: &mut Printbuf) -> Result<(), BchError> {
        let s = time_stat(c, attr)?;
        if JSON {
            s.json_to_text(out)
        } else {
            s.to_text(out)
        }
        Ok(())
    }

    fn store(c: &Fs, attr: &c::attribute, _: &str) -> Result<(), BchError> {
        time_stat(c, attr)?.reset();
        Ok(())
    }
}

#[c_export]
#[c("struct attribute *bch2_fs_time_stats_files[]")]
pub static bch2_fs_time_stats_files: Files<{ c::BCH_TIME_STAT_NR as usize + 1 }> = Files::list(&TIME_STATS);

#[c_export]
pub static bch2_fs_time_stats_sysfs_ops: c::sysfs_ops = sysfs::ops::<TimeStatsDir<false>>();

#[c_export]
#[c("struct attribute *bch2_fs_time_stats_json_files[]")]
pub static bch2_fs_time_stats_json_files: Files<{ c::BCH_TIME_STAT_NR as usize + 1 }> = Files::list(&TIME_STATS);

#[c_export]
pub static bch2_fs_time_stats_json_sysfs_ops: c::sysfs_ops = sysfs::ops::<TimeStatsDir<true>>();

// ── counters/ ───────────────────────────────────────────────────────────────
//
// A file per BCH_PERSISTENT_COUNTERS() entry: the count since mount and
// since the filesystem was made; writing one resets it.

macro_rules! counter_names {
    ($(($name:ident $(, $($rest:tt)*)?)),* $(,)?) => {
        const COUNTER_NAMES: [&CStr; c::BCH_COUNTER_NR as usize] = [$(file_name!($name),)*];
    };
}
c::BCH_PERSISTENT_COUNTERS!(counter_names);

static COUNTER_FILES: ListAttrs<{ c::BCH_COUNTER_NR as usize }> = ListAttrs::new(&COUNTER_NAMES, 0o644);

fn counter_idx(attr: &c::attribute) -> Result<usize, BchError> {
    COUNTER_FILES.index(attr).ok_or(c::EINVAL.into())
}

struct CountersDir;

unsafe impl Dir for CountersDir {
    type Obj = Fs;
    const KOBJ: usize = offset_of!(Fs, counters_kobj);

    fn show(c: &Fs, attr: &c::attribute, out: &mut Printbuf) -> Result<(), BchError> {
        let i = counter_idx(attr)?;
        let sectors = COUNTERS[i].is_sectors;

        let mut count = c.counter_read(i);
        let mut since_mount = count.wrapping_sub(c.counters.mount[i]);
        if sectors {
            count <<= 9;
            since_mount <<= 9;
        }

        let prt = |out: &mut Printbuf, v: u64| if sectors {
            out.human_readable_u64(v)
        } else {
            write!(out, "{v}")
        };

        out.tabstop_push(32);
        write!(out, "since mount:\t");
        prt(out, since_mount);
        out.newline();

        write!(out, "since filesystem creation:\t");
        prt(out, count);
        out.newline();
        Ok(())
    }

    fn store(c: &Fs, attr: &c::attribute, _: &str) -> Result<(), BchError> {
        c.counter_reset(counter_idx(attr)?);
        Ok(())
    }
}

#[c_export]
#[c("struct attribute *bch2_fs_counters_files[]")]
pub static bch2_fs_counters_files: Files<{ c::BCH_COUNTER_NR as usize + 1 }> = Files::list(&COUNTER_FILES);

#[c_export]
pub static bch2_fs_counters_sysfs_ops: c::sysfs_ops = sysfs::ops::<CountersDir>();

// ── options/ ────────────────────────────────────────────────────────────────
//
// An option's file is its bch_option's attribute, added at runtime -
// bch2_opts_create_sysfs_files() - to options/ for the filesystem options,
// and to a device's dir for the device options, among its own files.

fn opt_show(c: &Fs, ca: Option<&c::bch_dev>, id: c::bch_opt_id, out: &mut Printbuf)
    -> Result<(), BchError>
{
    let opt = &opts::opt_table()[id.0 as usize];

    let sb = c.sb_lock();

    let v = match ca {
        Some(ca) if opt.type_ == c::opt_type::BCH_OPT_STR_MEMBER => {
            // The value lives in the member, not a u64 - render it here:
            let Some(m) = members::sb_member(sb.sb(), ca.dev_idx as u32) else {
                return c.throw(bch_errcode::BCH_ERR_ENOENT_dev_idx_not_found);
            };
            out.write_bytes(opt.member_str(&m));
            out.write_bytes(b"\n");
            return Ok(());
        }
        Some(ca) => {
            if !(opt.has(c::opt_flags::OPT_DEVICE) && opt.get_member.is_some()) {
                return c.throw(bch_errcode::BCH_ERR_EINVAL_sysfs_opt_not_found);
            }
            opts::opt_from_sb(sb.sb(), id, ca.dev_idx as u32)
        }
        None => {
            if !opt.has(c::opt_flags::OPT_FS) {
                return c.throw(bch_errcode::BCH_ERR_EINVAL_sysfs_opt_not_found);
            }
            opts::opt_get_by_id(&c.opts, id)
        }
    };

    opts::opt_to_text(out, c, opt, v, c::OPT_SHOW_FULL_LIST);
    out.write_bytes(b"\n");
    Ok(())
}

fn opt_store(c: &Fs, ca: Option<&c::bch_dev>, id: c::bch_opt_id, s: &str) -> Result<(), BchError> {
    let opt = &opts::opt_table()[id.0 as usize];
    // strim()
    let val = s.trim_matches(|ch| matches!(ch, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r'));

    with_cstr(val, |val| {
        // We don't need c->writes for correctness, but it eliminates an
        // unsightly error message in the dmesg log when we're RO:
        let _ref = write_ref(c)?;
        let _noio = MemallocFlags::noio();
        let mut change = OptChange::new(c);

        let v = opts::opt_parse(Some(c), opt, val, None).map_err(|e| BchError::from_raw(-e))?;
        change.pre_set(ca, 0, id, v)?;

        let changed = if opt.in_sb() {
            change.set_sb(ca, opt, v, val)
        } else if ca.is_none() {
            opts::opt_get_by_id(&c.opts, id) != v
        } else {
            panic!("device option {:?} isn't a superblock option: not supported", opt.name())
        };

        if changed {
            if ca.is_none() {
                change.set_fs_opt(id, v);
            }
            change.post_set(ca, 0, id, v);
        }
        Ok(())
    })?
}

/// The option @attr is the file of: only options' files are added where
/// this is asked.
fn attr_opt(attr: &c::attribute) -> Result<c::bch_opt_id, BchError> {
    opts::opt_from_attr(attr).ok_or(c::EINVAL.into())
}

struct OptsDir;

unsafe impl Dir for OptsDir {
    type Obj = Fs;
    const KOBJ: usize = offset_of!(Fs, opts_dir);

    fn show(c: &Fs, attr: &c::attribute, out: &mut Printbuf) -> Result<(), BchError> {
        opt_show(c, None, attr_opt(attr)?, out)
    }

    fn store(c: &Fs, attr: &c::attribute, s: &str) -> Result<(), BchError> {
        opt_store(c, None, attr_opt(attr)?, s)
    }
}

#[c_export]
#[c("struct attribute *bch2_fs_opts_dir_files[]")]
pub static bch2_fs_opts_dir_files: Files<1> = Files::new::<Fs>(&[]);

#[c_export]
pub static bch2_fs_opts_dir_sysfs_ops: c::sysfs_ops = sysfs::ops::<OptsDir>();

/// Add a file for each option of @ty - OPT_FS, OPT_DEVICE - to @kobj: the
/// filesystem's options/, or a device's dir.
///
/// # Safety
/// @kobj is a live, added kobject, whose sysfs_ops handle options' files.
#[c_export]
pub unsafe extern "C" fn bch2_opts_create_sysfs_files(kobj: *mut c::kobject, ty: c_uint) -> c_int {
    let fs_opts = ty == c::opt_flags::OPT_FS.bits();

    for opt in opts::opt_table() {
        if opt.has(c::opt_flags::OPT_HIDDEN) || opt.flags.bits() & ty == 0 {
            continue;
        }

        /*
         * For options that are both OPT_FS and OPT_DEVICE (currently only
         * @discard), expose only the per-device sysfs entry: the FS-level
         * entry would alias to c->opts but member-backed options have no
         * FS-wide superblock field, so writes via the FS path are silent
         * no-ops and reads can't disambiguate which scope is being shown.
         * Mount-time @-o discard= remains the only fs-scope handle.
         */
        if fs_opts && opt.has(c::opt_flags::OPT_DEVICE) {
            continue;
        }

        // SAFETY: the caller's; an option's attribute is static
        if let Err(e) = unsafe { sysfs::create_file(kobj, &opt.attr) } {
            return -e.raw();
        }
    }
    0
}

// ── a device's dir: /sys/fs/bcachefs/<uuid>/dev-N/ ───────────────────────────
//
// Its own files, and the device options' - added at runtime, as options/'s.

const DEV: &[Attr<c::bch_dev>] = &[
    Attr::ro(c"uuid",                   |ca, out| { out.uuid(&ca.uuid); out.newline() }),
    Attr::ro(c"first_bucket",           |ca, out| writeln!(out, "{}", ca.mi.first_bucket)),
    Attr::ro(c"nbuckets",               |ca, out| writeln!(out, "{}", ca.mi.nbuckets)),

    Attr::ro(c"has_data",               |ca, out| {
        // SAFETY: the data types' names, NULL terminated
        unsafe { out.prt_bitflags(c::__bch2_data_types.as_ptr(), ca.has_data() as u64) };
        out.newline();
    }),
    Attr::ro(c"io_done",                io_done_to_text),
    Attr::ro(c"io_errors",              |ca, out| ca.io_errors_to_text(out)),
    Attr::wo(c"io_errors_reset",        |ca, _| Ok(ca.errors_reset())),

    Attr::ro(c"io_latency_read",        |ca, out| writeln!(out, "{}", ca.cur_latency(READ))),
    Attr::ro(c"io_latency_write",       |ca, out| writeln!(out, "{}", ca.cur_latency(WRITE))),
    Attr::ro(c"io_latency_stats_read",  |ca, out| ca.io_latency[READ].stats.to_text(out)),
    Attr::ro(c"io_latency_stats_write", |ca, out| ca.io_latency[WRITE].stats.to_text(out)),
    Attr::ro(c"io_latency_stats_read_json",  |ca, out| ca.io_latency[READ].stats.json_to_text(out)),
    Attr::ro(c"io_latency_stats_write_json", |ca, out| ca.io_latency[WRITE].stats.json_to_text(out)),
    #[cfg(not(CONFIG_BCACHEFS_NO_LATENCY_ACCT))]
    Attr::ro(c"congested",              |ca, out| ca.congested_to_text(out)),

    // debug:
    Attr::ro(c"alloc_debug",            |ca, out| ca.alloc_debug_to_text(out)),
    Attr::ro(c"open_buckets",           |ca, out| ca.fs().open_buckets_to_text(out, Some(ca))),
    Attr::ro(c"discards",               |ca, out| ca.fast_discards_to_text(out)),

    Attr::ro(c"read_refs",              |ca, out| ca.io_refs_to_text(out, READ)),
    Attr::ro(c"write_refs",             |ca, out| ca.io_refs_to_text(out, WRITE)),
];

/// Sectors read and written, by data type, as JSON.
#[cold]
fn io_done_to_text(ca: &c::bch_dev, out: &mut Printbuf) {
    let done = ca.io_done.sum();
    let nr = c::BCH_DATA_NR as usize;

    writeln!(out, "{{");
    for (rw, name) in ["read", "write"].into_iter().enumerate() {
        writeln!(out, "  \"{name}\": {{");
        for i in 1..nr {
            write!(out, "    \"");
            prt_data_type(out, c::bch_data_type(i as u32));
            writeln!(out, "\": {}{}", done.sectors[rw][i] << 9, if i < nr - 1 { "," } else { "" });
        }
        writeln!(out, "  }}{}", if rw == READ { "," } else { "" });
    }
    writeln!(out, "}}");
}

struct DevDir;

unsafe impl Dir for DevDir {
    type Obj = c::bch_dev;
    const KOBJ: usize = offset_of!(c::bch_dev, kobj);

    fn show(ca: &c::bch_dev, attr: &c::attribute, out: &mut Printbuf) -> Result<(), BchError> {
        match opts::opt_from_attr(attr) {
            Some(id) => opt_show(ca.fs(), Some(ca), id, out),
            // SAFETY: a device's files that aren't options' are DEV's
            None     => unsafe { sysfs::attr_show(ca, attr, out) },
        }
    }

    fn store(ca: &c::bch_dev, attr: &c::attribute, s: &str) -> Result<(), BchError> {
        match opts::opt_from_attr(attr) {
            Some(id) => opt_store(ca.fs(), Some(ca), id, s),
            // SAFETY: as show()'s
            None     => unsafe { sysfs::attr_store(ca, attr, s) },
        }
    }
}

#[c_export]
#[c("struct attribute *bch2_dev_files[]")]
pub static bch2_dev_files: Files<{ DEV.len() + 1 }> = Files::new(DEV);

#[c_export]
pub static bch2_dev_sysfs_ops: c::sysfs_ops = sysfs::ops::<DevDir>();

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
