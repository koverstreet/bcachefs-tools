// SPDX-License-Identifier: GPL-2.0

//! linux_version_cfgs KERNELVERSION OUT SRCDIR...
//!
//! The Rust's kernel version checks: for each cfg(linux_ge = "V") in the
//! SRCDIRs' Rust - not under a directory named vendor - `--cfg linux_ge="V"`
//! if the kernel is V or later, in OUT, an argfile for rustc's @OUT: one
//! argument a line. Written only when what's in it changes, so what takes it
//! is rebuilt only then.
//!
//! V is "X.Y" - from X.Y-rc1, where a merge window's changes first appear, as
//! C's KERNEL_VERSION(X, Y, 0) - or "X.Y-rcN", from that rc, for a change that
//! came after rc1, which C's can't say. Neither can tell a kernel in X.Y's
//! merge window from X.(Y-1): it's still versioned that.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::exit;

/// A version as a sortable key: major, minor, rc - @release, without one. A
/// linux_ge's, @exact, is X.Y or X.Y-rcN; a kernel's is X.Y[...][-rcN...].
fn key(v: &str, release: u32, exact: bool) -> Option<(u32, u32, u32)> {
    let num = |s: &str| match exact {
        true  => s.bytes().all(|b| b.is_ascii_digit()).then(|| s.parse().ok())?,
        false => s.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok(),
    };
    let (xy, rc) = v.split_once("-rc").map_or((v, None), |(xy, rc)| (xy, Some(rc)));
    let (x, y) = xy.split_once('.')?;
    Some((num(x)?, num(y)?, rc.map_or(Some(release), num)?))
}

/// Every V of a `linux_ge = "V"` in @src, a line of Rust that isn't a comment.
fn versions_in(src: &str, found: &mut BTreeSet<String>) {
    for line in src.lines().filter(|l| !l.trim_start().starts_with("//")) {
        let mut rest = line;
        while let Some(i) = rest.find("linux_ge") {
            rest = &rest[i + "linux_ge".len()..];
            let Some(v) = rest.trim_start().strip_prefix('=').map(str::trim_start)
                .and_then(|r| r.strip_prefix('"')) else { continue };
            if let Some(end) = v.find('"') {
                found.insert(v[..end].to_owned());
            }
        }
    }
}

/// The Rust under @dir, and its subdirectories but those named vendor.
fn scan(dir: &Path, found: &mut BTreeSet<String>) -> Result<(), String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("reading {}: {e}", dir.display()))?;
    for entry in entries {
        let path = entry.map_err(|e| format!("reading {}: {e}", dir.display()))?.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n != "vendor") {
                scan(&path, found)?;
            }
        } else if path.extension().is_some_and(|e| e == "rs") {
            let src = fs::read_to_string(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
            versions_in(&src, found);
        }
    }
    Ok(())
}

fn cfgs(kv: &str, dirs: &[String]) -> Result<String, String> {
    let k = key(kv, u32::MAX, false).ok_or_else(|| format!("kernel version \"{kv}\" isn't X.Y..."))?;

    let mut found = BTreeSet::new();
    for d in dirs {
        scan(Path::new(d), &mut found)?;
    }

    let mut out = String::new();
    for v in &found {
        if key(v, 0, true).ok_or_else(|| format!("linux_ge = \"{v}\" isn't X.Y or X.Y-rcN"))? <= k {
            out += &format!("--cfg\nlinux_ge=\"{v}\"\n");
        }
    }
    Ok(out)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [kv, out, dirs @ ..] = &args[..] else {
        eprintln!("usage: linux_version_cfgs KERNELVERSION OUT SRCDIR...");
        exit(2);
    };
    let cfgs = cfgs(kv, dirs).unwrap_or_else(|e| {
        eprintln!("linux_version_cfgs: {e}");
        exit(1);
    });
    if fs::read_to_string(out).ok().as_deref() != Some(&*cfgs) {
        if let Err(e) = fs::write(out, &cfgs) {
            eprintln!("linux_version_cfgs: writing {out}: {e}");
            exit(1);
        }
    }
}
