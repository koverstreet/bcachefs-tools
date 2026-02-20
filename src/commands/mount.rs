use std::{
    ffi::{CString, OsString},
    io::{stdout, IsTerminal},
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
    ptr, str,
};

use anyhow::{ensure, Result};
use bch_bindgen::{bcachefs, bcachefs::bch_sb_handle, path_to_cstr};
use clap::Parser;
use log::{debug, error, info};
use crate::device_scan;

use crate::{
    key::{KeyHandle, Keyring, Passphrase, UnlockPolicy},
    logging,
};

fn mount_inner(
    src: OsString,
    target: &std::path::Path,
    fstype: &str,
    mut mountflags: libc::c_ulong,
    data: Option<String>,
) -> anyhow::Result<()> {
    // bind the CStrings to keep them alive
    let c_src = CString::new(src.clone().into_vec())?;
    let c_target = path_to_cstr(target);
    let data = data.map(CString::new).transpose()?;
    let fstype = CString::new(fstype)?;

    // convert to pointers for ffi
    let c_src = c_src.as_ptr();
    let c_target = c_target.as_ptr();
    let data_ptr = data.as_ref().map_or(ptr::null(), |data| data.as_ptr().cast());
    let fstype = fstype.as_ptr();

    let mut ret;
    loop {
        ret = {
            info!("mounting filesystem");
            // REQUIRES: CAP_SYS_ADMIN
            unsafe { libc::mount(c_src, c_target, fstype, mountflags, data_ptr) }
        };

        let err = errno::errno().0;

        if ret == 0
            || (err != libc::EACCES && err != libc::EROFS)
            || (mountflags & libc::MS_RDONLY) != 0
        {
            break;
        }

        println!("mount: device write-protected, mounting read-only");
        mountflags |= libc::MS_RDONLY;
    }

    drop(data);

    if ret != 0 {
        let err = errno::errno();
        let e = crate::ErrnoError(err);

        if err.0 == libc::EBUSY {
            eprintln!("mount: {}: {:?} already mounted or mount point busy", target.to_string_lossy(), src);
        } else {
            eprintln!("mount: {:?}: {}", src, e);
        }

        Err(e.into())
    } else {
        Ok(())
    }
}

/// Parse a comma-separated mount options and split out mountflags and filesystem
/// specific options.
fn parse_mountflag_options(options: impl AsRef<str>) -> (Option<String>, libc::c_ulong) {
    use either::Either::{Left, Right};

    debug!("parsing mount options: {}", options.as_ref());
    let (opts, flags) = options
        .as_ref()
        .split(',')
        .map(|o| match o {
            "dirsync" => Left(libc::MS_DIRSYNC),
            "lazytime" => Left(1 << 25), // MS_LAZYTIME
            "mand" => Left(libc::MS_MANDLOCK),
            "noatime" => Left(libc::MS_NOATIME),
            "nodev" => Left(libc::MS_NODEV),
            "nodiratime" => Left(libc::MS_NODIRATIME),
            "noexec" => Left(libc::MS_NOEXEC),
            "nosuid" => Left(libc::MS_NOSUID),
            "relatime" => Left(libc::MS_RELATIME),
            "remount" => Left(libc::MS_REMOUNT),
            "ro" => Left(libc::MS_RDONLY),
            "rw" | "" => Left(0),
            "strictatime" => Left(libc::MS_STRICTATIME),
            "sync" => Left(libc::MS_SYNCHRONOUS),
            // Userspace-only fstab options — not passed to the kernel
            "auto" | "noauto" | "nofail" | "_netdev" |
            "user" | "nouser" | "users" | "group" | "owner" => Left(0),
            o if o.starts_with("x-") || o.starts_with("comment=") => Left(0),
            o => Right(o),
        })
        .fold((Vec::new(), 0), |(mut opts, flags), next| match next {
            Left(f) => (opts, flags | f),
            Right(o) => {
                opts.push(o);
                (opts, flags)
            }
        });

    (
        if opts.is_empty() {
            None
        } else {
            Some(opts.join(","))
        },
        flags,
    )
}

/// If a user explicitly specifies `unlock_policy` or `passphrase_file` then use
/// that without falling back to other mechanisms. If these options are not
/// used, then search for the key or ask for it.
fn handle_unlock(cli: &Cli, sb: &bch_sb_handle) -> Result<KeyHandle> {
    if let Some(policy) = cli.unlock_policy.as_ref() {
        return policy.apply(sb);
    }

    if let Some(path) = cli.passphrase_file.as_deref() {
        return Passphrase::new_from_file(path).and_then(|p| KeyHandle::new(sb, &p, Keyring::User));
    }

    let uuid = sb.sb().uuid();
    KeyHandle::new_from_search(&uuid)
        .or_else(|_| Passphrase::new(&uuid).and_then(|p| KeyHandle::new(sb, &p, Keyring::User)))
}

fn cmd_mount_inner(cli: &Cli) -> Result<()> {
    let (optstr, mountflags) = parse_mountflag_options(&cli.options);
    let opts = bch_bindgen::opts::parse_mount_opts(None, optstr.as_deref(), true)
        .unwrap_or_default();

    let sbs = device_scan::scan_sbs(&cli.dev, &opts)?;

    ensure!(!sbs.is_empty(), "No device(s) to mount specified");

    let devices = device_scan::joined_device_str(&sbs);

    let first_sb = &sbs[0].1;
    if unsafe { bcachefs::bch2_sb_is_encrypted(first_sb.sb) } {
        handle_unlock(cli, first_sb)?;
    }

    drop(sbs);

    if let Some(mountpoint) = cli.mountpoint.as_deref() {
        info!(
            "mounting with params: device: {:?}, target: {}, options: {}",
            devices,
            mountpoint.to_string_lossy(),
            &cli.options
        );

        mount_inner(devices, mountpoint, "bcachefs", mountflags, optstr)
    } else {
        info!(
            "would mount with params: device: {:?}, options: {}",
            devices, &cli.options
        );

        Ok(())
    }
}

/// Mount a bcachefs filesystem by its UUID.
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    /// Path to passphrase file
    ///
    /// This can be used to optionally specify a file to read the passphrase
    /// from. An explictly specified key_location/unlock_policy overrides this
    /// argument.
    #[arg(short = 'f', long)]
    passphrase_file: Option<PathBuf>,

    /// Passphrase policy to use in case of an encrypted filesystem. If not
    /// specified, the password will be searched for in the keyring. If not
    /// found, the password will be prompted or read from stdin, depending on
    /// whether the stdin is connected to a terminal or not.
    #[arg(short = 'k', long = "key_location", value_enum)]
    unlock_policy: Option<UnlockPolicy>,

    /// Device, or UUID=\<UUID\>
    dev: String,

    /// Where the filesystem should be mounted. If not set, then the filesystem
    /// won't actually be mounted. But all steps preceeding mounting the
    /// filesystem (e.g. asking for passphrase) will still be performed.
    mountpoint: Option<PathBuf>,

    /// Mount options
    #[arg(short, default_value = "")]
    options: String,

    #[arg(short = 't', long = "type", default_value = "")]
    fs_type: String,

    // FIXME: would be nicer to have `--color[=WHEN]` like diff or ls?
    /// Force color on/off. Autodetect tty is used to define default:
    #[arg(short, long, action = clap::ArgAction::Set, default_value_t=stdout().is_terminal())]
    colorize: bool,

    /// Verbose mode
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

fn check_bcachefs_module() -> bool {
    let path = Path::new("/sys/module/bcachefs");

    path.exists() || {
        let _ = std::process::Command::new("modprobe")
            .arg("bcachefs")
            .status();
        path.exists()
    }
}

pub fn mount(mut argv: Vec<String>, symlink_cmd: Option<&str>) -> std::process::ExitCode {
    let module_loaded = check_bcachefs_module();

    // If the bcachefs tool is being called as "bcachefs mount dev ..." (as opposed to via a
    // symlink like "/usr/sbin/mount.bcachefs dev ...", then we need to pop the 0th argument
    // ("bcachefs") since the CLI parser here expects the device at position 1.
    if symlink_cmd.is_none() {
        argv.remove(0);
    }

    let cli = Cli::parse_from(argv.clone());

    if cli.fs_type == "bcachefs.fuse" {
        // Build argv for fusemount from mount's parsed args
        let mut fuse_argv = vec!["fusemount".to_string()];
        if !cli.options.is_empty() {
            fuse_argv.push("-o".to_string());
            fuse_argv.push(cli.options.clone());
        }
        fuse_argv.push(cli.dev.clone());
        if let Some(mp) = cli.mountpoint.as_ref() {
            fuse_argv.push(mp.to_string_lossy().into_owned());
        }
        return match super::fusemount::cmd_fusemount(fuse_argv) {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                error!("FUSE mount failed: {e}");
                std::process::ExitCode::FAILURE
            }
        };
    }

    // TODO: centralize this on the top level CLI
    logging::setup(cli.verbose, cli.colorize);

    match cmd_mount_inner(&cli) {
        Ok(_)   => std::process::ExitCode::SUCCESS,
        Err(e)   => {
            error!("Mount failed for {}: {e}", cli.dev);
            if !module_loaded {
                error!("bcachefs module not loaded?");
            }
            std::process::ExitCode::FAILURE
        }
    }
}
