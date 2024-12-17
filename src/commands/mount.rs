use std::{
    collections::HashMap,
    env,
    ffi::CString,
    io::{stdout, IsTerminal},
    path::{Path, PathBuf},
    ptr, str,
};

use anyhow::{ensure, Result};
use bch_bindgen::{bcachefs, bcachefs::bch_sb_handle, opt_set, path_to_cstr};
use clap::Parser;
use log::{debug, error, info};
use uuid::Uuid;

use crate::{
    key::{KeyHandle, Passphrase, UnlockPolicy},
    logging,
};

fn mount_inner(
    src: String,
    target: &std::path::Path,
    fstype: &str,
    mut mountflags: libc::c_ulong,
    data: Option<String>,
) -> anyhow::Result<()> {
    // bind the CStrings to keep them alive
    let c_src = CString::new(src.clone())?;
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
            eprintln!("mount: {}: {} already mounted or mount point busy", target.to_string_lossy(), src);
        } else {
            eprintln!("mount: {}: {}", src, e);
        }

        Err(e.into())
    } else {
        Ok(())
    }
}

/// Parse a comma-separated mount options and split out mountflags and filesystem
/// specific options.
fn parse_mount_options(options: impl AsRef<str>) -> (Option<String>, libc::c_ulong) {
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

fn read_super_silent(path: impl AsRef<Path>) -> anyhow::Result<bch_sb_handle> {
    let mut opts = bcachefs::bch_opts::default();
    opt_set!(opts, noexcl, 1);

    bch_bindgen::sb_io::read_super_silent(path.as_ref(), opts)
}

fn device_property_map(dev: &udev::Device) -> HashMap<String, String> {
    let rc: HashMap<_, _> = dev
        .properties()
        .map(|i| {
            (
                String::from(i.name().to_string_lossy()),
                String::from(i.value().to_string_lossy()),
            )
        })
        .collect();
    rc
}

fn udev_bcachefs_info() -> anyhow::Result<HashMap<String, Vec<String>>> {
    let mut info = HashMap::new();

    if env::var("BCACHEFS_BLOCK_SCAN").is_ok() {
        debug!("Checking all block devices for bcachefs super block!");
        return Ok(info);
    }

    let mut udev = udev::Enumerator::new()?;

    debug!("Walking udev db!");

    udev.match_subsystem("block")?;
    udev.match_property("ID_FS_TYPE", "bcachefs")?;

    for m in udev
        .scan_devices()?
        .filter(udev::Device::is_initialized)
        .map(|dev| device_property_map(&dev))
        .filter(|m| m.contains_key("ID_FS_UUID") && m.contains_key("DEVNAME"))
    {
        let fs_uuid = m["ID_FS_UUID"].clone();
        let dev_node = m["DEVNAME"].clone();
        info.insert(dev_node.clone(), vec![fs_uuid.clone()]);
        info.entry(fs_uuid).or_insert(vec![]).push(dev_node.clone());
    }

    Ok(info)
}

fn get_super_blocks(uuid: Uuid, devices: &[String]) -> Vec<(PathBuf, bch_sb_handle)> {
    devices
        .iter()
        .filter_map(|dev| {
            read_super_silent(PathBuf::from(dev))
                .ok()
                .map(|sb| (PathBuf::from(dev), sb))
        })
        .filter(|(_, sb)| sb.sb().uuid() == uuid)
        .collect::<Vec<_>>()
}

fn get_all_block_devnodes() -> anyhow::Result<Vec<String>> {
    let mut udev = udev::Enumerator::new()?;
    udev.match_subsystem("block")?;

    let devices = udev
        .scan_devices()?
        .filter_map(|dev| {
            if dev.is_initialized() {
                dev.devnode().map(|dn| dn.to_string_lossy().into_owned())
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    Ok(devices)
}

fn get_devices_by_uuid(
    udev_bcachefs: &HashMap<String, Vec<String>>,
    uuid: Uuid,
) -> anyhow::Result<Vec<(PathBuf, bch_sb_handle)>> {
    let devices = {
        if !udev_bcachefs.is_empty() {
            let uuid_string = uuid.hyphenated().to_string();
            if let Some(devices) = udev_bcachefs.get(&uuid_string) {
                devices.clone()
            } else {
                Vec::new()
            }
        } else {
            get_all_block_devnodes()?
        }
    };

    Ok(get_super_blocks(uuid, &devices))
}

fn devs_str_sbs_from_uuid(
    udev_info: &HashMap<String, Vec<String>>,
    uuid: &str,
) -> anyhow::Result<(String, Vec<bch_sb_handle>)> {
    debug!("enumerating devices with UUID {}", uuid);

    let devs_sbs = Uuid::parse_str(uuid).map(|uuid| get_devices_by_uuid(udev_info, uuid))??;

    let devs_str = devs_sbs
        .iter()
        .map(|(dev, _)| dev.to_str().unwrap())
        .collect::<Vec<_>>()
        .join(":");

    let sbs: Vec<bch_sb_handle> = devs_sbs.iter().map(|(_, sb)| *sb).collect();

    Ok((devs_str, sbs))
}

fn devs_str_sbs_from_device(
    udev_info: &HashMap<String, Vec<String>>,
    device: &Path,
) -> anyhow::Result<(String, Vec<bch_sb_handle>)> {
    let dev_sb = read_super_silent(device)?;

    if dev_sb.sb().number_of_devices() == 1 {
        Ok((device.as_os_str().to_str().unwrap().to_string(), vec![dev_sb]))
    } else {
        let uuid = dev_sb.sb().uuid();

        devs_str_sbs_from_uuid(udev_info, &uuid.to_string())
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
        return Passphrase::new_from_file(path).and_then(|p| KeyHandle::new(sb, &p));
    }

    let uuid = sb.sb().uuid();
    KeyHandle::new_from_search(&uuid)
        .or_else(|_| Passphrase::new(&uuid).and_then(|p| KeyHandle::new(sb, &p)))
}

fn cmd_mount_inner(cli: &Cli) -> Result<()> {
    // Grab the udev information once
    let udev_info = udev_bcachefs_info()?;

    let (devices, mut sbs) =
        if let Some(("UUID" | "OLD_BLKID_UUID", uuid)) = cli.dev.split_once('=') {
            devs_str_sbs_from_uuid(&udev_info, uuid)?
        } else if cli.dev.contains(':') {
            // If the device string contains ":" we will assume the user knows the
            // entire list. If they supply a single device it could be either the FS
            // only has 1 device or it's only 1 of a number of devices which are
            // part of the FS. This appears to be the case when we get called during
            // fstab mount processing and the fstab specifies a UUID.

            let sbs = cli
                .dev
                .split(':')
                .map(read_super_silent)
                .collect::<Result<Vec<_>>>()?;

            (cli.dev.clone(), sbs)
        } else {
            devs_str_sbs_from_device(&udev_info, Path::new(&cli.dev))?
        };

    ensure!(!sbs.is_empty(), "No device(s) to mount specified");

    let first_sb = &sbs[0];
    if unsafe { bcachefs::bch2_sb_is_encrypted(first_sb.sb) } {
        handle_unlock(cli, first_sb)?;
    }

    for sb in &mut sbs {
        unsafe {
            bch_bindgen::sb_io::bch2_free_super(sb);
        }
    }
    drop(sbs);

    if let Some(mountpoint) = cli.mountpoint.as_deref() {
        info!(
            "mounting with params: device: {}, target: {}, options: {}",
            devices,
            mountpoint.to_string_lossy(),
            &cli.options
        );

        let (data, mountflags) = parse_mount_options(&cli.options);
        mount_inner(devices, mountpoint, "bcachefs", mountflags, data)
    } else {
        info!(
            "would mount with params: device: {}, options: {}",
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

    // FIXME: would be nicer to have `--color[=WHEN]` like diff or ls?
    /// Force color on/off. Autodetect tty is used to define default:
    #[arg(short, long, action = clap::ArgAction::Set, default_value_t=stdout().is_terminal())]
    colorize: bool,

    /// Verbose mode
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

pub fn mount(mut argv: Vec<String>, symlink_cmd: Option<&str>) -> std::process::ExitCode {
    // If the bcachefs tool is being called as "bcachefs mount dev ..." (as opposed to via a
    // symlink like "/usr/sbin/mount.bcachefs dev ...", then we need to pop the 0th argument
    // ("bcachefs") since the CLI parser here expects the device at position 1.
    if symlink_cmd.is_none() {
        argv.remove(0);
    }

    let cli = Cli::parse_from(argv);

    // TODO: centralize this on the top level CLI
    logging::setup(cli.verbose, cli.colorize);

    match cmd_mount_inner(&cli) {
        Ok(_)   => std::process::ExitCode::SUCCESS,
        Err(e)   => {
            error!("Mount failed: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
