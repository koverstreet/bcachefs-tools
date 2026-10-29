use std::ffi::{CStr, OsStr};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use bch_bindgen::c;
use clap::{Arg, ArgAction, Command};
use rustix::fs::{openat, removexattr, setxattr, statx, AtFlags, Dir, FileType, Mode, OFlags, StatxFlags,
                 XattrFlags};

use super::opts;

use crate::wrappers::ioctl::{ioctl_none, Ioctl,
    BCHFS_IOC_PROPAGATE_REFLINK_P_OPTS, BCHFS_IOC_REINHERIT_ATTRS,
    BCHFS_IOC_SET_REFLINK_P_MAY_UPDATE_OPTS};

/// Each child of @dir, at @path, takes on again what it inherits from @dir,
/// and each subdirectory's children from it, down the tree: how many failed,
/// each reported. Walked by fd - children named relative to their parent - so
/// the directory reinherited is the one descended into. Not into a mount
/// below: BCHFS_IOC_REINHERIT_ATTRS resolves a name in the dirent btree, so
/// it saw the directory the mount covers.
fn propagate_recurse(dir: &OwnedFd, path: &Path) -> usize {
    let walk = || -> rustix::io::Result<usize> {
        let mnt_id = statx(dir, "", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID)?.stx_mnt_id;
        let mut failed = 0;
        let report = |name: &CStr, e: &dyn std::fmt::Display| {
            eprintln!("{}: {e}", path.join(OsStr::from_bytes(name.to_bytes())).display());
            1
        };

        let mut entries = Dir::read_from(dir)?;
        while let Some(entry) = entries.read() {
            let entry = entry?;
            let (name, ty) = (entry.file_name(), entry.file_type());
            if matches!(name.to_bytes(), b"." | b"..") || ty == FileType::Symlink {
                continue;
            }

            // The argument is the name's address itself, not a pointer to it
            // SAFETY: a NUL-terminated name, which the kernel only reads
            let ret = unsafe {
                libc::ioctl(dir.as_raw_fd(), BCHFS_IOC_REINHERIT_ATTRS::OPCODE as libc::Ioctl, name.as_ptr())
            };
            if ret < 0 {
                failed += report(name, &std::io::Error::last_os_error());
                continue;
            }
            if ty != FileType::Directory {
                continue;
            }

            match statx(dir, name, AtFlags::SYMLINK_NOFOLLOW | AtFlags::NO_AUTOMOUNT, StatxFlags::MNT_ID) {
                Ok(stx) if stx.stx_mnt_id != mnt_id => continue,
                Ok(_) => {}
                Err(e) => {
                    failed += report(name, &e);
                    continue;
                }
            }
            match openat(dir, name, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                         Mode::empty()) {
                Ok(child) => {
                    let child_path = path.join(OsStr::from_bytes(name.to_bytes()));
                    failed += propagate_recurse(&child, &child_path);
                }
                Err(e) => failed += report(name, &e),
            }
        }
        Ok(failed)
    };
    walk().unwrap_or_else(|e| {
        eprintln!("{}: {e}", path.display());
        1
    })
}

fn remove_bcachefs_attr(path: &Path, attr_name: &str) {
    if let Err(e) = removexattr(path, attr_name) {
        if e != rustix::io::Errno::NODATA && e != rustix::io::Errno::INVAL {
            eprintln!("error removing attribute {} from {}: {}",
                attr_name, path.display(), e);
        }
    }
}

/// @opts set on @path, and if it's a directory, reinherited down it: how
/// many of its descendants failed to.
fn do_setattr(path: &Path, opts: &[(String, String)], remove_all: bool) -> Result<usize> {
    let is_dir = std::fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .is_dir();

    if remove_all {
        let nonempty_dir = is_dir &&
            std::fs::read_dir(path)
                .with_context(|| format!("reading directory {}", path.display()))?
                .next().is_some();

        for name in opts::bch_option_names(c::opt_flags::OPT_INODE.bits()) {
            // These change how a directory's names hash: the kernel refuses
            // to change them on a nonempty directory, and casefold is
            // directory-only
            if nonempty_dir && (name == "casefold" || name == "inodes_32bit") { continue }
            if !is_dir && name == "casefold" { continue }
            remove_bcachefs_attr(path, &format!("bcachefs.{}", name));
        }
    }

    for (name, value) in opts {
        let attr = format!("bcachefs.{}", name);

        if value == "-" {
            remove_bcachefs_attr(path, &attr);
        } else {
            setxattr(path, &attr, value.as_bytes(), XattrFlags::empty())
                .with_context(|| format!("setting {} on {}", attr, path.display()))?;
        }
    }

    if !is_dir {
        return Ok(0);
    }
    let dir = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    Ok(propagate_recurse(&dir.into(), path))
}

fn read_bcachefs_attr(path: &Path, attr: &str) -> Result<Option<String>> {
    use rustix::fs::getxattr;
    use rustix::io::Errno;

    // "no value here" rather than an error: NODATA (option unset), NOTSUP (not
    // a bcachefs filesystem), INVAL (not a recognized option).
    let read = |buf: &mut [u8]| -> Result<Option<usize>> {
        match getxattr(path, attr, buf) {
            Ok(len) => Ok(Some(len)),
            Err(Errno::NODATA | Errno::NOTSUP | Errno::INVAL) => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {attr} from {}", path.display())),
        }
    };

    // An empty buffer returns the attribute's size.
    let Some(len) = read(&mut [])? else { return Ok(None) };

    let mut buf = vec![0u8; len];
    let Some(len) = read(&mut buf)? else { return Ok(None) };

    buf.truncate(len);
    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
}

pub(super) fn setattr_cmd() -> Command {
    Command::new("set-file-option")
        .about("Set attributes on files in a bcachefs filesystem")
        .long_about("\
Sets per-file or per-directory IO path options, overriding the \
filesystem-wide defaults. See <<sec:io-path-options>> for the list \
of available IO path options. When set on a directory, options are \
propagated recursively to existing children and inherited by new files.\n\n\
Changed options take effect immediately for new writes. For existing \
data, reconcile applies the new options in the background---for example, \
setting a new compression algorithm will cause existing data to be \
rewritten with the new algorithm. Use --option=- to remove a specific \
option, or --remove-all to clear all per-file options.")
        .after_help("To remove a specific option, use: --option=-")
        .args(opts::bch_option_args(c::opt_flags::OPT_INODE.bits(), true))
        .arg(Arg::new("remove-all")
            .long("remove-all")
            .action(ArgAction::SetTrue)
            .help("Remove all file options"))
        .arg(Arg::new("files")
            .action(ArgAction::Append)
            .required(true))
}

fn cmd_setattr(argv: Vec<String>) -> Result<()> {
    let matches = setattr_cmd().get_matches_from(argv);

    let remove_all = matches.get_flag("remove-all");
    let opts = opts::bch_options_from_matches(&matches, c::opt_flags::OPT_INODE.bits());
    let mut failed = 0;
    for path in matches.get_many::<String>("files").unwrap() {
        failed += do_setattr(Path::new(path), &opts, remove_all)?;
    }
    if failed != 0 {
        bail!("{failed} files failed to inherit the new options");
    }
    Ok(())
}

pub(super) fn getattr_cmd() -> Command {
    Command::new("get-file-option")
        .about("Show file-level options")
        .long_about("\
Shows per-file or per-directory IO path options stored in a bcachefs \
filesystem. By default only explicitly set file options are printed. Use \
--effective to show inherited/effective options, or --all to include unset \
options.")
        .arg(Arg::new("effective")
            .long("effective")
            .short('e')
            .action(ArgAction::SetTrue)
            .help("Show inherited/effective file options"))
        .arg(Arg::new("all")
            .long("all")
            .short('a')
            .action(ArgAction::SetTrue)
            .help("Show unset options as '-'"))
        .arg(Arg::new("files")
            .action(ArgAction::Append)
            .required(true))
}

fn cmd_getattr(argv: Vec<String>) -> Result<()> {
    let matches = getattr_cmd().get_matches_from(argv);
    let effective = matches.get_flag("effective");
    let all = matches.get_flag("all");
    let prefix = if effective { "bcachefs_effective" } else { "bcachefs" };
    let files: Vec<&String> = matches.get_many("files").unwrap().collect();
    let names = opts::bch_option_names(c::opt_flags::OPT_INODE.bits());
    let multi_file = files.len() > 1;

    for file in files {
        let path = Path::new(file);
        for name in &names {
            let attr = format!("{prefix}.{name}");
            match read_bcachefs_attr(path, &attr)? {
                Some(value) => {
                    if multi_file {
                        println!("{file}\t{name}\t{value}");
                    } else {
                        println!("{name}\t{value}");
                    }
                }
                None if all => {
                    if multi_file {
                        println!("{file}\t{name}\t-");
                    } else {
                        println!("{name}\t-");
                    }
                }
                None => {}
            }
        }
    }

    Ok(())
}

pub(super) fn reflink_option_propagate_cmd() -> Command {
    Command::new("reflink-option-propagate")
        .about("Propagate IO options to reflinked extents")
        .long_about("\
Propagates each file's current IO options (compression, checksum, \
replicas, targets) to its extents, including indirect (reflinked) \
extents. Reflinked data is shared between files, so propagation is \
gated by the REFLINK_P_MAY_UPDATE_OPTIONS permission flag on each \
reflink pointer. When a reflink copy is created, the destination's \
pointer gets this flag only if the copying user owns the source \
file---this prevents unprivileged users from altering IO path \
settings on shared data they do not own.\n\n\
Old reflink pointers created before the flag was introduced lack it \
entirely. Use --set-may-update (requires CAP_SYS_ADMIN) to enable \
the flag on such pointers before propagating.")
        .arg(Arg::new("set-may-update")
            .long("set-may-update")
            .action(ArgAction::SetTrue)
            .help("Enable option propagation on old reflink_p extents that \
                   predate the may_update_options flag. Requires CAP_SYS_ADMIN. \
                   Only needed once per file for filesystems with reflinks \
                   created before the flag was introduced."))
        .arg(Arg::new("files")
            .action(ArgAction::Append)
            .required(true))
}

fn do_reflink_propagate(path: &str, set_may_update: bool) -> Result<()> {
    let file = std::fs::File::open(path)?;

    if set_may_update {
        ioctl_none::<BCHFS_IOC_SET_REFLINK_P_MAY_UPDATE_OPTS>(&file)
            .context("set may_update_opts")?;
    }

    ioctl_none::<BCHFS_IOC_PROPAGATE_REFLINK_P_OPTS>(&file).map_err(|e| {
        if e.raw_os_error() == Some(libc::EPERM) {
            anyhow!("reflink_p extents without may_update_options set;\n\
                     rerun as root with --set-may-update")
        } else {
            anyhow!(e).context("propagate reflink opts")
        }
    })?;

    Ok(())
}

fn cmd_reflink_option_propagate(argv: Vec<String>) -> Result<()> {
    let matches = reflink_option_propagate_cmd().get_matches_from(argv);

    let set_may_update = matches.get_flag("set-may-update");
    let files: Vec<&String> = matches.get_many("files").unwrap().collect();

    let mut errors = false;
    for path in &files {
        if let Err(e) = do_reflink_propagate(path, set_may_update) {
            eprintln!("{path}: {e:#}");
            errors = true;
        }
    }

    if errors {
        Err(anyhow!("some files had errors"))
    } else {
        Ok(())
    }
}

pub const CMD_SETATTR: super::CmdDef = raw_cmd!("set-file-option", "Set file-level options", cmd_setattr);
pub const CMD_GETATTR: super::CmdDef = raw_cmd!("get-file-option", "Show file-level options", cmd_getattr);
pub const CMD_REFLINK_PROPAGATE: super::CmdDef = raw_cmd!("reflink-option-propagate", "Propagate options to reflinked files", cmd_reflink_option_propagate);
