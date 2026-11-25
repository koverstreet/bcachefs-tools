// SPDX-License-Identifier: GPL-2.0

//! dkms-install DEST [NAME=VALUE...]: the module's source tree in DEST, for
//! make install_dkms and dkms-reload-interactive - what the module build
//! needs, a directory at a time:
//!
//!  - fs/ and os/, at DEST/src/fs/bcachefs and DEST/src/fs/os: what git has or
//!    would have - tracked, or untracked and not ignored - or, outside a git
//!    tree (a release tarball, made of what git tracked), all but dotfiles and
//!    objects;
//!  - the crates fs/Makefile.rust builds from crates.io and git - fs/Cargo.toml's
//!    package.metadata.dkms.crates - whole, from wherever cargo has the source
//!    of the version fs/ depends on: its registry, a git checkout, or what its
//!    source replacement says (a vendored tarball's vendor/, a distro's
//!    registry); at DEST/src/fs/bcachefs/vendor/NAME;
//!  - dkms/Makefile, at DEST;
//!  - version.h, with fs/;
//!  - the module signing CA, in fs/scripts;
//!  - DEST/build.vars: NAME := VALUE, for each NAME=VALUE.
//!
//! Source mtimes are kept, so kbuild rebuilds only what changed.

use serde::Deserialize;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, exit};

type Result<T> = std::result::Result<T, String>;

/// What we read of `cargo metadata`.
#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
    resolve: Resolve,
}

#[derive(Deserialize)]
struct Package {
    id: String,
    name: String,
    manifest_path: PathBuf,
    metadata: Option<PackageMetadata>,
}

#[derive(Deserialize)]
struct PackageMetadata {
    dkms: Option<Dkms>,
}

#[derive(Deserialize)]
struct Dkms {
    crates: Vec<String>,
}

#[derive(Deserialize)]
struct Resolve {
    nodes: Vec<Node>,
}

#[derive(Deserialize)]
struct Node {
    id: String,
    deps: Vec<NodeDep>,
}

#[derive(Deserialize)]
struct NodeDep {
    pkg: String,
}

fn ctx<T>(r: io::Result<T>, what: impl FnOnce() -> String) -> Result<T> {
    r.map_err(|e| format!("{}: {e}", what()))
}

/// @src to @dst, its mode and mtime with it.
fn copy(src: &Path, dst: &Path) -> Result<()> {
    let what = || format!("copying {} to {}", src.display(), dst.display());
    if let Some(dir) = dst.parent() {
        ctx(fs::create_dir_all(dir), what)?;
    }
    ctx(fs::copy(src, dst), what)?;
    let mtime = ctx(fs::metadata(src).and_then(|m| m.modified()), what)?;
    ctx(fs::File::options().write(true).open(dst).and_then(|f| f.set_modified(mtime)), what)
}

/// The files of @top's @dir, relative to @top: see the top of the file.
fn files(top: &Path, dir: &str) -> Result<Vec<PathBuf>> {
    if !top.join(".git").exists() {
        let mut files = Vec::new();
        walk(top, Path::new(dir), &mut files)?;
        return Ok(files);
    }

    // safe.directory: install_dkms runs as root, in a user's tree
    let out = ctx(Command::new("git")
        .arg("-C").arg(top)
        .arg("-c").arg(format!("safe.directory={}", top.display()))
        .args(["ls-files", "-z", "--cached", "--others", "--exclude-standard", "--", dir])
        .output(), || "running git ls-files".into())?;
    if !out.status.success() {
        return Err(format!("git ls-files {dir}: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }

    Ok(out.stdout.split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| PathBuf::from(OsStr::from_bytes(p)))
        // deleted, and not yet from the index
        .filter(|p| top.join(p).exists())
        .collect())
}

/// The files under @top's @dir, relative to @top, but dotfiles and objects.
fn walk(top: &Path, dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    let what = || format!("reading {}", top.join(dir).display());
    let entries = ctx(fs::read_dir(top.join(dir)), what)?;
    for e in entries {
        let e = ctx(e, what)?;
        let name = e.file_name();
        let path = dir.join(&name);
        let ty = ctx(e.file_type(), what)?;

        if name.as_bytes().starts_with(b".") {
            continue;
        }
        if ty.is_dir() {
            walk(top, &path, files)?;
        } else if !matches!(path.extension().and_then(OsStr::to_str), Some("o" | "d")) {
            files.push(path);
        }
    }
    Ok(())
}

/// fs/'s package.metadata.dkms.crates, and where cargo has each one's source.
fn crates(top: &Path) -> Result<Vec<(String, PathBuf)>> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let out = ctx(Command::new(cargo)
        .current_dir(top)
        .args(["metadata", "--format-version", "1", "--locked"])
        .output(), || "running cargo metadata".into())?;
    if !out.status.success() {
        return Err(format!("cargo metadata: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    let m: Metadata = serde_json::from_slice(&out.stdout).map_err(|e| format!("cargo metadata's output: {e}"))?;

    let pkg = |id: &str| m.packages.iter().find(|p| p.id == id);
    let fs = m.packages.iter().find(|p| p.name == "bcachefs-kernel").ok_or("cargo metadata: no bcachefs-kernel")?;
    let wanted = fs.metadata.as_ref().and_then(|m| m.dkms.as_ref())
        .ok_or("fs/Cargo.toml: no package.metadata.dkms")?;
    let deps: Vec<&Package> = m.resolve.nodes.iter()
        .find(|n| n.id == fs.id).ok_or("cargo metadata: bcachefs-kernel not in the resolve graph")?
        .deps.iter().filter_map(|d| pkg(&d.pkg)).collect();

    wanted.crates.iter()
        .map(|name| deps.iter().find(|p| &p.name == name)
             .and_then(|p| Some((name.clone(), p.manifest_path.parent()?.to_path_buf())))
             .ok_or_else(|| format!("{name}: not a dependency of bcachefs-kernel")))
        .collect()
}

fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let dest = PathBuf::from(args.next().ok_or("usage: dkms-install DEST [NAME=VALUE...]")?);
    let vars: String = args
        .map(|v| v.split_once('=').map(|(n, v)| format!("{n} := {v}\n"))
             .ok_or_else(|| format!("{v}: not NAME=VALUE")))
        .collect::<Result<_>>()?;

    let top = Path::new(env!("CARGO_MANIFEST_DIR")).parent().ok_or("dkms/ has no parent")?;
    let src = dest.join("src/fs");

    for (dir, to) in [("fs", "bcachefs"), ("os", "os")] {
        for f in files(top, dir)? {
            let rel = f.strip_prefix(dir).map_err(|_| format!("{}: not in {dir}/", f.display()))?;
            copy(&top.join(&f), &src.join(to).join(rel))?;
        }
    }

    for (name, dir) in crates(top)? {
        let to = src.join("bcachefs/vendor").join(&name);
        // an older version's files mustn't stay
        if to.exists() {
            ctx(fs::remove_dir_all(&to), || format!("removing {}", to.display()))?;
        }
        let mut files = Vec::new();
        walk(&dir, Path::new(""), &mut files)?;
        for f in files {
            copy(&dir.join(&f), &to.join(&f))?;
        }
    }

    for (from, to) in [
        ("dkms/Makefile", dest.join("Makefile")),
        ("version.h", src.join("bcachefs/version.h")),
        ("signing/bcachefs-signing-ca.pem", src.join("bcachefs/scripts/bcachefs-signing-ca.pem")),
    ] {
        copy(&top.join(from), &to)?;
    }

    let build_vars = dest.join("build.vars");
    ctx(fs::write(&build_vars, vars), || format!("writing {}", build_vars.display()))
}

fn main() {
    if let Err(e) = run() {
        eprintln!("dkms-install: {e}");
        exit(1);
    }
}
