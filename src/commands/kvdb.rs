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

use std::io::{stdin, stdout, IsTerminal, Write};
use std::ops::ControlFlow;
use std::path::PathBuf;

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

use crate::logging;

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

    #[arg(required(true))]
    devices: Vec<PathBuf>,
}

// ---------------------------------------------------------------------------
// parsing helpers

fn parse_btree(s: &str) -> Result<c::btree_id> {
    s.parse()
        .map_err(|_| anyhow!("invalid btree '{s}' (try: snapshots, subvolumes, extents, ...)"))
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

fn parse_assign(s: &str) -> Result<(&str, u64)> {
    let (path, val) = s
        .split_once('=')
        .ok_or_else(|| anyhow!("expected field=value, got '{s}'"))?;
    Ok((path, parse_int(val)?))
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

fn cmd_get(fs: &Fs, btree: c::btree_id, pos: c::bpos) -> Result<String> {
    let trans = BtreeTrans::new(fs);
    Ok(lockrestart_do(&trans, |t| {
        let mut iter = BtreeIter::new(t.trans(), btree, pos, RAW_EXACT);
        let out = iter
            .peek_max_flags(SPOS_MAX, BtreeIterFlags::SLOTS)
            .map(|k| match k {
                Some(k) => render_key(fs, &k, true),
                None => "(no key)\n".to_string(),
            });
        t.result_value(out)
    })?)
}

fn cmd_peek(fs: &Fs, btree: c::btree_id, pos: c::bpos, prev: bool) -> Result<String> {
    let trans = BtreeTrans::new(fs);
    Ok(lockrestart_do(&trans, |t| {
        let mut iter = BtreeIter::new(t.trans(), btree, pos, BtreeIterFlags::ALL_SNAPSHOTS);
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

fn cmd_list(fs: &Fs, btree: c::btree_id, start: c::bpos, end: c::bpos) -> Result<String> {
    let trans = BtreeTrans::new(fs);
    let mut out = String::new();
    let mut iter = BtreeIter::new(
        &trans,
        btree,
        start,
        BtreeIterFlags::ALL_SNAPSHOTS | BtreeIterFlags::PREFETCH,
    );
    iter.for_each_max(&trans, end, |k| {
        out.push_str(&format!("{}\n", k.to_text(fs)));
        ControlFlow::Continue(())
    })?;
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
    assigns: &[(&str, u64)],
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
            // it, zero-filled, if a written field lies beyond the end.
            let mut need = 0usize;
            for (path, _) in assigns {
                match resolve_field(k.k.type_, path) {
                    Ok((r, _)) => need = need.max(r.offset + r.len),
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
            for (path, v) in assigns {
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
    assigns: &[(&str, u64)],
) -> Result<String> {
    let ti = typeinfo::bkey_type_info_by_name(type_name)
        .ok_or_else(|| anyhow!("unknown key type '{type_name}'"))?;
    let val_u64s = ti.info.size.div_ceil(8);

    let trans = BtreeTrans::new(fs);
    let mut user_err: Option<anyhow::Error> = None;

    let commit = commit_do(
        &trans,
        None,
        CommitOpts::new().flags(CommitFlags::NO_ENOSPC),
        |t| {
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
            for (path, v) in assigns {
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

            t.insert_nonextent(btree, new, UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE)
        },
    );

    match commit {
        Ok(()) => Ok(String::new()),
        Err(e) => Err(user_err.unwrap_or_else(|| anyhow!("set failed: {e}"))),
    }
}

// ---------------------------------------------------------------------------
// command dispatch + REPL

const HELP: &str = "\
get       <btree> <pos>                        exact lookup, dump fields
peek      <btree> <pos>                        first key >= pos
peek_prev <btree> <pos>                        last key <= pos
list      <btree> [start] [end]                keys in range
update    <btree> <pos> <field=val>...         modify fields of an existing key
set       <btree> <pos> <type> [field=val]...  insert a whole new key
help                                           this text
";

fn run_line(fs: &Fs, line: &str) -> Result<()> {
    let args: Vec<&str> = line.split_whitespace().collect();
    let Some((&op, args)) = args.split_first() else {
        return Ok(());
    };

    let out = match op {
        "get" | "peek" | "peek_prev" => {
            let [btree, pos] = args else {
                bail!("usage: {op} <btree> <pos>");
            };
            let (btree, pos) = (parse_btree(btree)?, parse_pos(pos)?);
            match op {
                "get" => cmd_get(fs, btree, pos)?,
                "peek" => cmd_peek(fs, btree, pos, false)?,
                _ => cmd_peek(fs, btree, pos, true)?,
            }
        }
        "list" => {
            let (btree, rest) = args
                .split_first()
                .ok_or_else(|| anyhow!("usage: list <btree> [start] [end]"))?;
            let start = rest.first().map_or(Ok(POS_MIN), |s| parse_pos(s))?;
            let end = rest.get(1).map_or(Ok(SPOS_MAX), |s| parse_pos(s))?;
            cmd_list(fs, parse_btree(btree)?, start, end)?
        }
        "update" => {
            let [btree, pos, assigns @ ..] = args else {
                bail!("usage: update <btree> <pos> <field=val>...");
            };
            if assigns.is_empty() {
                bail!("usage: update <btree> <pos> <field=val>...");
            }
            let assigns = assigns
                .iter()
                .map(|s| parse_assign(s))
                .collect::<Result<Vec<_>>>()?;
            cmd_update(fs, parse_btree(btree)?, parse_pos(pos)?, &assigns)?
        }
        "set" => {
            let [btree, pos, type_name, assigns @ ..] = args else {
                bail!("usage: set <btree> <pos> <type> [field=val]...");
            };
            let assigns = assigns
                .iter()
                .map(|s| parse_assign(s))
                .collect::<Result<Vec<_>>>()?;
            cmd_set(fs, parse_btree(btree)?, parse_pos(pos)?, type_name, &assigns)?
        }
        "help" | "?" => HELP.to_string(),
        _ => bail!("unknown command '{op}' (try: help)"),
    };

    print!("{out}");
    Ok(())
}

fn kvdb(cli: Cli) -> Result<()> {
    logging::setup(cli.verbose, cli.colorize);

    let mut fs_opts = c::bch_opts::default();
    opt_set!(fs_opts, degraded, bch_degraded_actions::BCH_DEGRADED_very as u8);
    // An injection tool must not consume its own injections: with this on,
    // writing WILL_DELETE to a snapshot node kicks the deletion worker in
    // *this* fs instance, and the state under test is gone (root inode and
    // all) before fsck ever sees the image.
    opt_set!(fs_opts, auto_snapshot_deletion, 0);
    opt_set!(
        fs_opts,
        errors,
        c::bch_error_actions::BCH_ON_ERROR_continue as u8
    );
    if cli.verbose > 0 {
        opt_set!(fs_opts, verbose, 1);
    }

    let fs = crate::device_scan::open_scan(&cli.devices, fs_opts)?;

    if !cli.commands.is_empty() {
        for line in &cli.commands {
            run_line(&fs, line)?;
        }
        return Ok(());
    }

    let interactive = stdin().is_terminal();
    let mut lines = stdin().lines();
    loop {
        if interactive {
            print!("kvdb> ");
            stdout().flush()?;
        }
        let Some(line) = lines.next() else { break };
        let line = line?;
        // In a piped script an error must abort (a test's later commands
        // likely depend on earlier ones); interactively, report and go on.
        match run_line(&fs, &line) {
            Err(e) if interactive => eprintln!("{e}"),
            other => other?,
        }
    }
    if interactive {
        println!();      // ^D at the prompt: don't glue it to the shell's
    }
    Ok(())
}

pub const CMD: super::CmdDef = typed_cmd!("kvdb", "Btree read/write REPL (debug)", Cli, kvdb);
