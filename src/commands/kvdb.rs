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
//! `set <btree> <pos> deleted` deletes), snapshot (session view context),
//! list_journal (transaction search, --journal opens). One-shot via -c,
//! REPL on stdin otherwise; the REPL reads commands line by line, so tests
//! can pipe a script in.
//!
//! The default open is norecovery (read-only, no replay, no repair passes) -
//! inspection must not disturb the state under inspection. --rw opts into
//! journal replay and writes, and stops there: repair passes must not run
//! either, or they rewrite the state an editing session opened rw to build.
//!
//! Visibility: without a snapshot context, reads are raw
//! (BTREE_ITER_all_snapshots - positions taken literally). With a context,
//! reads without an explicit :snapshot run snapshot-filtered at the context
//! (runtime visibility resolution, whiteouts shown); an explicit :snapshot
//! makes that command raw again. Writes always target the exact key.
//! Full semantics: doc/kvdb.md.
//!
//! Fields are set by the key type's set() (btree/bkey_methods.rs):
//! fixed-layout vals and inodes (through bch_inode_unpacked) are fully
//! editable, entry-stream (extent) vals only up to their fixed header. Updates run the
//! normal triggers and key validation — per-key-invalid keys are (correctly)
//! rejected; whether we want a validation-bypass mode for injecting those is
//! an open question.

use std::io::{stdin, stdout, IsTerminal};
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{anyhow, bail, Result};
use bcachefs_kernel::btree::bkey::{BkeySC, POS_MIN, SPOS_MAX};
use bcachefs_kernel::btree::bkey_methods;
use bcachefs_kernel::btree::iter::{
    commit_do, lockrestart_do, BtreeIter, BtreeIterFlags, CommitFlags,
    TransBkey, UpdateTriggerFlags,
};
use bcachefs_kernel::c;
use bcachefs_kernel::errcode::{bch_errcode, BchError};
use bcachefs_kernel::fs::Fs;
use bcachefs_kernel::inode;
use bcachefs_kernel::opt_set;
use bcachefs_kernel::typeinfo::{self, FieldTarget, StructInfo, TypeInfo};
use bch_bindgen::c::bch_degraded_actions;
use clap::Parser;

use crate::device_scan::OpenedFs;
use crate::logging;
use crate::wrappers::handle::BcachefsHandle;
use crate::wrappers::online_iter::{OnlineBtreeIter, OnlineIterFlags};

const BKEY_U64S: usize = size_of::<c::bkey>() / size_of::<u64>();

// The canonical kvdb documentation: this constant is the --help long text,
// and docgen extracts it into the Principles of Operation (doc/generated/
// kvdb.tex, included from the Debugging tools section). Markup: blank-line
// paragraphs, `- ` bullets, [term] description items, backticks for code,
// 4-space-indented lines render verbatim.
// DOC_STRING(kvdb)
const KVDB_DOC: &str = "\
kvdb is an interactive debugger for bcachefs metadata: it reads and writes
btree keys by field name, using generated runtime type information for the
on-disk structs. Its two jobs are forensics - inspecting a damaged
filesystem's state precisely, without disturbing it - and injection:
constructing exact corruption for fsck/repair tests, or performing
field-level surgery on a filesystem wedged in the field.

Commands come from the REPL (readline editing, history, tab completion for
command and btree names), from -c arguments, or from piped stdin. In a
piped script an error aborts, since later commands likely depend on earlier
ones; interactively, errors are reported and the session continues. ctrl-C
interrupts a long-running command and returns to the prompt; ctrl-D or
`quit` exits.

Opening modes: the default open is read-only with recovery capped
(norecovery). Inspection is the primary use, and a full-recovery open of a
damaged filesystem can repair - rewrite - the very state under inspection;
the default is guaranteed not to write. The journal is still read and
overlaid on btree reads, so listings show current state, but it is never
replayed and no repair passes run.

- `--rw`: journal replay and writes enabled - recovery stops there, at
  going read-write. Repair passes are not run: they would rewrite the
  state you opened rw to construct, before your first command. Whatever
  the superblock requires stays recorded and runs at the next real mount.
  Version upgrades still happen; see the traps below. Required for
  update/set/sb set.
- `--journal`: retain the entire journal in memory for `list_journal`;
  costs memory proportional to journal size.
- `--nostart`: superblock only, the btree is never started. For sb get/set
  on an image that can't or shouldn't be started.
- On a mounted filesystem, reads go through the kernel query ioctl and
  writes are refused.

Two traps: an `--rw` open of a not-yet-upgraded filesystem rewrites
metadata via the version upgrade (for example the snapshot and subvolume
state fields), destroying not-upgraded evidence. And the default open of a
formatted-but-never-started image silently runs first-start initialization
in memory: listings then show a root inode that does not exist on disk.

Commands:

    get    [-k|-r] <btree> <pos>                   exact lookup (slot iteration)
    peek      [-k] <btree> <pos>                   first key >= pos
    peek_prev [-k] <btree> <pos>                   last key <= pos
    list      [-k] <btree> [start] [end]           keys in range
    update [-r] <btree> <pos> <field=val>...       modify fields of a key
    set  [-s][-r] <btree> <pos> <type> [field=val]... insert a whole new key
    copy [-r] <btree> <pos> <new_pos>              duplicate a key elsewhere
    sb get [-d <dev>] <field>                      read a superblock field
    sb set [-d <dev>] <field=val>                  write one
    snapshot  [<id>|none]                          session snapshot context
    list_journal [-k <ranges>]                     journal transactions
    help, quit

Positions are inode:offset[:snapshot], or POS_MIN/POS_MAX/SPOS_MAX. `-k` on
the read commands prints keys only - type, position, size - without
rendering values; useful when mapping out what exists.

Fields are value-struct fields addressed by path (parent, children[1],
btime.hi). Declared flag bits (LE*_BITMASK) resolve by name, qualified as
flags.subvol when a name collides, and fields holding enum codewords accept
value names: state=will_delete. Values are decimal, 0x hex, or negative
decimal. `set <btree> <pos> deleted` removes the exact key; with -s it
deletes within pos's snapshot instead (inserting whiteouts, like a runtime
delete).

`copy <btree> <pos> <new_pos>` duplicates a key exactly as it is - for
values set can't build, extents above all - at another position, which may
be in another inode or another snapshot. The insert is raw: triggers run,
but an extent copy overwrites nothing, so it can overlap the extents there;
the destination slot must be empty. An extent's new_pos is its new end.

`-r` on update, set and copy runs no triggers at all: nothing else is
updated to match - not other btrees, not the accounting - for injecting a
key the rest of the filesystem doesn't know about.

The snapshot context. Snapshot visibility is the subtle dimension of every
bcachefs lookup: a key at snapshot S is visible at S and its descendants
unless overwritten, and a lookup in a snapshot resolves to the nearest
visible version. The `snapshot` command gives the session a view, and reads
then answer 'what does this view see?' instead of 'what is at this exact
position?'. Precisely:

- No context: reads are raw (all snapshots, positions taken literally); an
  omitted :snapshot parses as 0.
- Context set, pos written without :snapshot: the pos picks up the context
  and the read runs snapshot-filtered at it - ancestor versions resolve,
  siblings and descendants are invisible - exactly what a runtime lookup in
  that snapshot sees. Whiteouts are shown, not skipped: in forensics the
  whiteout doing the shadowing is data.
- Context set, pos written with an explicit :snapshot: that command is
  fully raw - unfiltered, exact position, context bypassed. Explicit means
  exact.
- The context only applies to btrees whose keys are snapshotted, and never
  filters writes: update/set target the exact key, the context only fills
  an omitted :snapshot.

For a filtered `list`, the start position names the view: its snapshot
field carries the context whether given, defaulted, or POS_MIN.

Journal search: with `--journal` at open, `list_journal -k <ranges>` prints
only the journal transactions containing updates to the given key ranges -
the who-touched-this-key question. Ranges are btree:pos or
btree:pos-btree:pos, comma separated, optionally prefixed + or -, the same
syntax as the standalone list_journal command. Each transaction prints with
its name, overwrites, and new keys.

Editing goes through the normal transactional path - journalled, triggers
run, key validation applies - in an instance opened with commit-time
validation relaxed and background workers (snapshot deletion, copygc,
reconcile) disabled, so the tool neither refuses the states fsck is being
tested against nor consumes its own injections. This tool can corrupt a
filesystem in precise, surgical ways; that is its purpose.

One editing trap: snapshot IDs allocate descending from U32_MAX and the
in-memory snapshot table is id-indexed; inserting a snapshot key with an
unrealistically low id asks the table to span billions of entries and fails
with ENOMEM_mark_snapshot. Use realistic, near-U32_MAX ids when fabricating
snapshots.

Fixed-layout values are fully editable. Inodes are too, by their unpacked
field names (bi_dir, bi_nlink, ...): an edit unpacks, sets and repacks, so
it rewrites the inode as inode_v3, and can only produce what the packer
does. `update -r` writes the value as stored instead, by its own struct's
fields (an inode's bi_flags, bi_journal_seq, ...), and runs no triggers:
for states the packer or a trigger would correct, such as has_inode_opts
or a bi_journal_seq in the future. Entry-stream values (extents) are
editable only up to their fixed header.
";

/// Btree read/write REPL (debug)
#[derive(Parser, Debug)]
#[command(long_about = KVDB_DOC)]
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

    /// Read-only, no journal replay or repair passes (recovery capped at
    /// snapshots_read; journal keys still overlay reads). This is the
    /// default; the flag is accepted for compatibility.
    #[arg(long)]
    norecovery: bool,

    /// Open read-write with full recovery: journal replay, scheduled repair
    /// passes, version upgrades. Required for update/set/sb set. On a
    /// damaged filesystem this can repair - rewrite - the state you may
    /// have wanted to inspect.
    #[arg(long)]
    rw: bool,

    /// Retain the entire journal in memory for the list_journal command
    /// (costs memory proportional to journal size).
    #[arg(long)]
    journal: bool,

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

/// An explicit :snapshot in a pos means a raw lookup at exactly that
/// position - it disables the snapshot context for the command, rather than
/// the command running filtered (which would resolve visibility and could
/// return a different version than the one asked for).
fn pos_has_explicit_snapshot(s: &str) -> bool {
    s.matches(':').count() >= 2
}

fn parse_pos(s: &str) -> Result<c::bpos> {
    s.parse()
        .map_err(|_| anyhow!("invalid pos '{s}' (inode:offset[:snapshot], POS_MIN, SPOS_MAX)"))
}

fn parse_int(s: &str) -> Result<u64> {
    bkey_methods::parse_int(s).ok_or_else(|| anyhow!("expected an integer, got '{s}'"))
}

/// `field=value`, both as text: the value is parsed by the field's key type
/// (TransBkey::set()).
fn parse_assign(s: &str) -> Result<(&str, &str)> {
    s.split_once('=')
        .ok_or_else(|| anyhow!("expected field=value, got '{s}'"))
}

/// The bcachefs to_text methods are the faithful rendering of the on-disk
/// structs - that is their purpose. The typeinfo field tables are for
/// addressing fields on the write side (update/set), not for display.
fn render_key(fs: &Fs, k: &BkeySC<'_>, key_only: bool) -> String {
    if key_only {
        format!("{}\n", k.to_text_key())
    } else {
        format!("{}\n", k.to_text(fs))
    }
}

/// The read half of the field engine: selected fields of a key's value as
/// bare values, one line per path - the same paths update addresses for
/// writes, so scripts get a stable contract instead of scraping the
/// human-oriented to_text display. Whole arrays print space-separated.
/// A field beyond the end of a short (older-format) value reads as zero,
/// mirroring update's grow-zero-filled write semantics.
///
/// Inodes are read through the decoded form, bch_inode_unpacked, as update
/// writes them: the varint-packed fields have no fixed position.
fn render_key_fields(fs: &Fs, k: &BkeySC<'_>, paths: &[&str]) -> Result<String> {
    if inode::bkey_is_inode(k.k) {
        let u = inode::unpack(fs, *k);
        let bytes = unsafe {
            core::slice::from_raw_parts(&u as *const _ as *const u8,
                                        size_of::<c::bch_inode_unpacked>())
        };
        return render_fields(bytes, <c::bch_inode_unpacked as TypeInfo>::INFO, paths);
    }

    render_raw_fields(k, paths)
}

/// Fields of the value as stored, by its own struct: get -r, for update -r.
fn render_raw_fields(k: &BkeySC<'_>, paths: &[&str]) -> Result<String> {
    let info = typeinfo::bkey_val_info(k.k.type_ as u32)
        .ok_or_else(|| anyhow!("unknown key type {}", k.k.type_))?;
    render_fields(k.val_bytes(), info, paths)
}

fn render_fields(val: &[u8], info: &'static StructInfo, paths: &[&str]) -> Result<String> {
    use std::fmt::Write as _;
    use typeinfo::{AccessError, FieldKind, FieldRef};

    let read_int = |path: &str, r: &FieldRef| -> Result<String> {
        let v = match typeinfo::read_scalar(val, r) {
            Ok(v) => v,
            Err(AccessError::OutOfBounds { .. }) => 0,
            Err(e) => bail!("{path}: {e}"),
        };
        Ok(match r.kind {
            FieldKind::Int { signed: true, bytes, .. } =>
                typeinfo::sign_extend(v, *bytes).to_string(),
            _ => v.to_string(),
        })
    };

    let mut out = String::new();
    for path in paths {
        let (r, bm) = typeinfo::resolve_with_bits(info, path).map_err(|e| anyhow!("{e}"))?;
        let line = match bm {
            Some(bm) => match typeinfo::read_bits(val, &r, bm) {
                Ok(v) => v.to_string(),
                Err(AccessError::OutOfBounds { .. }) => "0".to_string(),
                Err(e) => bail!("{path}: {e}"),
            },
            None => match r.kind {
                FieldKind::Int { .. } => read_int(path, &r)?,
                FieldKind::Array { elem, n, stride }
                        if matches!(elem, FieldKind::Int { .. }) => {
                    (0..*n)
                        .map(|i| read_int(path, &FieldRef {
                            offset: r.offset + i * stride,
                            kind: elem,
                            len: *stride,
                        }))
                        .collect::<Result<Vec<_>>>()?
                        .join(" ")
                }
                // A vartail's length is however much value remains:
                FieldKind::VarTail { elem, stride }
                        if matches!(elem, FieldKind::Int { .. }) => {
                    (0..(val.len().saturating_sub(r.offset) / stride))
                        .map(|i| read_int(path, &FieldRef {
                            offset: r.offset + i * stride,
                            kind: elem,
                            len: *stride,
                        }))
                        .collect::<Result<Vec<_>>>()?
                        .join(" ")
                }
                _ => bail!("{path}: not a scalar or integer-array field"),
            },
        };
        writeln!(out, "{line}").unwrap();
    }
    Ok(out)
}

/// Room for whatever set() grows a value to: the largest key there is.
const BKEY_VAL_U64S_MAX: usize = u8::MAX as usize - BKEY_U64S;

/// Apply @assigns to @k, reporting the first failure through @user_err. @raw:
/// to the value as stored - see TransBkey::set_raw().
fn set_fields(
    fs:       &Fs,
    k:        &mut TransBkey<'_, '_>,
    assigns:  &[(&str, &str)],
    raw:      bool,
    user_err: &mut Option<anyhow::Error>,
) -> Result<(), BchError> {
    for (field, val) in assigns {
        let ret = if raw { k.set_raw(field, val) } else { k.set(fs, field, val) };
        if let Err(e) = ret {
            *user_err = Some(anyhow!("{e}"));
            return Err(no_key_err());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// ops

/// Raw exact addressing for get/update: the snapshot field of pos is taken
/// literally. all_snapshots is dropped by the C flag fixup on btrees without
/// a snapshot field, so passing it unconditionally is fine.
const RAW_EXACT: BtreeIterFlags = BtreeIterFlags::SLOTS.union(BtreeIterFlags::ALL_SNAPSHOTS);

/// The snapshot context only applies to btrees whose keys are actually
/// snapshotted - on anything else it would corrupt positions (snapshots
/// btree keys live at snapshot 0) and filtering is meaningless.
fn btree_uses_snapshots(btree: c::btree_id) -> bool {
    bcachefs_kernel::BTREE_HAS_SNAPSHOTS_MASK & (1u64 << btree as u64) != 0
}

/// With a snapshot context active, reads drop ALL_SNAPSHOTS - the iterator
/// runs snapshot-filtered (BTREE_ITER_filter_snapshots) at pos.snapshot, so
/// lookups resolve visibility the way runtime lookups do - and set
/// NOFILTER_WHITEOUTS: a forensics tool must show the whiteout doing the
/// shadowing, not silently hide the deletion.
fn iter_flags(base: BtreeIterFlags, filtered: bool) -> BtreeIterFlags {
    if filtered {
        base.difference(BtreeIterFlags::ALL_SNAPSHOTS)
            .union(BtreeIterFlags::NOFILTER_WHITEOUTS)
    } else {
        base
    }
}

/// What a read command prints for a matched key: the C to_text line (the
/// faithful display), the key alone (-k), or selected value fields as bare
/// values (trailing field paths). A deleted slot is "no key" for a field
/// read - scripts reading fields need the miss to fail, not to read zeros -
/// while the display modes do show deleted slots and whiteouts. RawFields
/// (-r): the value as stored, by its own struct - what update -r writes.
#[derive(Clone, Copy)]
enum Render<'a> {
    Full,
    KeyOnly,
    Fields(&'a [&'a str]),
    RawFields(&'a [&'a str]),
}

fn render_read(fs: &Fs, k: &BkeySC<'_>, how: Render<'_>) -> Result<String> {
    match how {
        Render::Full => Ok(render_key(fs, k, false)),
        Render::KeyOnly => Ok(render_key(fs, k, true)),
        Render::Fields(_) | Render::RawFields(_) if k.is_deleted() => {
            let (inode, offset, snapshot) = (k.k.p.inode, k.k.p.offset, k.k.p.snapshot);
            Err(anyhow!("no key at {inode}:{offset}:{snapshot}"))
        }
        Render::Fields(paths) => render_key_fields(fs, k, paths),
        Render::RawFields(paths) => render_raw_fields(k, paths),
    }
}

/// The one-key read commands differ only in how they position the
/// iterator: get is an exact slot lookup, peek/peek_prev scan for the
/// nearest key.
#[derive(Clone, Copy, PartialEq)]
enum ReadOp {
    Get,
    Peek,
    PeekPrev,
}

fn cmd_read(fs: &Fs, op: ReadOp, btree: c::btree_id, pos: c::bpos, filtered: bool,
            how: Render<'_>) -> Result<String> {
    let trans = bcachefs_kernel::btree_trans!(fs);
    let mut user_err: Option<anyhow::Error> = None;
    let out = lockrestart_do(&trans, |t| {
        let base = match op {
            ReadOp::Get => RAW_EXACT,
            _ => BtreeIterFlags::ALL_SNAPSHOTS,
        };
        let mut iter = BtreeIter::new(t.trans(), btree, pos, iter_flags(base, filtered));
        let out = match op {
            ReadOp::Get => iter.peek_max_flags(t, SPOS_MAX, BtreeIterFlags::SLOTS),
            ReadOp::Peek => iter.peek(t),
            ReadOp::PeekPrev => iter.peek_prev(t),
        }?;
        match out {
            Some(k) => render_read(fs, &k, how).map_err(|e| {
                // Render errors (bad field path, no key for a field read)
                // are the user's, not the transaction's: stash and abort
                // the retry loop with a stand-in errcode.
                user_err = Some(e);
                BchError::from(bch_errcode::BCH_ERR_ENOENT_bkey_type_mismatch)
            }),
            None => Ok("(no key)\n".to_string()),
        }
    });
    match user_err {
        Some(e) => Err(e),
        None => Ok(out?),
    }
}

/// Tab completion: command names for the first word, btree names (from the
/// same string table FromStr parses) after a key command, get/set after sb.
struct KvdbHelper {
    btrees: Vec<String>,
}


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
            [] => COMMANDS.iter().map(|c| c.name.to_string()).collect(),
            [op] => match COMMANDS.iter().find(|c| c.name == *op || c.aliases.contains(op)) {
                Some(c) if c.completes_btree => self.btrees.clone(),
                Some(c) => c.subcommands.iter().map(|s| s.to_string()).collect(),
                None => vec![],
            },
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
            filtered: bool, key_only: bool) -> Result<String> {
    let trans = bcachefs_kernel::btree_trans!(fs);
    let mut out = String::new();
    let mut iter = BtreeIter::new(
        &trans,
        btree,
        start,
        iter_flags(BtreeIterFlags::ALL_SNAPSHOTS | BtreeIterFlags::PREFETCH, filtered),
    );
    iter.for_each_max(&trans, end, |_, k| {
        if take_interrupt() {
            out.push_str("(interrupted)\n");
            return Ok(ControlFlow::Break(()));
        }
        if key_only {
            out.push_str(&format!("{}\n", k.to_text_key()));
        } else {
            out.push_str(&format!("{}\n", k.to_text(fs)));
        }
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(out)
}

/// One key from a mounted filesystem, or None. get: slot iteration at an
/// exact pos; peek/peek_prev: first key at/after (at/before) pos.
fn online_one_key(handle: &BcachefsHandle, fs: &Fs,
		  btree: c::btree_id, pos: c::bpos,
		  flags: OnlineIterFlags, how: Render<'_>) -> Result<String> {
    // Small buffer: the kernel fills the whole thing per call, and we only
    // want one key (it grows automatically if the key doesn't fit):
    let mut iter = OnlineBtreeIter::with_buf_size(handle, btree, 0, pos,
					if flags.0 & OnlineIterFlags::PREV.0 != 0 { POS_MIN } else { SPOS_MAX },
					flags, 4096);
    match iter.next().map_err(|e| anyhow!("BCH_IOCTL_QUERY_BTREE_KEYS: {e}"))? {
        Some(k) => render_read(fs, &k, how),
        None => Ok("(no key)\n".to_string()),
    }
}

/// Online counterpart of iter_flags(): the ioctl maps its flags 1:1 into
/// btree iter flags, so omitting all_snapshots gets filtered iteration
/// kernel-side. (nofilter_whiteouts is rejected by kernels predating the
/// bit - run a matching kernel for online filtered reads.)
fn online_snapshots_flag(filtered: bool) -> OnlineIterFlags {
    if filtered {
        OnlineIterFlags::NOFILTER_WHITEOUTS
    } else {
        OnlineIterFlags::ALL_SNAPSHOTS
    }
}

fn cmd_read_online(handle: &BcachefsHandle, fs: &Fs, op: ReadOp,
		   btree: c::btree_id, pos: c::bpos, filtered: bool,
		   how: Render<'_>) -> Result<String> {
    let flags = online_snapshots_flag(filtered) | match op {
        ReadOp::Get => OnlineIterFlags::SLOTS,
        ReadOp::Peek => OnlineIterFlags(0),
        ReadOp::PeekPrev => OnlineIterFlags::PREV,
    };
    online_one_key(handle, fs, btree, pos, flags, how)
}

fn cmd_list_online(handle: &BcachefsHandle, fs: &Fs,
		   btree: c::btree_id, start: c::bpos, end: c::bpos,
		   filtered: bool, key_only: bool) -> Result<String> {
    let mut out = String::new();
    let mut iter = OnlineBtreeIter::new(handle, btree, 0, start, end,
					online_snapshots_flag(filtered));
    iter.for_each(|k| {
        if take_interrupt() {
            out.push_str("(interrupted)\n");
            return ControlFlow::Break(());
        }
        if key_only {
            out.push_str(&format!("{}\n", k.to_text_key()));
        } else {
            out.push_str(&format!("{}\n", k.to_text(fs)));
        }
        ControlFlow::Continue(())
    }).map_err(|e| anyhow!("BCH_IOCTL_QUERY_BTREE_KEYS: {e}"))?;
    Ok(out)
}

fn no_key_err() -> BchError {
    bch_errcode::BCH_ERR_ENOENT_bkey_type_mismatch.into()
}

fn cmd_update(
    fs: &Fs,
    btree: c::btree_id,
    pos: c::bpos,
    assigns: &[(&str, &str)],
    raw: bool,
) -> Result<String> {
    let trans = bcachefs_kernel::btree_trans!(fs);
    let mut user_err: Option<anyhow::Error> = None;

    let commit = commit_do(
        &trans,
        None,
        CommitFlags::NO_ENOSPC,
        |t| {
            let mut iter = BtreeIter::new(
                t.trans(),
                btree,
                pos,
                RAW_EXACT | BtreeIterFlags::INTENT,
            );
            let k = iter.peek_max_flags(t, SPOS_MAX, BtreeIterFlags::SLOTS)?;
            let Some(k) = k.filter(|k| !k.is_deleted()) else {
                let (inode, offset, snapshot) = (pos.inode, pos.offset, pos.snapshot);
                user_err = Some(anyhow!("no key at {inode}:{offset}:{snapshot}"));
                return Err(no_key_err());
            };

            // A copy of the key, as it is, in a buffer set() can grow it into:
            let mut new = t.bkey_reassemble_resized(k, BKEY_VAL_U64S_MAX)?;
            new.k_mut().u64s = k.k.u64s;

            set_fields(fs, &mut new, assigns, raw, &mut user_err)?;

            t.update(&iter, &new, raw_flags(UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE, raw))
        },
    );

    match commit {
        Ok(()) => Ok(String::new()),
        Err(e) => Err(user_err.unwrap_or_else(|| anyhow!("update failed: {e}"))),
    }
}

/// The -r writes: no triggers. They would restamp what was just set (an
/// inode's bi_journal_seq), or update other btrees and the accounting to
/// match - and a key the accounting doesn't know about is one of the states
/// being injected.
fn raw_flags(flags: UpdateTriggerFlags, raw: bool) -> UpdateTriggerFlags {
    if raw { flags | UpdateTriggerFlags::NORUN } else { flags }
}

/// Duplicate the key at @pos at @new_pos, exactly as it is: a raw insert -
/// triggers run, but an extent overwrites nothing, so the copy can overlap
/// what's there - into a slot that must be empty. For an extent, @new_pos is
/// the copy's end, its size unchanged. @raw: no triggers - see raw_flags().
fn cmd_copy(fs: &Fs, btree: c::btree_id, pos: c::bpos, new_pos: c::bpos, raw: bool)
    -> Result<String>
{
    let trans = bcachefs_kernel::btree_trans!(fs);
    let mut user_err: Option<anyhow::Error> = None;

    let commit = commit_do(
        &trans,
        None,
        CommitFlags::NO_ENOSPC,
        |t| {
            let mut iter = BtreeIter::new(t.trans(), btree, pos, RAW_EXACT);
            let k = iter.peek_max_flags(t, SPOS_MAX, BtreeIterFlags::SLOTS)?;
            let Some(k) = k.filter(|k| !k.is_deleted()) else {
                let (inode, offset, snapshot) = (pos.inode, pos.offset, pos.snapshot);
                user_err = Some(anyhow!("no key at {inode}:{offset}:{snapshot}"));
                return Err(no_key_err());
            };

            let mut new_iter = BtreeIter::new(t.trans(), btree, new_pos,
                                              RAW_EXACT | BtreeIterFlags::INTENT);
            let old = new_iter.peek_max_flags(t, SPOS_MAX, BtreeIterFlags::SLOTS)?;
            if old.is_some_and(|old| !old.is_deleted()) {
                let (inode, offset, snapshot) = (new_pos.inode, new_pos.offset, new_pos.snapshot);
                user_err = Some(anyhow!("{inode}:{offset}:{snapshot} is occupied"));
                return Err(no_key_err());
            }

            let mut new = t.bkey_reassemble(k)?;
            new.k_mut().p = new_pos;

            t.update(&new_iter, &new, raw_flags(UpdateTriggerFlags::INTERNAL_SNAPSHOT_NODE, raw))
        },
    );

    match commit {
        Ok(()) => Ok(String::new()),
        Err(e) => Err(user_err.unwrap_or_else(|| anyhow!("copy failed: {e}"))),
    }
}

fn cmd_set(
    fs: &Fs,
    btree: c::btree_id,
    pos: c::bpos,
    type_name: &str,
    assigns: &[(&str, &str)],
    in_snapshot: bool,
    raw: bool,
) -> Result<String> {
    let ti = typeinfo::bkey_type_info_by_name(type_name)
        .ok_or_else(|| anyhow!("unknown key type '{type_name}'"))?;

    let trans = bcachefs_kernel::btree_trans!(fs);
    let mut user_err: Option<anyhow::Error> = None;

    let commit = commit_do(
        &trans,
        None,
        CommitFlags::NO_ENOSPC,
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
            let update_flags = raw_flags(update_flags, raw);
            let mut iter = BtreeIter::new(
                t.trans(),
                btree,
                pos,
                iter_flags,
            );
            iter.peek_max_flags(t, SPOS_MAX, BtreeIterFlags::SLOTS)?;

            // A zeroed value of the struct's fixed size, in a buffer set()
            // can grow it into - vartail elements (damage errors[n]) lie
            // beyond the struct:
            let mut new = t.bkey_alloc_init(BKEY_VAL_U64S_MAX, ti.type_ as u8, pos)?;
            new.k_mut().u64s = (BKEY_U64S + ti.info.size.div_ceil(8)) as u8;

            set_fields(fs, &mut new, assigns, false, &mut user_err)?;

            t.update(&iter, &new, update_flags)
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

// With a device, its own superblock (ca->disk_sb) rather than the filesystem's:
// bch2_write_super() copies the filesystem's over each device's, all but the
// per-device parts - the layout, the offset and dev_idx - so those are the
// fields worth setting this way; anything else is overwritten by the write.
fn sb_dev(fs: &Fs, dev: u32) -> Result<bcachefs_kernel::fs::DevRef> {
    fs.dev_get(dev).ok_or_else(|| anyhow!("no device {dev}"))
}

fn cmd_sb_get(fs: &Fs, dev: Option<u32>, field: &str) -> Result<String> {
    let (r, bm) = sb_field(field)?;
    let ca = dev.map(|d| sb_dev(fs, d)).transpose()?;
    let buf = match &ca {
        Some(ca) => ca.disk_sb.sb_bytes(),
        None     => fs.disk_sb().sb_bytes(),
    };
    let v = match bm {
        Some(bm) => typeinfo::read_bits(buf, &r, bm),
        None => typeinfo::read_scalar(buf, &r),
    }.map_err(|e| anyhow!("{field}: {e}"))?;
    Ok(format!("{field} = {v} (0x{v:x})\n"))
}

fn cmd_sb_set(fs: &Fs, dev: Option<u32>, field: &str, v: u64) -> Result<String> {
    let target = sb_field(field)?;
    let ca = dev.map(|d| sb_dev(fs, d)).transpose()?;

    let _lock = unsafe { crate::wrappers::sb_lock(fs.raw) };

    /* sb_lock is held above - disk_sb_mut's contract, and ca->disk_sb's: */
    let buf = match &ca {
        Some(ca) => unsafe { &mut (*ca.as_mut_ptr()).disk_sb }.sb_bytes_mut(),
        None     => unsafe { fs.disk_sb_mut() }.sb_bytes_mut(),
    };
    typeinfo::write(buf, &target, v).map_err(|e| anyhow!("{field}: {e}"))?;

    // Only --rw (started) and --nostart (opened will_not_start, which makes
    // no version decisions to persist) get here - see the check in the
    // command loop.
    fs.write_super_ret()
        .map_err(|e| anyhow!("bch2_write_super failed: {e}"))?;
    Ok(String::new())
}

// ---------------------------------------------------------------------------
// command dispatch + REPL

const HELP: &str = "\
get  [-k|-r] <btree> <pos> [<field>..]         exact lookup (slot iteration)
peek [-k] <btree> <pos> [<field>..]            first key >= pos
peek_prev <btree> <pos> [<field>..]            last key <= pos ([-k] too)
list [-k] <btree> [start] [end]                keys in range
          -k prints keys only, without rendering values
          trailing field paths print those fields as bare values, one per
          line, instead of the display rendering - the same paths update
          takes (depth, btime.lo, skip[1]); a whole array prints its
          elements space-separated
          -r reads fields of the value as stored, not an inode's unpacked
          form - what update -r writes
update [-r] <btree> <pos> <field=val>...       modify fields of an existing key
          -r writes the value as stored, by its own struct's fields, and
          runs no triggers: states the inode packer or a trigger would
          correct (has_inode_opts, a future bi_journal_seq)
set  [-s][-r] <btree> <pos> <type> [field=val]... insert a whole new key
          values are integers; fields holding enum codewords (snapshot/
          subvolume state) also accept the value name, e.g. state=will_delete
          `set <pos> deleted` removes the exact key; with -s it deletes
          within pos's snapshot instead (whiteouts, like a runtime delete)
copy [-r] <btree> <pos> <new_pos>              duplicate a key, as it is, elsewhere
          raw: an extent copy can overlap what's there; new_pos may be in
          another inode or snapshot, and must be empty. An extent's new_pos
          is its end
sb get [-d <dev>] <field>                      read a superblock field/flag
sb set [-d <dev>] <field=val>                  write one, then bch2_write_super
          -d: that device's own superblock - only its per-device parts
          (layout.*, offset, dev_idx) survive the write; the rest is
          copied over from the filesystem's
list_journal [-k [+-]<bbpos>[-<bbpos>],...]    journal transactions, filtered to
                                               those referencing the given key
                                               ranges (needs --journal at open)
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

    /// The offline Fs, for operations that need libbcachefs btree/journal
    /// access (writes, list_journal).
    fn offline(&self) -> Result<&Fs> {
        match self {
            KvdbFs::Offline(fs) => Ok(fs),
            KvdbFs::Online(..) => bail!(
                "filesystem is mounted: this command needs offline access \
                 (online kvdb reads via the kernel, read-only)"
            ),
        }
    }

    fn read(&self, op: ReadOp, btree: c::btree_id, pos: c::bpos, filtered: bool,
            how: Render<'_>) -> Result<String> {
        match self {
            KvdbFs::Offline(fs) => cmd_read(fs, op, btree, pos, filtered, how),
            KvdbFs::Online(handle, fs) =>
                cmd_read_online(handle, fs, op, btree, pos, filtered, how),
        }
    }

    fn list(&self, btree: c::btree_id, start: c::bpos, end: c::bpos, filtered: bool,
            key_only: bool) -> Result<String> {
        match self {
            KvdbFs::Offline(fs) => cmd_list(fs, btree, start, end, filtered, key_only),
            KvdbFs::Online(handle, fs) =>
                cmd_list_online(handle, fs, btree, start, end, filtered, key_only),
        }
    }
}

/// Session state threaded to command handlers: the open filesystem, the
/// open-mode capabilities, and the snapshot context.
struct Repl<'a> {
    fs: &'a KvdbFs,
    nostart: bool,
    journal: bool,
    rw: bool,
    snapshot: Option<u32>,
}

type CmdHandler = fn(&mut Repl, &Cmd, &[&str]) -> Result<ControlFlow<(), String>>;

/// One entry per command; dispatch, capability gating, usage messages, and
/// tab completion all read from this table, so they can't drift.
struct Cmd {
    name: &'static str,
    aliases: &'static [&'static str],
    usage: &'static str,
    /// second word completes as a btree name
    completes_btree: bool,
    /// second-word completions for commands with subcommands
    subcommands: &'static [&'static str],
    /// available under --nostart (superblock-only open)
    nostart_ok: bool,
    /// write command: requires --rw
    needs_rw: bool,
    /// requires --journal
    needs_journal: bool,
    handler: CmdHandler,
}

const COMMANDS: &[Cmd] = &[
    Cmd { name: "get", aliases: &[], usage: "get [-k|-r] <btree> <pos> [<field>..]",
          completes_btree: true, subcommands: &[],
          nostart_ok: false, needs_rw: false, needs_journal: false, handler: h_read },
    Cmd { name: "peek", aliases: &[], usage: "peek [-k] <btree> <pos> [<field>..]",
          completes_btree: true, subcommands: &[],
          nostart_ok: false, needs_rw: false, needs_journal: false, handler: h_read },
    Cmd { name: "peek_prev", aliases: &[], usage: "peek_prev [-k] <btree> <pos> [<field>..]",
          completes_btree: true, subcommands: &[],
          nostart_ok: false, needs_rw: false, needs_journal: false, handler: h_read },
    Cmd { name: "list", aliases: &[], usage: "list [-k] <btree> [start] [end]",
          completes_btree: true, subcommands: &[],
          nostart_ok: false, needs_rw: false, needs_journal: false, handler: h_list },
    Cmd { name: "update", aliases: &[], usage: "update [-r] <btree> <pos> <field=val>...",
          completes_btree: true, subcommands: &[],
          nostart_ok: false, needs_rw: true, needs_journal: false, handler: h_update },
    Cmd { name: "set", aliases: &[], usage: "set [-s] [-r] <btree> <pos> <type> [field=val]...",
          completes_btree: true, subcommands: &[],
          nostart_ok: false, needs_rw: true, needs_journal: false, handler: h_set },
    Cmd { name: "copy", aliases: &[], usage: "copy [-r] <btree> <pos> <new_pos>",
          completes_btree: true, subcommands: &[],
          nostart_ok: false, needs_rw: true, needs_journal: false, handler: h_copy },
    Cmd { name: "sb", aliases: &[], usage: "sb get [-d <dev>] <field> | sb set [-d <dev>] <field=val>",
          completes_btree: false, subcommands: &["get", "set"],
          nostart_ok: true, needs_rw: false, needs_journal: false, handler: h_sb },
    Cmd { name: "snapshot", aliases: &[], usage: "snapshot [<id>|none]",
          completes_btree: false, subcommands: &[],
          nostart_ok: true, needs_rw: false, needs_journal: false, handler: h_snapshot },
    Cmd { name: "list_journal", aliases: &[],
          usage: "list_journal [-k [+-]<btree>:<pos>[-<btree>:<pos>],...]",
          completes_btree: false, subcommands: &[],
          nostart_ok: false, needs_rw: false, needs_journal: true, handler: h_list_journal },
    Cmd { name: "help", aliases: &["?"], usage: "help",
          completes_btree: false, subcommands: &[],
          nostart_ok: true, needs_rw: false, needs_journal: false, handler: h_help },
    Cmd { name: "quit", aliases: &["exit", "q"], usage: "quit",
          completes_btree: false, subcommands: &[],
          nostart_ok: true, needs_rw: false, needs_journal: false, handler: h_quit },
];

fn h_quit(_: &mut Repl, _: &Cmd, _: &[&str]) -> Result<ControlFlow<(), String>> {
    Ok(ControlFlow::Break(()))
}

fn h_help(_: &mut Repl, _: &Cmd, _: &[&str]) -> Result<ControlFlow<(), String>> {
    Ok(ControlFlow::Continue(HELP.to_string()))
}

fn h_snapshot(repl: &mut Repl, cmd: &Cmd, args: &[&str]) -> Result<ControlFlow<(), String>> {
    Ok(ControlFlow::Continue(match args {
        [] => match repl.snapshot {
            Some(s) => format!("snapshot context: {s}\n"),
            None => "no snapshot context\n".to_string(),
        },
        ["none"] | ["clear"] => {
            repl.snapshot = None;
            String::new()
        }
        [s] => {
            repl.snapshot = Some(u32::try_from(parse_int(s)?)?);
            String::new()
        }
        _ => bail!("usage: {}", cmd.usage),
    }))
}

fn h_read(repl: &mut Repl, cmd: &Cmd, args: &[&str]) -> Result<ControlFlow<(), String>> {
    let (key_only, raw, args) = match args {
        ["-k", rest @ ..] => (true, false, rest),
        ["-r", rest @ ..] => (false, true, rest),
        _ => (false, false, args),
    };
    let [btree, pos, fields @ ..] = args else {
        bail!("usage: {}", cmd.usage);
    };
    let how = match (key_only, raw, fields) {
        (false, false, []) => Render::Full,
        (true, _, []) => Render::KeyOnly,
        (false, false, fields) => Render::Fields(fields),
        (false, true, []) => bail!("-r reads fields: name them"),
        (false, true, fields) => Render::RawFields(fields),
        (true, _, _) => bail!("-k and field selection are mutually exclusive"),
    };
    let btree = parse_btree(btree)?;
    let ctx = repl.snapshot
        .filter(|_| btree_uses_snapshots(btree) && !pos_has_explicit_snapshot(pos));
    let mut pos = parse_pos(pos)?;
    if let Some(snap) = ctx {
        pos.snapshot = snap;
    }
    let filtered = ctx.is_some();
    let op = match cmd.name {
        "get" => ReadOp::Get,
        "peek" => ReadOp::Peek,
        _ => ReadOp::PeekPrev,
    };
    Ok(ControlFlow::Continue(repl.fs.read(op, btree, pos, filtered, how)?))
}

fn h_list(repl: &mut Repl, cmd: &Cmd, args: &[&str]) -> Result<ControlFlow<(), String>> {
    let (key_only, args) = match args {
        ["-k", rest @ ..] => (true, rest),
        _ => (false, args),
    };
    let (btree, rest) = args
        .split_first()
        .ok_or_else(|| anyhow!("usage: {}", cmd.usage))?;
    let btree = parse_btree(btree)?;
    let ctx = repl.snapshot.filter(|_| {
        btree_uses_snapshots(btree)
            && !rest.iter().take(2).any(|s| pos_has_explicit_snapshot(s))
    });
    let mut start = rest.first().map_or(Ok(POS_MIN), |s| parse_pos(s))?;
    let end = rest.get(1).map_or(Ok(SPOS_MAX), |s| parse_pos_ctx(s, ctx))?;
    // A filtered iterator's snapshot comes from the start pos - it's the
    // view - so it must carry the context whether the start was given,
    // defaulted, or POS_MIN (snapshot 0 is never a valid view):
    if let Some(snap) = ctx {
        start.snapshot = snap;
    }
    let filtered = ctx.is_some();
    Ok(ControlFlow::Continue(repl.fs.list(btree, start, end, filtered, key_only)?))
}

fn h_update(repl: &mut Repl, cmd: &Cmd, args: &[&str]) -> Result<ControlFlow<(), String>> {
    let (raw, args) = match args {
        ["-r", rest @ ..] => (true, rest),
        _ => (false, args),
    };
    let [btree, pos, assigns @ ..] = args else {
        bail!("usage: {}", cmd.usage);
    };
    if assigns.is_empty() {
        bail!("usage: {}", cmd.usage);
    }
    let fs = repl.fs.offline()?;
    let assigns = assigns
        .iter()
        .map(|s| parse_assign(s))
        .collect::<Result<Vec<_>>>()?;
    let btree = parse_btree(btree)?;
    let ctx = repl.snapshot.filter(|_| btree_uses_snapshots(btree));
    Ok(ControlFlow::Continue(cmd_update(fs, btree, parse_pos_ctx(pos, ctx)?, &assigns, raw)?))
}

fn h_set(repl: &mut Repl, cmd: &Cmd, args: &[&str]) -> Result<ControlFlow<(), String>> {
    let (mut in_snapshot, mut raw, mut args) = (false, false, args);
    loop {
        match args {
            ["-s", rest @ ..] => { in_snapshot = true; args = rest; }
            ["-r", rest @ ..] => { raw = true;         args = rest; }
            _ => break,
        }
    }
    let [btree, pos, type_name, assigns @ ..] = args else {
        bail!("usage: {}", cmd.usage);
    };
    if in_snapshot && *type_name != "deleted" {
        bail!("-s (delete within pos's snapshot) only applies to deletions");
    }
    let fs = repl.fs.offline()?;
    let assigns = assigns
        .iter()
        .map(|s| parse_assign(s))
        .collect::<Result<Vec<_>>>()?;
    let btree = parse_btree(btree)?;
    let ctx = repl.snapshot.filter(|_| btree_uses_snapshots(btree));
    Ok(ControlFlow::Continue(
        cmd_set(fs, btree, parse_pos_ctx(pos, ctx)?, type_name, &assigns, in_snapshot, raw)?,
    ))
}

fn h_copy(repl: &mut Repl, cmd: &Cmd, args: &[&str]) -> Result<ControlFlow<(), String>> {
    let (raw, args) = match args {
        ["-r", rest @ ..] => (true, rest),
        _ => (false, args),
    };
    let [btree, pos, new_pos] = args else {
        bail!("usage: {}", cmd.usage);
    };
    let fs = repl.fs.offline()?;
    let btree = parse_btree(btree)?;
    let ctx = repl.snapshot.filter(|_| btree_uses_snapshots(btree));
    Ok(ControlFlow::Continue(
        cmd_copy(fs, btree, parse_pos_ctx(pos, ctx)?, parse_pos_ctx(new_pos, ctx)?, raw)?,
    ))
}

fn h_sb(repl: &mut Repl, cmd: &Cmd, args: &[&str]) -> Result<ControlFlow<(), String>> {
    let (&sub, rest) = args.split_first()
        .ok_or_else(|| anyhow!("usage: {}", cmd.usage))?;
    let (dev, rest) = match rest {
        ["-d", dev, rest @ ..] => (Some(dev.parse::<u32>()
                                        .map_err(|_| anyhow!("sb: -d takes a device index, got {dev}"))?),
                                   rest),
        _ => (None, rest),
    };
    Ok(ControlFlow::Continue(match sub {
        "get" => {
            let [field] = rest else { bail!("usage: sb get [-d <dev>] <field>"); };
            cmd_sb_get(repl.fs.fs(), dev, field)?
        }
        "set" => {
            let [assign] = rest else { bail!("usage: sb set [-d <dev>] <field=val>"); };
            // Under the default norecovery open, nochanges makes
            // bch2_write_super() a silent no-op - refuse rather than claim
            // success. (Not table-gated needs_rw: sb get is fine read-only,
            // and --nostart is the sb-editing mode and never sets nochanges.)
            if !repl.rw && !repl.nostart {
                bail!("read-only (the default is norecovery): \
                       reopen with --rw, or --nostart for sb-only edits");
            }
            let (field, val) = parse_assign(assign)?;
            cmd_sb_set(repl.fs.offline()?, dev, field, parse_int(val)?)?
        }
        _ => bail!("usage: {}", cmd.usage),
    }))
}

fn h_list_journal(repl: &mut Repl, cmd: &Cmd, args: &[&str]) -> Result<ControlFlow<(), String>> {
    let fs = repl.fs.offline()?;
    let mut f = super::list_journal::JournalFilter::default();
    // Searching the journal, not auditing it: gaps in the sequence aren't
    // what we're here for, and on a damaged fs they're endless spam.
    f.print_missing = false;
    let mut args = args;
    while let Some((&flag, rest)) = args.split_first() {
        match flag {
            "-k" => {
                let Some((&ranges, rest)) = rest.split_first() else {
                    bail!("usage: {}", cmd.usage);
                };
                let (sign, r) = super::list_journal::parse_sign(ranges);
                for part in r.split(',') {
                    let range = bcachefs_kernel::bbpos_range_parse(part)
                        .map_err(|e| anyhow!("{e}: {part}"))?;
                    f.key.ranges.push((sign, range));
                }
                f.filtering = true;
                args = rest;
            }
            _ => bail!("list_journal: unknown arg '{flag}' (supported: -k <range>)"),
        }
    }
    let interrupt: &dyn Fn() -> bool = &take_interrupt;
    super::list_journal::list_journal_run(fs.raw, &f, false, 0, u64::MAX, None,
                                          Some(interrupt))?;
    Ok(ControlFlow::Continue(String::new()))
}

fn run_line(repl: &mut Repl, line: &str) -> Result<ControlFlow<()>> {
    let args: Vec<&str> = line.split_whitespace().collect();
    let Some((&op, args)) = args.split_first() else {
        return Ok(ControlFlow::Continue(()));
    };

    let Some(cmd) = COMMANDS.iter()
        .find(|c| c.name == op || c.aliases.contains(&op)) else {
        bail!("unknown command '{op}' (try: help)");
    };

    if repl.nostart && !cmd.nostart_ok {
        bail!("--nostart: the btree isn't started, only sb commands are available");
    }
    if cmd.needs_rw && !repl.rw {
        bail!("read-only (the default is norecovery): reopen with --rw to edit");
    }
    if cmd.needs_journal && !repl.journal {
        bail!("{}: reopen with --journal (retains the whole journal in memory)", cmd.name);
    }

    match (cmd.handler)(repl, cmd, args)? {
        ControlFlow::Continue(out) => {
            print!("{out}");
            Ok(ControlFlow::Continue(()))
        }
        ControlFlow::Break(()) => Ok(ControlFlow::Break(())),
    }
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
        opt_set!(fs_opts, will_not_start, 1);
    }
    // Whatever repair the superblock has scheduled is not what we came for:
    // check_allocations is pass 5, so recovery_pass_last below is a ceiling it
    // sits under, not a filter that excludes it - a filesystem carrying an
    // upgrade's scheduled passes rebuilds accounting before the first command
    // runs. It stays scheduled, and runs at the next real mount.
    opt_set!(fs_opts, recovery_passes_skip_scheduled, 1);
    if cli.rw && cli.norecovery {
        bail!("--rw and --norecovery are mutually exclusive");
    }
    // Inspection is the primary use, and a full-recovery open can repair -
    // rewrite - the state under inspection; rw is opt-in. Not for --nostart:
    // nothing recovers there anyway, and the nochanges norecovery implies
    // would turn sb set into a silent no-op.
    if !cli.rw && !cli.nostart {
        opt_set!(fs_opts, norecovery, 1);
    } else {
        // Go read-write, and stop there. Everything through journal_replay is
        // what makes the filesystem writable and consistent to write to -
        // bch2_fs_read_write() refuses to go rw with recovery_pass_last below
        // it. Everything after is repair, and repair is the one thing an
        // injection must not have: the passes run at open, before the first
        // command, so they rewrite the state we were about to construct. (A
        // snapshot deletion that consumed the very node a test had staged is
        // what prompted this.) Whatever the superblock still requires stays
        // recorded and runs at the next real mount.
        opt_set!(fs_opts, recovery_pass_last, c::bch_recovery_pass::BCH_RECOVERY_PASS_journal_replay as u8);
    }
    if cli.journal {
        opt_set!(fs_opts, retain_recovery_info, 1);
        opt_set!(fs_opts, read_entire_journal, 1);
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
    let mut repl = Repl {
        fs,
        nostart: cli.nostart,
        journal: cli.journal,
        rw: cli.rw,
        snapshot: None,
    };

    if !cli.commands.is_empty() {
        for line in &cli.commands {
            if run_line(&mut repl, line)?.is_break() {
                break;
            }
        }
        return Ok(());
    }

    // In a piped script an error must abort (a test's later commands likely
    // depend on earlier ones); interactively, report and go on.
    if !stdin().is_terminal() {
        for line in stdin().lines() {
            if run_line(&mut repl, &line?)?.is_break() {
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
        let prompt = match repl.snapshot {
            Some(s) => format!("kvdb[{s}]> "),
            None => "kvdb> ".to_string(),
        };
        match rl.readline(&prompt) {
            Ok(line) => {
                if !line.trim().is_empty() {
                    let _ = rl.add_history_entry(&line);
                }
                INTERRUPTED.store(false, Ordering::Relaxed);
                match run_line(&mut repl, &line) {
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
