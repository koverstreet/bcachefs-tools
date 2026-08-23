//! A drop-in raising the mount timeout, written before units are loaded.
//! A mount helper can't raise it from the inside; see the commit for why.
//!
//! Two constraints on anything added here. All generators share one 90-second
//! budget and overrunning kills the whole batch, so nothing in here may probe a
//! device. And drop-in precedence sorts by basename across every directory, not
//! within one - the 10- prefix is load-bearing.
//!
//! Ordering against systemd-fstab-generator doesn't matter: drop-ins are merged
//! when a unit is loaded, after every generator has exited, and we read the same
//! fstab rather than its output.
//!
//! Only fstab entries. Generators can't enumerate units, so a hand-written
//! .mount or a mount(8) from a shell isn't covered - both have somebody at a
//! terminal. An entry with its own x-systemd.mount-timeout= is left alone.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;
use log::{debug, warn};

use crate::logging;

/// No ceiling, as systemd-fsck@.service does it. A recovery making progress
/// must not be killed for taking a long time - and if one is genuinely wedged,
/// hanging visibly is a better failure than an unbootable system whose symptom
/// points somewhere else.
const TIMEOUT: &str = "infinity";

/// systemd's UNIT_NAME_MAX, from src/basic/unit-name.h. At this length a name
/// stops being escaped and starts being hashed - see unit_name().
const UNIT_NAME_MAX: usize = 256;

/// systemd's unit-name escaping for a path, as unit_name_path_escape() does it:
/// simplify, drop leading and trailing slashes, '/' becomes '-', and anything
/// that isn't a valid unit-name character - including a literal '-' - becomes
/// \xNN. The root directory is "-".
///
/// Done here rather than by shelling out to systemd-escape because a generator
/// runs in early boot, where that binary may not be present.
/// `None` for a path systemd itself refuses. unit_name_path_escape() runs
/// path_is_normalized() first and returns -EINVAL for anything with a "." or
/// ".." component, so such an fstab line has no unit to attach a drop-in to and
/// writing one would leave a file nothing will ever read.
fn escape_path(path: &str) -> Option<String> {
    if path.split('/').any(|c| c == "." || c == "..") {
        return None;
    }

    let trimmed = path.trim_matches('/');

    if trimmed.is_empty() {
        return Some("-".to_string());
    }

    let mut out = String::with_capacity(trimmed.len());
    let mut prev_slash = false;

    for (i, c) in trimmed.chars().enumerate() {
        // A leading dot is escaped even though '.' is otherwise valid, as
        // systemd's do_escape() does: a unit file starting with one is hidden.
        if i == 0 && c == '.' {
            out.push_str("\\x2e");
            continue;
        }
        // Collapse repeated slashes, which path_simplify() would have removed.
        if c == '/' {
            if !prev_slash {
                out.push('-');
            }
            prev_slash = true;
            continue;
        }
        prev_slash = false;

        if c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '.') {
            out.push(c);
        } else {
            for b in c.to_string().as_bytes() {
                out.push_str(&format!("\\x{b:02x}"));
            }
        }
    }

    Some(out)
}

/// One fstab line's worth of what we care about.
struct Entry {
    target: String,
    opts:   String,
}

/// fstab, minus comments and blank lines.
fn parse_fstab(text: &str) -> Vec<Entry> {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let _source = f.next()?;
            let target = f.next()?;
            let fstype = f.next()?;
            let opts = f.next().unwrap_or("defaults");

            (fstype == "bcachefs").then(|| Entry {
                target: unescape_octal(target),
                opts:   opts.to_string(),
            })
        })
        .collect()
}

/// fstab escapes whitespace in paths as \0NN. Nothing else is escaped.
fn unescape_octal(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;

    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() {
            if let Some(v) = std::str::from_utf8(&b[i + 1..i + 4])
                .ok()
                .and_then(|o| u8::from_str_radix(o, 8).ok())
            {
                out.push(v as char);
                i += 4;
                continue;
            }
        }
        out.push(b[i] as char);
        i += 1;
    }

    out
}

fn has_own_timeout(opts: &str) -> bool {
    opts.split(',').any(|o| o.starts_with("x-systemd.mount-timeout="))
}

/// The name systemd will give this mount's unit.
///
/// systemd-fstab-generator resolves the mount point before escaping it -
/// canonicalize_mount_path() at fstab-generator.c, which chases symlinks with
/// CHASE_NONEXISTENT - so /mnt/data pointing at /srv/data becomes
/// srv-data.mount. Escaping the raw fstab text instead would put the drop-in
/// beside a unit that doesn't exist: right directory, exit 0, no effect, and
/// nothing says so until somebody eats the timeout it was meant to remove.
///
/// A mount point that isn't there yet can't be resolved, which at generator
/// time is most of them; then the raw path is the best guess available and is
/// what systemd's own chase() falls back to for the part that doesn't exist.
fn unit_name(target: &str) -> Option<String> {
    let resolved = fs::canonicalize(target)
        .ok()
        .and_then(|p| p.to_str().map(str::to_owned));

    let escaped = escape_path(resolved.as_deref().unwrap_or(target))?;

    // Past UNIT_NAME_MAX systemd stops escaping and starts hashing:
    // unit_name_hash_long() truncates and appends siphash24 under a fixed key.
    // A hash we get subtly wrong is worse than none, so refuse and say so.
    if escaped.len() + ".mount".len() >= UNIT_NAME_MAX {
        warn!("generator: {target} escapes to a unit name systemd would hash, \
               which we can't reproduce - its mount timeout is NOT raised. \
               Set x-systemd.mount-timeout=infinity on the fstab entry.");
        return None;
    }

    Some(escaped)
}

fn write_dropin(normal_dir: &Path, e: &Entry) -> Result<()> {
    let Some(unit) = unit_name(&e.target) else {
        debug!("generator: {} is not a normalized path, as systemd wants it", e.target);
        return Ok(());
    };

    let dir = normal_dir.join(format!("{unit}.mount.d"));
    fs::create_dir_all(&dir)?;

    let mut f = fs::File::create(dir.join("10-bcachefs-timeout.conf"))?;
    writeln!(
        f,
        "# Written by bcachefs's systemd generator.\n\
         #\n\
         # A recovery can run for much longer than DefaultTimeoutStartSec, and a\n\
         # mount helper has no way to ask for more time - mount units get no\n\
         # NOTIFY_SOCKET, so EXTEND_TIMEOUT_USEC= is unreachable. Killing a\n\
         # recovery that is making progress only means doing it again from the\n\
         # start on the next boot, and never finishing.\n\
         #\n\
         # Set x-systemd.mount-timeout= on the fstab entry to override.\n\
         [Mount]\n\
         TimeoutSec={TIMEOUT}"
    )?;

    Ok(())
}

/// systemd calls a generator with three directories - normal, early, late - and
/// ignores its output. Failing here must never fail the boot, so everything is
/// a warning and we keep going: a filesystem that mounts slowly is a much
/// better outcome than one that doesn't mount at all.
fn cmd_generator(argv: Vec<String>) -> Result<()> {
    // No colour: a generator's output goes to the journal, not a terminal.
    logging::setup(0, false);

    let Some(normal_dir) = argv.get(1).map(PathBuf::from) else {
        warn!("generator: no output directory given");
        return Ok(());
    };

    let text = match fs::read_to_string("/etc/fstab") {
        Ok(t) => t,
        Err(e) => {
            debug!("generator: no fstab to read: {e}");
            return Ok(());
        }
    };

    for e in parse_fstab(&text) {
        if has_own_timeout(&e.opts) {
            debug!("generator: {} sets its own timeout, leaving it", e.target);
            continue;
        }

        match write_dropin(&normal_dir, &e) {
            Ok(())  => debug!("generator: TimeoutSec={TIMEOUT} for {}", e.target),
            Err(er) => warn!("generator: no drop-in for {}: {er}", e.target),
        }
    }

    Ok(())
}

fn run(argv: Vec<String>) -> std::process::ExitCode {
    let _ = cmd_generator(argv);
    std::process::ExitCode::SUCCESS
}

pub const CMD: super::CmdDef =
    raw_cmd!("systemd-generator",
             "systemd generator: raise the mount timeout for bcachefs filesystems",
             run);
