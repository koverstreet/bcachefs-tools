use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use bch_bindgen::c;
use clap::{Arg, ArgAction, Command};
use rustix::fs::{XattrFlags, setxattr, removexattr};

use super::opts;

const BCHFS_IOC_REINHERIT_ATTRS: libc::c_ulong = 0x8008bc40;
const BCHFS_IOC_SET_REFLINK_P_MAY_UPDATE_OPTS: libc::c_ulong = 0xbc41;
const BCHFS_IOC_PROPAGATE_REFLINK_P_OPTS: libc::c_ulong = 0xbc42;

/// Call a no-argument ioctl, returning io::Result.
fn ioctl_none(fd: i32, request: libc::c_ulong) -> std::io::Result<()> {
    if unsafe { libc::ioctl(fd, request) } < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn propagate_recurse(dir_path: &Path) {
    let inner = || -> std::io::Result<()> {
        let dir = std::fs::File::open(dir_path)?;
        for entry in std::fs::read_dir(dir_path)?.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_symlink() { continue }
            let Ok(name) = CString::new(entry.file_name().as_bytes().to_vec()) else { continue };

            let ret = unsafe { libc::ioctl(dir.as_raw_fd(), BCHFS_IOC_REINHERIT_ATTRS, name.as_ptr()) };
            if ret < 0 {
                eprintln!("{}: {}", entry.path().display(), std::io::Error::last_os_error());
                continue;
            }
            if ret == 0 || !ft.is_dir() { continue }
            propagate_recurse(&entry.path());
        }
        Ok(())
    };
    if let Err(e) = inner() {
        eprintln!("{}: {e}", dir_path.display());
    }
}

fn remove_bcachefs_attr(path: &Path, attr_name: &str) {
    if let Err(e) = removexattr(path, attr_name) {
        if e != rustix::io::Errno::NODATA && e != rustix::io::Errno::INVAL {
            eprintln!("error removing attribute {} from {}: {}",
                attr_name, path.display(), e);
        }
    }
}

fn do_setattr(path: &Path, opts: &[(String, String)], remove_all: bool) -> Result<()> {
    if remove_all {
        for name in opts::bch_option_names(c::opt_flags::OPT_INODE as u32) {
            // casefold only works on empty directories
            if name == "casefold" { continue }
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

    if std::fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .is_dir()
    {
        propagate_recurse(path);
    }
    Ok(())
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
        .args(opts::bch_option_args(c::opt_flags::OPT_INODE as u32))
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
    let opts = opts::bch_options_from_matches(&matches, c::opt_flags::OPT_INODE as u32);
    let files: Vec<&String> = matches.get_many("files").unwrap().collect();

    for path in files {
        do_setattr(Path::new(path), &opts, remove_all)?;
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
    let fd = file.as_raw_fd();

    if set_may_update {
        ioctl_none(fd, BCHFS_IOC_SET_REFLINK_P_MAY_UPDATE_OPTS)
            .context("set may_update_opts")?;
    }

    ioctl_none(fd, BCHFS_IOC_PROPAGATE_REFLINK_P_OPTS).map_err(|e| {
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
pub const CMD_REFLINK_PROPAGATE: super::CmdDef = raw_cmd!("reflink-option-propagate", "Propagate options to reflinked files", cmd_reflink_option_propagate);
