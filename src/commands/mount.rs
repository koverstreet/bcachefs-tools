use std::{
    ffi::{CString, OsString},
    io::{stdout, IsTerminal},
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
    ptr, str,
};

use anyhow::{ensure, Result};
use bcachefs_kernel::c::bch_sb_handle;
use bcachefs_kernel::path_to_cstr;
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

/// A comma-separated mount option string split into its consumers.
///
/// The same option vocabulary feeds three places - the mount(2) syscall
/// (`flags`), the FUSE mount (`fuse_options`), and the filesystem itself
/// (`fs_opts`, handed to parse_mount_opts later) - so it's tabulated once in
/// [`parse_mountflag_options`] rather than re-derived per caller.
#[derive(Default)]
pub(crate) struct ParsedMountOptions {
    /// Filesystem-specific options: everything not consumed as a kernel flag.
    pub fs_opts:      Option<String>,
    /// Kernel mount flags for mount(2).
    pub flags:        libc::c_ulong,
    /// `flags` expressed as fuser options, for the FUSE path. Flags with no
    /// fuser equivalent are omitted here but still apply via `flags`.
    #[cfg(feature = "fuse")]
    pub fuse_options: Vec<fuser::MountOption>,
}

/// Parse a comma-separated mount option string, splitting kernel mount flags
/// (and their fuser equivalents) from filesystem-specific options.
pub(crate) fn parse_mountflag_options(options: impl AsRef<str>) -> ParsedMountOptions {
    debug!("parsing mount options: {}", options.as_ref());

    let mut parsed = ParsedMountOptions::default();
    let mut fs_opts: Vec<&str> = Vec::new();

    // A kernel flag, optionally paired with its fuser option. The fuser arm is
    // only referenced under the `fuse` feature, so its tokens must live inside
    // the cfg - hence the macro rather than a plain match value.
    macro_rules! flag {
        ($ms:expr) => {{ parsed.flags |= $ms; }};
        ($ms:expr, $fuse:expr) => {{
            parsed.flags |= $ms;
            #[cfg(feature = "fuse")]
            parsed.fuse_options.push($fuse);
        }};
    }

    for opt in options.as_ref().split(',') {
        match opt {
            "dirsync"     => flag!(libc::MS_DIRSYNC, fuser::MountOption::DirSync),
            "lazytime"    => flag!(1 << 25), // MS_LAZYTIME
            "mand"        => flag!(libc::MS_MANDLOCK),
            "noatime"     => flag!(libc::MS_NOATIME, fuser::MountOption::NoAtime),
            "nodev"       => flag!(libc::MS_NODEV, fuser::MountOption::NoDev),
            "nodiratime"  => flag!(libc::MS_NODIRATIME),
            "noexec"      => flag!(libc::MS_NOEXEC, fuser::MountOption::NoExec),
            "nosuid"      => flag!(libc::MS_NOSUID, fuser::MountOption::NoSuid),
            "relatime"    => flag!(libc::MS_RELATIME),
            "remount"     => flag!(libc::MS_REMOUNT),
            "ro"          => flag!(libc::MS_RDONLY, fuser::MountOption::RO),
            "rw" | ""     => {}
            "strictatime" => flag!(libc::MS_STRICTATIME),
            "sync"        => flag!(libc::MS_SYNCHRONOUS, fuser::MountOption::Sync),
            // Userspace-only fstab options - not passed to the kernel:
            "auto" | "noauto" | "nofail" | "_netdev"
            | "user" | "nouser" | "users" | "group" | "owner" => {}
            o if o.starts_with("x-") || o.starts_with("comment=") => {}
            o => fs_opts.push(o),
        }
    }

    parsed.fs_opts = (!fs_opts.is_empty()).then(|| fs_opts.join(","));
    parsed
}

#[cfg(test)]
mod tests {
    use super::parse_mountflag_options;

    #[test]
    fn parse_mountflag_options_splits_kernel_and_fs_options() {
        let p = parse_mountflag_options("ro,noexec,metadata_replicas=2,norecovery");

        assert_eq!(p.fs_opts.as_deref(), Some("metadata_replicas=2,norecovery"));
        assert_ne!(p.flags & libc::MS_RDONLY, 0);
        assert_ne!(p.flags & libc::MS_NOEXEC, 0);
    }

    #[test]
    fn parse_mountflag_options_drops_userspace_fstab_options() {
        let p = parse_mountflag_options("nofail,_netdev,x-systemd.device-timeout=5");

        assert_eq!(p.fs_opts, None);
        assert_eq!(p.flags, 0);
    }
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
    let parsed = parse_mountflag_options(&cli.options);
    let opts = bcachefs_kernel::opts::parse_mount_opts(None, parsed.fs_opts.as_deref(), true)
        .unwrap_or_default();

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

        mount_inner(devices, mountpoint, "bcachefs", parsed.flags, parsed.fs_opts)
    } else {
        info!(
            "would mount with params: device: {:?}, options: {}",
            devices, &cli.options
        );

        Ok(())
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
to specify alternative unlock methods.")]
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

struct ModuleCheck {
    loaded:         bool,
    modprobe_error: Option<String>,
}

fn check_bcachefs_module() -> ModuleCheck {
    let path = Path::new("/sys/module/bcachefs");
    if path.exists() {
        return ModuleCheck { loaded: true, modprobe_error: None };
    }

    let modprobe_error = match std::process::Command::new("modprobe").arg("bcachefs").status() {
        Ok(s) if s.success() => None,
        Ok(_)  => Some("modprobe bcachefs exited unsuccessfully".to_string()),
        Err(e) => Some(format!("could not run modprobe bcachefs: {e}")),
    };

    ModuleCheck { loaded: path.exists(), modprobe_error }
}

fn mount(cli: Cli) -> std::process::ExitCode {
    let module = check_bcachefs_module();

    if cli.fs_type == "bcachefs.fuse" {
        #[cfg(feature = "fuse")]
        {
            let fuse_cli = super::fusemount::Cli {
                options: if cli.options.is_empty() { None } else { Some(cli.options.clone()) },
                foreground: false,
                device: cli.dev.clone(),
                mountpoint: cli.mountpoint.as_ref()
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
        #[cfg(not(feature = "fuse"))]
        {
            error!("FUSE support not compiled in (build with the 'fuse' feature)");
            return std::process::ExitCode::FAILURE;
        }
    }

    // TODO: centralize this on the top level CLI
    logging::setup(cli.verbose, cli.colorize);

    match cmd_mount_inner(&cli) {
        Ok(_)   => std::process::ExitCode::SUCCESS,
        Err(e)   => {
            error!("Mount failed for {}: {e}", cli.dev);
            if !module.loaded {
                error!("bcachefs module not loaded?");
                if let Some(e) = module.modprobe_error {
                    error!("{e}");
                }
            }
            std::process::ExitCode::FAILURE
        }
    }
}

pub static CMD: super::CmdDef = {
    fn __cmd() -> clap::Command { <Cli as clap::CommandFactory>::command() }
    fn __run(argv: Vec<String>) -> std::process::ExitCode {
        mount(Cli::parse_from(argv))
    }
    super::CmdDef {
        name: "mount", about: "Mount a filesystem", aliases: &[],
        kind: super::CmdKind::Typed { cmd: __cmd, run: __run },
    }
};
