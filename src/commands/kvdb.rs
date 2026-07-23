//! kvdb: btree read/write REPL — inspect and modify btree keys by field name.
//!
//! The runtime type information from fs/typeinfo.rs (generated per-type field
//! tables: name, offset, width, endianness) is what makes `update`/`set`
//! possible: fields are addressed by path ("parent", "children[1]",
//! "btime.hi") and written into the raw value bytes, then committed through
//! the normal transactional update path — so journalling, triggers and
//! validation all apply. Primary uses: field-level surgery when debugging in
//! the field, and corruption injection for fsck/repair tests.
//!
//! Ops: get / peek / peek_prev / list (read), update (read-modify-write
//! fields of an existing key), set (construct and insert a whole key —
//! `set <btree> <pos> deleted` deletes). One-shot via -c, REPL on stdin
//! otherwise; the REPL reads commands line by line, so tests can pipe a
//! script in.
//!
//! Scope notes: values are addressed raw (BTREE_ITER_all_snapshots — the
//! position's snapshot field is taken literally, no visibility filtering;
//! harmlessly dropped on btrees without a snapshot field). Fixed-layout vals
//! are fully editable; varint-packed (inode) and entry-stream (extent) vals
//! only up to their fixed header. Updates run the normal triggers and key
//! validation — per-key-invalid keys are (correctly) rejected; whether we
//! want a validation-bypass mode for injecting those is an open question.

use std::io::{stdin, stdout, IsTerminal};
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{anyhow, bail, Result};
use bcachefs_kernel::btree::bkey::{BkeySC, POS_MIN, SPOS_MAX};
use bcachefs_kernel::btree::iter::{
    commit_do, lockrestart_do, BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags, CommitOpts,
    TransError, UpdateTriggerFlags,
};
use bcachefs_kernel::c;
use bcachefs_kernel::errcode::{bch_errcode, BchError};
use bcachefs_kernel::fs::Fs;
use bcachefs_kernel::opt_set;
use bcachefs_kernel::typeinfo;
use bch_bindgen::c::bch_degraded_actions;
use clap::Parser;

use crate::device_scan::OpenedFs;
use crate::logging;
use crate::wrappers::handle::BcachefsHandle;
use crate::wrappers::online_iter::{OnlineBtreeIter, OnlineIterFlags};

const BKEY_U64S: usize = size_of::<c::bkey>() / size_of::<u64>();

/// Btree read/write REPL (debug)
#[derive(Parser, Debug)]
#[command(long_about = "\
Read and write btree keys by field name, using generated runtime type \
information for the on-disk structs. Without -c, reads commands from stdin \
(a REPL; pipe a script in for non-interactive use).\n\n\
Commands:\n\
  get       <btree> <pos>                    exact lookup, dump fields\n\
  peek      <btree> <pos>                    first key >= pos\n\
  peek_prev <btree> <pos>                    last key <= pos\n\
  list      <btree> [start] [end]            keys in range\n\
  update    <btree> <pos> <field=val>...     modify fields of an existing key\n\
  set       <btree> <pos> <type> [field=val]...  insert a whole new key\n\n\
pos is inode:offset[:snapshot], or POS_MIN/POS_MAX/SPOS_MAX. Fields are \
val struct fields: parent, children[1], btime.hi, ... Declared flag bits \
(LE*_BITMASK) resolve by name too: no_keys=1, or qualified as flags.subvol=0 \
when the name collides with a field. get decodes them: flags: 10 (subvol|no_keys). \
Values are decimal, 0x hex, or negative decimal. `set <btree> <pos> deleted` \
deletes a key.\n\n\
Updates go through the normal transactional path: journalled, triggers run, \
key validation applies. This tool can corrupt a filesystem in precise, \
surgical ways - that is its purpose. Use accordingly.")]
pub struct Cli {
    /// Command to run (repeatable; skips the REPL)
    #[arg(short = 'c', long = "command")]
    commands: Vec<String>,

    /// Force color on/off. Default: autodetect tty
    #[arg(long, action = clap::ArgAction::Set, default_value_t=stdout().is_terminal())]
    colorize: bool,

    /// Verbose mode
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Open without starting the filesystem (no recovery, no journal): only
    /// `sb get`/`sb set` work, the btree is inaccessible. For superblock edits
    /// on an image that can't or shouldn't be started.
    #[arg(long)]
    nostart: bool,

    #[arg(required(true))]
    devices: Vec<PathBuf>,
}

// ---------------------------------------------------------------------------
// parsing helpers

fn parse_btree(s: &str) -> Result<c::btree_id> {
    s.parse()
        .map_err(|_| anyhow!("invalid btree '{s}' (try: snapshots, subvolumes, extents, ...)"))
}

/// Position parse honoring the session snapshot context: a pos written
/// without the :snapshot component picks it up from the context. Explicit
/// :snapshot and the named positions (POS_MIN etc.) are left alone.
fn parse_pos_ctx(s: &str, snapshot: Option<u32>) -> Result<c::bpos> {
    let mut pos = parse_pos(s)?;
    if let Some(snap) = snapshot {
        if s.matches(':').count() == 1 {
            pos.snapshot = snap;
        }
    }
    Ok(pos)
}

fn parse_pos(s: &str) -> Result<c::bpos> {
    s.parse()
        .map_err(|_| anyhow!("invalid pos '{s}' (inode:offset[:snapshot], POS_MIN, SPOS_MAX)"))
}

fn parse_int(s: &str) -> Result<u64> {
    if let Some(h) = s.strip_prefix("0x") {
        Ok(u64::from_str_radix(h, 16)?)
    } else if s.starts_with('-') {
        Ok(s.parse::<i64>()? as u64)
    } else {
        Ok(s.parse::<u64>()?)
    }
}

/// An assignment value: numbers resolve immediately, anything else is an
/// enum value name, resolved once the field (and so the key type) is known.
enum FieldVal {
    Int(u64),
    Name(String),
}

fn parse_assign(s: &str) -> Result<(&str, FieldVal)> {
    let (path, val) = s
        .split_once('=')
        .ok_or_else(|| anyhow!("expected field=value, got '{s}'"))?;
    Ok((path, match parse_int(val) {
        Ok(v) => FieldVal::Int(v),
        Err(_) => FieldVal::Name(val.to_string()),
    }))
}

/// The one piece of schema kvdb owns: which fields hold enum codewords. The
/// name<->value tables come from the x-macro imports (fs/codegen.rs); this
/// goes away when the format has a real schema:
fn field_enum(type_: u8, path: &str) -> Option<&'static [(&'static str, u64)]> {
    use bcachefs_kernel::snapshot_states::*;

    match (type_ as u32, path) {
        (t, "state") if t == c::bch_bkey_type::KEY_TYPE_snapshot.0 =>
            Some(SNAPSHOT_STATE_VALUES),
        (t, "state") if t == c::bch_bkey_type::KEY_TYPE_subvolume.0 =>
            Some(SUBVOLUME_STATE_VALUES),
        _ => None,
    }
}

fn field_val(type_: u8, path: &str, v: &FieldVal) -> Result<u64> {
    match v {
        FieldVal::Int(v) => Ok(*v),
        FieldVal::Name(n) => {
            let vals = field_enum(type_, path)
                .ok_or_else(|| anyhow!("{path}: expected integer, got '{n}'"))?;
            vals.iter()
                .find(|(name, _)| name == n)
                .map(|(_, v)| *v)
                .ok_or_else(|| anyhow!("{path}: unknown value '{n}' (valid: {})",
                                       vals.iter().map(|(n, _)| *n)
                                           .collect::<Vec<_>>().join(", ")))
        }
    }
}

// ---------------------------------------------------------------------------
// value access

fn val_bytes<'a>(k: &BkeySC<'a>) -> &'a [u8] {
    let len = k.k.u64s as usize * 8 - size_of::<c::bkey>();
    unsafe { std::slice::from_raw_parts(k.v as *const c::bch_val as *const u8, len) }
}

fn render_key(fs: &Fs, k: &BkeySC<'_>, fields: bool) -> String {
    let mut out = format!("{}\n", k.to_text(fs));
    if !fields {
        return out;
    }
    let Some(info) = typeinfo::bkey_val_info(k.k.type_ as u32) else {
        return out;
    };
    if info.fields.is_empty() {
        return out;
    }
    let mut dump = String::new();
    let _ = typeinfo::struct_to_text(&mut dump, info, val_bytes(k));
    for l in dump.lines() {
        out.push_str("  ");
        out.push_str(l);
        out.push('\n');
    }
    out
}

/// A resolved assignment target: a field, or a declared bit range within one
/// (`no_keys`, `flags.subvol`).
type FieldTarget = (typeinfo::FieldRef, Option<&'static typeinfo::BitmaskField>);

/// Resolve a field-or-bit path against a key type's val struct.
fn resolve_field(type_: u8, path: &str) -> Result<FieldTarget> {
    let info = typeinfo::bkey_val_info(type_ as u32)
        .ok_or_else(|| anyhow!("unknown key type {type_}"))?;
    typeinfo::resolve_with_bits(info, path).map_err(|e| anyhow!("{e}"))
}

fn write_field(
    val: &mut [u8],
    (r, bm): &FieldTarget,
    v: u64,
) -> std::result::Result<(), typeinfo::AccessError> {
    match bm {
        Some(bm) => typeinfo::write_bits(val, r, bm, v),
        None => typeinfo::write_scalar(val, r, v),
    }
}

// ---------------------------------------------------------------------------
// ops

/// Raw exact addressing for get/update: the snapshot field of pos is taken
/// literally. all_snapshots is dropped by the C flag fixup on btrees without
/// a snapshot field, so passing it unconditionally is fine.
const RAW_EXACT: BtreeIterFlags = BtreeIterFlags::SLOTS.union(BtreeIterFlags::ALL_SNAPSHOTS);

/// With a snapshot context active, reads drop ALL_SNAPSHOTS: the iterator
/// runs snapshot-filtered (iter init adds BTREE_ITER_filter_snapshots for
/// btrees with snapshot fields) at pos.snapshot, so lookups resolve
/// visibility the way runtime lookups do. Harmless on btrees without
/// snapshot fields - iteration is simply positional there.
fn iter_flags(base: BtreeIterFlags, filtered: bool) -> BtreeIterFlags {
    if filtered {
        base.difference(BtreeIterFlags::ALL_SNAPSHOTS)
    } else {
        base
    }
}

fn cmd_get(fs: &Fs, btree: c::btree_id, pos: c::bpos, filtered: bool) -> Result<String> {
    let trans = BtreeTrans::new(fs);
    Ok(lockrestart_do(&trans, |t| {
        let mut iter = BtreeIter::new(t.trans(), btree, pos, iter_flags(RAW_EXACT, filtered));
        let out = iter
            .peek_max_flags(SPOS_MAX, BtreeIterFlags::SLOTS)
            .map(|k| match k {
                Some(k) => render_key(fs, &k, true),
                None => "(no key)\n".to_string(),
            });
        t.result_value(out)
    })?)
}

fn cmd_peek(fs: &Fs, btree: c::btree_id, pos: c::bpos, prev: bool, filtered: bool) -> Result<String> {
    let trans = BtreeTrans::new(fs);
    Ok(lockrestart_do(&trans, |t| {
        let mut iter = BtreeIter::new(t.trans(), btree, pos,
                                      iter_flags(BtreeIterFlags::ALL_SNAPSHOTS, filtered));
        let out = if prev {
            iter.peek_prev()
        } else {
            iter.peek()
        }
        .map(|k| match k {
            Some(k) => render_key(fs, &k, true),
            None => "(no key)\n".to_string(),
        });
        t.result_value(out)
    })?)
}

/// Tab completion: command names for the first word, btree names (from the
/// same string table FromStr parses) after a key command, get/set after sb.
struct KvdbHelper {
    btrees: Vec<String>,
}

const OPS: &[&str] = &[
    "get", "peek", "peek_prev", "list", "update", "set", "sb", "snapshot", "help", "quit",
];
const KEY_OPS: &[&str] = &["get", "peek", "peek_prev", "list", "update", "set"];

impl rustyline::completion::Completer for KvdbHelper {
    type Candidate = String;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _: &rustyline::Context<'_>,
    ) -> rustyline::Result<(usize, Vec<String>)> {
        let start = line[..pos].rfind(char::is_whitespace).map_or(0, |i| i + 1);
        let word = &line[start..pos];
        let prior: Vec<&str> = line[..start].split_whitespace().collect();

        let candidates: Vec<String> = match prior.as_slice() {
            [] => OPS.iter().map(|s| s.to_string()).collect(),
            [op] if KEY_OPS.contains(op) => self.btrees.clone(),
            ["sb"] => vec!["get".to_string(), "set".to_string()],
            _ => vec![],
        };
        Ok((
            start,
            candidates
                .into_iter()
                .filter(|c| c.starts_with(word))
                .collect(),
        ))
    }
}

impl rustyline::hint::Hinter for KvdbHelper {
    type Hint = String;
}
impl rustyline::highlight::Highlighter for KvdbHelper {}
impl rustyline::validate::Validator for KvdbHelper {}
impl rustyline::Helper for KvdbHelper {}

/// ^C during a long-running command: the REPL installs a SIGINT handler that
/// sets this flag (rustyline's raw mode swallows ^C at the prompt itself, so
/// the handler only ever fires mid-command). Iteration loops poll it and stop,
/// returning what they've collected; the flag is cleared before each command.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn kvdb_sigint(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::Relaxed);
}

fn take_interrupt() -> bool {
    INTERRUPTED.swap(false, Ordering::Relaxed)
}

fn cmd_list(fs: &Fs, btree: c::btree_id, start: c::bpos, end: c::bpos,
            filtered: bool) -> Result<String> {
    let trans = BtreeTrans::new(fs);
    let mut out = String::new();
    let mut iter = BtreeIter::new(
        &trans,
        btree,
        start,
        iter_flags(BtreeIterFlags::ALL_SNAPSHOTS | BtreeIterFlags::PREFETCH, filtered),
    );
    iter.for_each_max(&trans, end, |k| {
        if take_interrupt() {
            out.push_str("(interrupted)\n");
            return ControlFlow::Break(());
        }
        out.push_str(&format!("{}\n", k.to_text(fs)));
        ControlFlow::Continue(())
    })?;
    Ok(out)
}

/// One key from a mounted filesystem, or None. get: slot iteration at an
/// exact pos; peek/peek_prev: first key at/after (at/before) pos.
fn online_one_key(handle: &BcachefsHandle, fs: &Fs,
		  btree: c::btree_id, pos: c::bpos,
		  flags: OnlineIterFlags, fields: bool) -> Result<String> {
    // Small buffer: the kernel fills the whole thing per call, and we only
    // want one key (it grows automatically if the key doesn't fit):
    let mut iter = OnlineBtreeIter::with_buf_size(handle, btree, 0, pos,
					if flags.0 & OnlineIterFlags::PREV.0 != 0 { POS_MIN } else { SPOS_MAX },
					flags, 4096);
    Ok(match iter.next().map_err(|e| anyhow!("BCH_IOCTL_QUERY_BTREE_KEYS: {e}"))? {
        Some(k) => render_key(fs, &k, fields),
        None => "(no key)\n".to_string(),
    })
}

/// Online counterpart of iter_flags(): the ioctl maps its flags 1:1 into
/// btree iter flags, so omitting all_snapshots gets filtered iteration
/// kernel-side.
fn online_snapshots_flag(filtered: bool) -> OnlineIterFlags {
    if filtered {
        OnlineIterFlags::default()
    } else {
        OnlineIterFlags::ALL_SNAPSHOTS
    }
}

fn cmd_get_online(handle: &BcachefsHandle, fs: &Fs,
		  btree: c::btree_id, pos: c::bpos, filtered: bool) -> Result<String> {
    online_one_key(handle, fs, btree, pos,
		   OnlineIterFlags::SLOTS | online_snapshots_flag(filtered), true)
}

fn cmd_peek_online(handle: &BcachefsHandle, fs: &Fs,
		   btree: c::btree_id, pos: c::bpos, prev: bool, filtered: bool) -> Result<String> {
    let mut flags = online_snapshots_flag(filtered);
    if prev {
        flags = flags | OnlineIterFlags::PREV;
    }
    online_one_key(handle, fs, btree, pos, flags, true)
}

fn cmd_list_online(handle: &BcachefsHandle, fs: &Fs,
		   btree: c::btree_id, start: c::bpos, end: c::bpos,
		   filtered: bool) -> Result<String> {
    let mut out = String::new();
    let mut iter = OnlineBtreeIter::new(handle, btree, 0, start, end,
					online_snapshots_flag(filtered));
    iter.for_each(|k| {
        if take_interrupt() {
            out.push_str("(interrupted)\n");
            return ControlFlow::Break(());
        }
        out.push_str(&format!("{}\n", k.to_text(fs)));
        ControlFlow::Continue(())
    }).map_err(|e| anyhow!("BCH_IOCTL_QUERY_BTREE_KEYS: {e}"))?;
    Ok(out)
}

fn no_key_err() -> TransError {
    TransError::from(BchError::from_errcode(
        bch_errcode::BCH_ERR_ENOENT_bkey_type_mismatch,
    ))
}

fn cmd_update(
    fs: &Fs,
    btree: c::btree_id,
    pos: c::bpos,
    assigns: &[(&str, FieldVal)],
) -> Result<String> {
    let trans = BtreeTrans::new(fs);
    let mut user_err: Option<anyhow::Error> = None;

    let commit = commit_do(
        &trans,
        None,
        CommitOpts::new().flags(CommitFlags::NO_ENOSPC),
        |t| {
            let mut iter = BtreeIter::new(
                t.trans(),
                btree,
                pos,
                RAW_EXACT | BtreeIterFlags::INTENT,
            );
            let k = iter
                .peek_max_flags(SPOS_MAX, BtreeIterFlags::SLOTS)
                .map_err(TransError::from)?;
            let Some(k) = k.filter(|k| !k.is_deleted()) else {
                let (inode, offset, snapshot) = (pos.inode, pos.offset, pos.snapshot);
                user_err = Some(anyhow!("no key at {inode}:{offset}:{snapshot}"));
                return Err(no_key_err());
            };

            // Byte extent the assignments need; the value may legally be
            // shorter than the current struct (older format version) - grow
            // it, zero-filled, if a written field lies beyond the end. The
            // key type is known now, so enum value names resolve here too:
            let mut need = 0usize;
            let mut resolved = Vec::with_capacity(assigns.len());
            for (path, fv) in assigns {
                match resolve_field(k.k.type_, path) {
                    Ok((r, _)) => need = need.max(r.offset + r.len),
                    Err(e) => {
                        user_err = Some(e);
                        return Err(no_key_err());
                    }
                }
                match field_val(k.k.type_, path, fv) {
                    Ok(v) => resolved.push((*path, v)),
                    Err(e) => {
                        user_err = Some(e);
                        return Err(no_key_err());
                    }
                }
            }

            // BkeySC is a split key: k (unpacked header) and v are separate
            // pointers, so copy them separately.
            let cur_val = k.k.u64s as usize - BKEY_U64S;
            let val_u64s = cur_val.max(need.div_ceil(8));
            let mut new = t.bkey_alloc((BKEY_U64S + val_u64s) as u32)
                .map_err(TransError::from)?;
            new.as_mut_u64s().fill(0);
            unsafe {
                core::ptr::copy_nonoverlapping(k.k, new.k_mut(), 1);
                core::ptr::copy_nonoverlapping(
                    k.v as *const c::bch_val as *const u64,
                    new.as_mut_u64s()[BKEY_U64S..].as_mut_ptr(),
                    cur_val,
                );
            }
            new.k_mut().u64s = (BKEY_U64S + val_u64s) as u8;

            let val = &mut new.as_mut_u64s()[BKEY_U64S..];
            let val: &mut [u8] = unsafe {
                std::slice::from_raw_parts_mut(val.as_mut_ptr() as *mut u8, val.len() * 8)
            };
            for (path, v) in &resolved {
                let target = resolve_field(new.k().type_, path).expect("resolved above");
                if let Err(e) = write_field(val, &target, *v) {
                    user_err = Some(anyhow!("{path}: {e}"));
                    return Err(no_key_err());
                }
            }

            t.update(&mut iter, new, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)
        },
    );

    match commit {
        Ok(()) => Ok(String::new()),
        Err(e) => Err(user_err.unwrap_or_else(|| anyhow!("update failed: {e}"))),
    }
}

fn cmd_set(
    fs: &Fs,
    btree: c::btree_id,
    pos: c::bpos,
    type_name: &str,
    assigns: &[(&str, FieldVal)],
    in_snapshot: bool,
) -> Result<String> {
    let ti = typeinfo::bkey_type_info_by_name(type_name)
        .ok_or_else(|| anyhow!("unknown key type '{type_name}'"))?;
    let val_u64s = ti.info.size.div_ceil(8);

    let assigns = assigns
        .iter()
        .map(|(path, fv)| Ok((*path, field_val(ti.type_ as u8, path, fv)?)))
        .collect::<Result<Vec<(&str, u64)>>>()?;

    let trans = BtreeTrans::new(fs);
    let mut user_err: Option<anyhow::Error> = None;

    let commit = commit_do(
        &trans,
        None,
        CommitOpts::new().flags(CommitFlags::NO_ENOSPC),
        |t| {
            // Deletion has two meanings. Raw (the default): remove this
            // exact key - a filtered iter would have bch2_trans_update()
            // convert the deletion into a whiteout whenever the key is
            // visible in an ancestor snapshot, so deleting a whiteout
            // silently rewrites it as itself. -s: delete within pos's
            // snapshot - the filtered iter gets exactly that conversion,
            // the same semantics as a runtime delete. The peek is just
            // the traverse bch2_trans_update() requires.
            let (iter_flags, update_flags) = if in_snapshot {
                (BtreeIterFlags::INTENT, UpdateTriggerFlags::empty())
            } else {
                (RAW_EXACT | BtreeIterFlags::INTENT,
                 UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)
            };
            let mut iter = BtreeIter::new(
                t.trans(),
                btree,
                pos,
                iter_flags,
            );
            iter.peek_max_flags(SPOS_MAX, BtreeIterFlags::SLOTS)
                .map_err(TransError::from)?;

            let mut new = t.bkey_alloc((BKEY_U64S + val_u64s) as u32)
                .map_err(TransError::from)?;
            new.as_mut_u64s().fill(0);
            unsafe { c::bkey_init(new.k_mut()) };
            new.k_mut().u64s = (BKEY_U64S + val_u64s) as u8;
            new.k_mut().type_ = ti.type_ as u8;
            new.k_mut().p = pos;

            let val = &mut new.as_mut_u64s()[BKEY_U64S..];
            let val: &mut [u8] = unsafe {
                std::slice::from_raw_parts_mut(val.as_mut_ptr() as *mut u8, val.len() * 8)
            };
            for (path, v) in &assigns {
                let target = match typeinfo::resolve_with_bits(ti.info, path) {
                    Ok(t) => t,
                    Err(e) => {
                        user_err = Some(anyhow!("{e}"));
                        return Err(no_key_err());
                    }
                };
                if let Err(e) = write_field(val, &target, *v) {
                    user_err = Some(anyhow!("{path}: {e}"));
                    return Err(no_key_err());
                }
            }

            t.update(&mut iter, new, update_flags)
        },
    );

    match commit {
        Ok(()) => Ok(String::new()),
        Err(e) => Err(user_err.unwrap_or_else(|| anyhow!("set failed: {e}"))),
    }
}

// ---------------------------------------------------------------------------
// superblock fields — same field engine as keys, anchored on bch_sb::INFO
// (which carries the BCH_SB_* LE64_BITMASK fields), written back via
// bch2_write_super so the csum is recomputed and every sb copy updated.

fn sb_field(field: &str) -> Result<FieldTarget> {
    typeinfo::resolve_with_bits(<c::bch_sb as typeinfo::TypeInfo>::INFO, field)
        .map_err(|e| anyhow!("{e}"))
}

/// The in-memory superblock as a byte slice over its full vstruct extent.
fn sb_bytes(fs: &Fs) -> (*mut u8, usize) {
    let sb = unsafe { (*fs.raw).disk_sb.sb };
    (sb as *mut u8, crate::wrappers::super_io::vstruct_bytes_sb(unsafe { &*sb }))
}

fn cmd_sb_get(fs: &Fs, field: &str) -> Result<String> {
    let (r, bm) = sb_field(field)?;
    let (p, len) = sb_bytes(fs);
    let buf = unsafe { std::slice::from_raw_parts(p, len) };
    let v = match bm {
        Some(bm) => typeinfo::read_bits(buf, &r, bm),
        None => typeinfo::read_scalar(buf, &r),
    }.map_err(|e| anyhow!("{field}: {e}"))?;
    Ok(format!("{field} = {v} (0x{v:x})\n"))
}

fn cmd_sb_set(fs: &Fs, field: &str, v: u64) -> Result<String> {
    let target = sb_field(field)?;
    let (p, len) = sb_bytes(fs);
    let buf = unsafe { std::slice::from_raw_parts_mut(p, len) };
    write_field(buf, &target, v).map_err(|e| anyhow!("{field}: {e}"))?;

    let ret = unsafe { c::bch2_write_super(fs.raw) };
    if ret != 0 {
        bail!("bch2_write_super failed: {ret}");
    }
    Ok(String::new())
}

// ---------------------------------------------------------------------------
// command dispatch + REPL

const HELP: &str = "\
get       <btree> <pos>                        exact lookup, dump fields
peek      <btree> <pos>                        first key >= pos
peek_prev <btree> <pos>                        last key <= pos
list      <btree> [start] [end]                keys in range
update    <btree> <pos> <field=val>...         modify fields of an existing key
set  [-s] <btree> <pos> <type> [field=val]...  insert a whole new key
          values are integers; fields holding enum codewords (snapshot/
          subvolume state) also accept the value name, e.g. state=will_delete
          `set <pos> deleted` removes the exact key; with -s it deletes
          within pos's snapshot instead (whiteouts, like a runtime delete)
sb get    <field>                              read a superblock field/flag
sb set    <field=val>                          write one, then bch2_write_super
snapshot  [<id>|none]                          set/show/clear the session snapshot
                                               context: reads (get/peek/list) run
                                               snapshot-filtered in that view, and
                                               a <pos> without :snapshot uses it;
                                               writes (update/set) stay exact-key
help                                           this text
quit                                           exit (also ^D)
";

/// A kvdb session: fully offline (read + write via libbcachefs), or against
/// a mounted filesystem (reads via BCH_IOCTL_QUERY_BTREE_KEYS; the Fs is
/// opened noexcl|nostart purely for key formatting - never started, journal
/// never read - and writes are refused).
enum KvdbFs {
    Offline(Fs),
    Online(BcachefsHandle, Fs),
}

impl KvdbFs {
    /// The userspace Fs handle, for reads that don't go through the kernel
    /// (superblock access; the sb is loaded in both the offline and online
    /// cases).
    fn fs(&self) -> &Fs {
        match self {
            KvdbFs::Offline(fs) | KvdbFs::Online(_, fs) => fs,
        }
    }
}

fn run_line(
    kvdb_fs: &KvdbFs,
    nostart: bool,
    snapshot: &mut Option<u32>,
    line: &str,
) -> Result<ControlFlow<()>> {
    let args: Vec<&str> = line.split_whitespace().collect();
    let Some((&op, args)) = args.split_first() else {
        return Ok(ControlFlow::Continue(()));
    };

    if nostart && !matches!(op, "sb" | "help" | "?" | "snapshot" | "quit" | "exit" | "q") {
        bail!("--nostart: the btree isn't started, only sb commands are available");
    }

    let out = match op {
        "quit" | "exit" | "q" => return Ok(ControlFlow::Break(())),
        "snapshot" => match args {
            [] => match snapshot {
                Some(s) => format!("snapshot context: {s}\n"),
                None => "no snapshot context\n".to_string(),
            },
            ["none"] | ["clear"] => {
                *snapshot = None;
                String::new()
            }
            [s] => {
                *snapshot = Some(u32::try_from(parse_int(s)?)?);
                String::new()
            }
            _ => bail!("usage: snapshot [<id>|none]"),
        },
        "get" | "peek" | "peek_prev" => {
            let [btree, pos] = args else {
                bail!("usage: {op} <btree> <pos>");
            };
            let (btree, pos) = (parse_btree(btree)?, parse_pos_ctx(pos, *snapshot)?);
            let filtered = snapshot.is_some();
            match kvdb_fs {
                KvdbFs::Offline(fs) => match op {
                    "get" => cmd_get(fs, btree, pos, filtered)?,
                    "peek" => cmd_peek(fs, btree, pos, false, filtered)?,
                    _ => cmd_peek(fs, btree, pos, true, filtered)?,
                },
                KvdbFs::Online(handle, fs) => match op {
                    "get" => cmd_get_online(handle, fs, btree, pos, filtered)?,
                    "peek" => cmd_peek_online(handle, fs, btree, pos, false, filtered)?,
                    _ => cmd_peek_online(handle, fs, btree, pos, true, filtered)?,
                },
            }
        }
        "list" => {
            let (btree, rest) = args
                .split_first()
                .ok_or_else(|| anyhow!("usage: list <btree> [start] [end]"))?;
            // A filtered iterator's snapshot comes from pos.snapshot, so the
            // default start must carry the context (POS_MIN's snapshot 0 is
            // never a valid view):
            let default_start = match *snapshot {
                Some(snap) => c::bpos { inode: 0, offset: 0, snapshot: snap },
                None => POS_MIN,
            };
            let start = rest.first().map_or(Ok(default_start), |s| parse_pos_ctx(s, *snapshot))?;
            let end = rest.get(1).map_or(Ok(SPOS_MAX), |s| parse_pos_ctx(s, *snapshot))?;
            let filtered = snapshot.is_some();
            let btree = parse_btree(btree)?;
            match kvdb_fs {
                KvdbFs::Offline(fs) => cmd_list(fs, btree, start, end, filtered)?,
                KvdbFs::Online(handle, fs) => cmd_list_online(handle, fs, btree, start, end, filtered)?,
            }
        }
        "update" => {
            let [btree, pos, assigns @ ..] = args else {
                bail!("usage: update <btree> <pos> <field=val>...");
            };
            if assigns.is_empty() {
                bail!("usage: update <btree> <pos> <field=val>...");
            }
            let KvdbFs::Offline(fs) = kvdb_fs else {
                bail!("filesystem is mounted: kvdb is read-only on mounted filesystems");
            };
            let assigns = assigns
                .iter()
                .map(|s| parse_assign(s))
                .collect::<Result<Vec<_>>>()?;
            cmd_update(fs, parse_btree(btree)?, parse_pos_ctx(pos, *snapshot)?, &assigns)?
        }
        "set" => {
            let (in_snapshot, args) = match args {
                ["-s", rest @ ..] => (true, rest),
                _ => (false, args),
            };
            let [btree, pos, type_name, assigns @ ..] = args else {
                bail!("usage: set [-s] <btree> <pos> <type> [field=val]...");
            };
            if in_snapshot && *type_name != "deleted" {
                bail!("-s (delete within pos's snapshot) only applies to deletions");
            }
            let KvdbFs::Offline(fs) = kvdb_fs else {
                bail!("filesystem is mounted: kvdb is read-only on mounted filesystems");
            };
            let assigns = assigns
                .iter()
                .map(|s| parse_assign(s))
                .collect::<Result<Vec<_>>>()?;
            cmd_set(fs, parse_btree(btree)?, parse_pos_ctx(pos, *snapshot)?, type_name, &assigns, in_snapshot)?
        }
        "sb" => {
            let (&sub, rest) = args.split_first()
                .ok_or_else(|| anyhow!("usage: sb get <field> | sb set <field=val>"))?;
            match sub {
                "get" => {
                    let [field] = rest else { bail!("usage: sb get <field>"); };
                    cmd_sb_get(kvdb_fs.fs(), field)?
                }
                "set" => {
                    let [assign] = rest else { bail!("usage: sb set <field=val>"); };
                    let KvdbFs::Offline(fs) = kvdb_fs else {
                        bail!("filesystem is mounted: kvdb is read-only on mounted filesystems");
                    };
                    let (field, val) = parse_assign(assign)?;
                    let FieldVal::Int(v) = val else {
                        bail!("sb set: expected an integer value");
                    };
                    cmd_sb_set(fs, field, v)?
                }
                _ => bail!("usage: sb get <field> | sb set <field=val>"),
            }
        }
        "help" | "?" => HELP.to_string(),
        _ => bail!("unknown command '{op}' (try: help)"),
    };

    print!("{out}");
    Ok(ControlFlow::Continue(()))
}

fn kvdb(cli: Cli) -> Result<()> {
    logging::setup(cli.verbose, cli.colorize);

    let mut fs_opts = c::bch_opts::default();
    opt_set!(fs_opts, degraded, bch_degraded_actions::BCH_DEGRADED_very as u8);
    // An injection tool must not consume its own injections: background
    // workers in *this* fs instance would otherwise act on the state under
    // test the moment we go read-write. Snapshot deletion reaps a snapshot
    // node a test marked WILL_DELETE (root inode and all) before fsck ever
    // sees the image; copygc evacuates fragmented buckets, rewriting
    // backpointers a test just staged or is about to read; reconcile
    // consumes pending needs_reconcile state.
    opt_set!(fs_opts, auto_snapshot_deletion, 0);
    opt_set!(fs_opts, copygc_enabled, 0);
    opt_set!(fs_opts, reconcile_enabled, 0);
    // ...and the write path must not refuse the states fsck is being tested
    // against: commit-only validation (invalid state codewords, subvolume ->
    // interior node references) stays off in this instance.
    opt_set!(fs_opts, no_commit_validate, 1);
    opt_set!(
        fs_opts,
        errors,
        c::bch_error_actions::BCH_ON_ERROR_continue as u8
    );
    if cli.verbose > 0 {
        opt_set!(fs_opts, verbose, 1);
    }
    // sb-only mode: open the sb but don't run recovery or touch the btree.
    if cli.nostart {
        opt_set!(fs_opts, nostart, 1);
    }

    let kvdb_fs = match crate::device_scan::open_online_or_offline(&cli.devices, fs_opts)? {
        OpenedFs::Offline(fs) => KvdbFs::Offline(fs),
        OpenedFs::Online(handle) => {
            // Reads go through the kernel; the userspace Fs is opened
            // noexcl|nostart purely for key formatting (never started,
            // journal never read), from the member block devices - the
            // path we were given may be a mount point or UUID:
            log::info!("filesystem is mounted: reads via the kernel, writes disabled");

            let devs = handle.member_devices()
                .map_err(|e| anyhow!("getting member devices from sysfs: {e}"))?;

            opt_set!(fs_opts, noexcl, 1);
            opt_set!(fs_opts, nostart, 1);
            opt_set!(fs_opts, read_only, 1);
            let fs = crate::device_scan::open_scan(&devs, fs_opts)
                .map_err(|e| anyhow!(
                    "opening {devs:?} (noexcl/nostart, for formatting keys): {e}"))?;

            KvdbFs::Online(handle, fs)
        }
    };
    let fs = &kvdb_fs;
    let mut snapshot_ctx: Option<u32> = None;

    if !cli.commands.is_empty() {
        for line in &cli.commands {
            if run_line(fs, cli.nostart, &mut snapshot_ctx, line)?.is_break() {
                break;
            }
        }
        return Ok(());
    }

    // In a piped script an error must abort (a test's later commands likely
    // depend on earlier ones); interactively, report and go on.
    if !stdin().is_terminal() {
        for line in stdin().lines() {
            if run_line(fs, cli.nostart, &mut snapshot_ctx, &line?)?.is_break() {
                break;
            }
        }
        return Ok(());
    }

    unsafe {
        libc::signal(libc::SIGINT, kvdb_sigint as *const () as libc::sighandler_t);
    }

    let mut rl: rustyline::Editor<KvdbHelper, rustyline::history::DefaultHistory> =
        rustyline::Editor::new()?;
    rl.set_helper(Some(KvdbHelper {
        btrees: bcachefs_kernel::BTREE_IDS_KNOWN
            .iter()
            .map(|id| id.to_string())
            .collect(),
    }));
    let history = std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".cache/bcachefs-kvdb-history"));
    if let Some(h) = &history {
        let _ = rl.load_history(h);
    }
    loop {
        let prompt = match snapshot_ctx {
            Some(s) => format!("kvdb[{s}]> "),
            None => "kvdb> ".to_string(),
        };
        match rl.readline(&prompt) {
            Ok(line) => {
                if !line.trim().is_empty() {
                    let _ = rl.add_history_entry(&line);
                }
                INTERRUPTED.store(false, Ordering::Relaxed);
                match run_line(fs, cli.nostart, &mut snapshot_ctx, &line) {
                    Ok(ControlFlow::Break(())) => break,
                    Ok(ControlFlow::Continue(())) => {}
                    Err(e) => eprintln!("{e}"),
                }
            }
            Err(rustyline::error::ReadlineError::Interrupted) => continue,
            Err(rustyline::error::ReadlineError::Eof) => break,
            Err(e) => return Err(e.into()),
        }
    }
    if let Some(h) = &history {
        if let Some(dir) = h.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = rl.save_history(h);
    }
    Ok(())
}

pub const CMD: super::CmdDef = typed_cmd!("kvdb", "Btree read/write REPL (debug)", Cli, kvdb);
