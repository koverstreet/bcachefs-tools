use std::{
    ffi::{CString, OsString},
    io::{stdout, IsTerminal},
    os::unix::ffi::OsStringExt,
    path::{Component, Path, PathBuf},
    ptr, str,
};

use crate::device_scan;
use anyhow::{bail, ensure, Context, Result};
use bcachefs_kernel::c::bch_sb_handle;
use bcachefs_kernel::path_to_cstr;
use clap::Parser;
use log::{debug, error, info};

use crate::{
    key::{KeyHandle, Keyring, Passphrase, UnlockPolicy},
    logging,
};

fn mount_inner(
    src: OsString,
    target: &std::path::Path,
    fstype: Option<&str>,
    mut mountflags: libc::c_ulong,
    data: Option<String>,
) -> anyhow::Result<()> {
    // bind the CStrings to keep them alive
    let c_src = CString::new(src.clone().into_vec())?;
    let c_target = path_to_cstr(target);
    let data = data.map(CString::new).transpose()?;
    let fstype = fstype.map(CString::new).transpose()?;

    // convert to pointers for ffi
    let c_src = c_src.as_ptr();
    let c_target = c_target.as_ptr();
    let data_ptr = data
        .as_ref()
        .map_or(ptr::null(), |data| data.as_ptr().cast());
    let fstype = fstype
        .as_ref()
        .map_or(ptr::null(), |fstype| fstype.as_ptr());

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
            eprintln!(
                "mount: {}: {:?} already mounted or mount point busy",
                target.to_string_lossy(),
                src
            );
        } else {
            eprintln!("mount: {:?}: {}", src, e);
        }

        Err(e.into())
    } else {
        Ok(())
    }
}

struct TempMount {
    path: PathBuf,
    mounted: bool,
}

impl TempMount {
    fn new() -> Result<Self> {
        let base = Path::new("/run/mount");
        let base = if base.is_dir() {
            base
        } else {
            Path::new("/tmp")
        };
        let pid = std::process::id();

        for i in 0..1000 {
            let path = base.join(format!("bcachefs-subvol.{pid}.{i}"));
            match std::fs::create_dir(&path) {
                Ok(()) => {
                    return Ok(Self {
                        path,
                        mounted: false,
                    })
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e).with_context(|| format!("creating {}", path.display())),
            }
        }

        bail!(
            "could not create temporary mountpoint under {}",
            base.display()
        )
    }

    fn umount(&mut self) -> Result<()> {
        if self.mounted {
            let c_path = path_to_cstr(&self.path);
            let ret = unsafe { libc::umount2(c_path.as_ptr(), libc::MNT_DETACH) };
            if ret != 0 {
                return Err(crate::ErrnoError(errno::errno()).into());
            }
            self.mounted = false;
        }

        std::fs::remove_dir(&self.path)
            .with_context(|| format!("removing {}", self.path.display()))?;
        Ok(())
    }
}

impl Drop for TempMount {
    fn drop(&mut self) {
        let _ = self.umount();
    }
}

fn parse_subvol_path(path: &str) -> Result<Option<PathBuf>> {
    let path = path.trim_start_matches('/');
    if path.is_empty() {
        return Ok(None);
    }

    let mut normalized = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir => bail!("subvol= path must not contain '..'"),
            Component::RootDir | Component::Prefix(_) => {
                bail!("subvol= path must be relative to the filesystem root")
            }
        }
    }

    Ok((!normalized.as_os_str().is_empty()).then_some(normalized))
}

/// Parse comma-separated mount options and split out mountflags, filesystem
/// specific options, and helper-handled subvolume path selection.
fn parse_mountflag_options(
    options: impl AsRef<str>,
) -> Result<(Option<String>, libc::c_ulong, Option<PathBuf>)> {
    debug!("parsing mount options: {}", options.as_ref());

    let mut opts = Vec::new();
    let mut flags = 0;
    let mut subvol = None;

    for opt in options.as_ref().split(',') {
        match opt {
            "dirsync" => flags |= libc::MS_DIRSYNC,
            "lazytime" => flags |= 1 << 25, // MS_LAZYTIME
            "mand" => flags |= libc::MS_MANDLOCK,
            "noatime" => flags |= libc::MS_NOATIME,
            "nodev" => flags |= libc::MS_NODEV,
            "nodiratime" => flags |= libc::MS_NODIRATIME,
            "noexec" => flags |= libc::MS_NOEXEC,
            "nosuid" => flags |= libc::MS_NOSUID,
            "relatime" => flags |= libc::MS_RELATIME,
            "remount" => flags |= libc::MS_REMOUNT,
            "ro" => flags |= libc::MS_RDONLY,
            "rw" | "" => {}
            "strictatime" => flags |= libc::MS_STRICTATIME,
            "sync" => flags |= libc::MS_SYNCHRONOUS,
            // Userspace-only fstab options — not passed to the kernel
            "auto" | "noauto" | "nofail" | "_netdev" | "user" | "nouser" | "users" | "group"
            | "owner" => {}
            o if o.starts_with("x-") || o.starts_with("X-") || o.starts_with("comment=") => {}
            o if o.starts_with("subvol=") => {
                ensure!(subvol.is_none(), "subvol= specified more than once");
                subvol = parse_subvol_path(&o["subvol=".len()..])?;
            }
            o => opts.push(o),
        }
    }

    Ok((
        if opts.is_empty() {
            None
        } else {
            Some(opts.join(","))
        },
        flags,
        subvol,
    ))
}

fn mount_subvolume(
    src: OsString,
    target: &Path,
    mountflags: libc::c_ulong,
    data: Option<String>,
    subvol: &Path,
) -> Result<()> {
    let mut tmp = TempMount::new()?;

    mount_inner(src, &tmp.path, Some("bcachefs"), mountflags, data)?;
    tmp.mounted = true;

    let subvol_path = tmp.path.join(subvol);
    std::fs::metadata(&subvol_path)
        .with_context(|| format!("opening subvolume path {}", subvol.display()))?;

    mount_inner(
        subvol_path.as_os_str().to_os_string(),
        target,
        None,
        libc::MS_BIND,
        None,
    )?;

    tmp.umount()?;
    Ok(())
}

/// If a user explicitly specifies `unlock_policy` or `passphrase_file` then use
/// that without falling back to other mechanisms. If these options are not
/// used, then search for the key or ask for it.
fn handle_unlock(cli: &Cli, sb: &bch_sb_handle) -> Result<KeyHandle> {
    if let Some(policy) = cli.unlock_policy.as_ref() {
        return policy.apply(sb);
    }

    if let Some(path) = cli.passphrase_file.as_deref() {
        let passphrase_correct = Passphrase::read_from_file(path)?
            .check(sb)
            .ok_or_else(|| anyhow::anyhow!("incorrect passphrase"))?;
        return KeyHandle::new(&passphrase_correct, Keyring::User);
    }

    let uuid = sb.sb().uuid();
    if let Ok(handle) = KeyHandle::new_from_search(&uuid) {
        return Ok(handle);
    }

    let passphrase_correct = Passphrase::ask_and_check(sb)?;
    KeyHandle::new(&passphrase_correct, Keyring::User)
}

fn cmd_mount_inner(cli: &Cli) -> Result<()> {
    let (optstr, mountflags, subvol) = parse_mountflag_options(&cli.options)?;
    let opts =
        bcachefs_kernel::opts::parse_mount_opts(None, optstr.as_deref(), true).unwrap_or_default();

    let sbs = device_scan::scan_sbs(&cli.dev, &opts)?;

    ensure!(!sbs.is_empty(), "No device(s) to mount specified");

    let devices = device_scan::joined_device_str(&sbs);

    let first_sb = &sbs[0].1;
    if unsafe { bch_bindgen::c::bch2_sb_is_encrypted(first_sb.sb) } {
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

        if let Some(subvol) = subvol.as_deref() {
            mount_subvolume(devices, mountpoint, mountflags, optstr, subvol)
        } else {
            mount_inner(devices, mountpoint, Some("bcachefs"), mountflags, optstr)
        }
    } else {
        info!(
            "would mount with params: device: {:?}, options: {}",
            devices, &cli.options
        );

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_subvol_mount_option() {
        let (opts, flags, subvol) =
            parse_mountflag_options("rw,noatime,subvol=/@root,X-mount.mkdir").unwrap();

        assert_eq!(opts, None);
        assert_eq!(flags & libc::MS_NOATIME, libc::MS_NOATIME);
        assert_eq!(subvol, Some(PathBuf::from("@root")));
    }

    #[test]
    fn rejects_escaping_subvol_path() {
        assert!(parse_mountflag_options("subvol=../root").is_err());
    }

    #[test]
    fn keeps_filesystem_options() {
        let (opts, _flags, subvol) =
            parse_mountflag_options("compression=lz4,subvol=home").unwrap();

        assert_eq!(opts.as_deref(), Some("compression=lz4"));
        assert_eq!(subvol, Some(PathBuf::from("home")));
    }
}

/// Mount a bcachefs filesystem by its UUID or label.
#[derive(Parser, Debug)]
#[command(author, version, about,
    long_about = "`mount -t bcachefs` invokes the installed mount.bcachefs helper; \
this is the same mount path exposed as `bcachefs mount`.\n\n\
Mounts a bcachefs filesystem. Devices are discovered automatically \
by scanning for the filesystem UUID or label---unlike btrfs, this is handled \
entirely in userspace.\n\n\
Use OLD_BLKID_UUID=<uuid> in fstab entries when systemd consumes \
UUID=<uuid> before the bcachefs mount helper can scan all members.\n\n\
If the filesystem is encrypted, the passphrase will be looked up in \
the kernel keyring first; if not found, the user is prompted \
interactively (or reads from stdin if not a terminal). Use -k or -f \
to specify alternative unlock methods.\n\n\
Use -o subvol=PATH to mount a subvolume or snapshot path as the mount root."
)]
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

    /// Device, UUID=\<UUID\>, OLD_BLKID_UUID=\<UUID\> (fstab), or LABEL=\<label\>
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

fn mount(cli: Cli) -> std::process::ExitCode {
    let module_loaded = check_bcachefs_module();

    if cli.fs_type == "bcachefs.fuse" {
        let fuse_cli = super::fusemount::Cli {
            options: if cli.options.is_empty() {
                None
            } else {
                Some(cli.options.clone())
            },
            foreground: false,
            device: cli.dev.clone(),
            mountpoint: cli
                .mountpoint
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
        };
        return match super::fusemount::cmd_fusemount(fuse_cli) {
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
        Ok(_) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            error!("Mount failed for {}: {e}", cli.dev);
            if !module_loaded {
                error!("bcachefs module not loaded?");
            }
            std::process::ExitCode::FAILURE
        }
    }
}

pub static CMD: super::CmdDef = {
    fn __cmd() -> clap::Command {
        <Cli as clap::CommandFactory>::command()
    }
    fn __run(argv: Vec<String>) -> std::process::ExitCode {
        mount(Cli::parse_from(argv))
    }
    super::CmdDef {
        name: "mount",
        about: "Mount a filesystem",
        aliases: &[],
        kind: super::CmdKind::Typed {
            cmd: __cmd,
            run: __run,
        },
    }
};
