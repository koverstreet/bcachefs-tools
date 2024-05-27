use crate::key;
use crate::key::UnlockPolicy;
use bch_bindgen::{bcachefs, bcachefs::bch_sb_handle, opt_set, path_to_cstr};
use clap::Parser;
use log::{debug, error, info, LevelFilter};
use std::collections::HashMap;
use std::ffi::{c_char, c_void, CString};
use std::io::{stdout, IsTerminal};
use std::path::{Path, PathBuf};
use std::{env, fs, str};
use uuid::Uuid;

fn mount_inner(
    src: String,
    target: impl AsRef<std::path::Path>,
    fstype: &str,
    mountflags: libc::c_ulong,
    data: Option<String>,
) -> anyhow::Result<()> {
    // bind the CStrings to keep them alive
    let src = CString::new(src)?;
    let target = path_to_cstr(target);
    let data = data.map(CString::new).transpose()?;
    let fstype = CString::new(fstype)?;

    // convert to pointers for ffi
    let src = src.as_c_str().to_bytes_with_nul().as_ptr() as *const c_char;
    let target = target.as_c_str().to_bytes_with_nul().as_ptr() as *const c_char;
    let data = data.as_ref().map_or(std::ptr::null(), |data| {
        data.as_c_str().to_bytes_with_nul().as_ptr() as *const c_void
    });
    let fstype = fstype.as_c_str().to_bytes_with_nul().as_ptr() as *const c_char;

    let ret = {
        info!("mounting filesystem");
        // REQUIRES: CAP_SYS_ADMIN
        unsafe { libc::mount(src, target, fstype, mountflags, data) }
    };
    match ret {
        0 => Ok(()),
        _ => Err(crate::ErrnoError(errno::errno()).into()),
    }
}

/// Parse a comma-separated mount options and split out mountflags and filesystem
/// specific options.
fn parse_mount_options(options: impl AsRef<str>) -> (Option<String>, libc::c_ulong) {
    use either::Either::*;
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
            "rw" => Left(0),
            "strictatime" => Left(libc::MS_STRICTATIME),
            "sync" => Left(libc::MS_SYNCHRONOUS),
            "" => Left(0),
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

fn do_mount(
    device: String,
    target: impl AsRef<std::path::Path>,
    options: impl AsRef<str>,
) -> anyhow::Result<()> {
    let (data, mountflags) = parse_mount_options(options);

    info!(
        "mounting bcachefs filesystem, {}",
        target.as_ref().display()
    );
    mount_inner(device, target, "bcachefs", mountflags, data)
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
        .filter(|dev| dev.is_initialized())
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

fn get_super_blocks(
    uuid: Uuid,
    devices: &[String],
) -> anyhow::Result<Vec<(PathBuf, bch_sb_handle)>> {
    Ok(devices
        .iter()
        .filter_map(|dev| {
            read_super_silent(PathBuf::from(dev))
                .ok()
                .map(|sb| (PathBuf::from(dev), sb))
        })
        .filter(|(_, sb)| sb.sb().uuid() == uuid)
        .collect::<Vec<_>>())
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

    get_super_blocks(uuid, &devices)
}

#[allow(clippy::type_complexity)]
fn get_uuid_for_dev_node(
    udev_bcachefs: &HashMap<String, Vec<String>>,
    device: &std::path::PathBuf,
) -> anyhow::Result<(Option<Uuid>, Option<(PathBuf, bch_sb_handle)>)> {
    let canonical = fs::canonicalize(device)?;

    if !udev_bcachefs.is_empty() {
        let dev_node_str = canonical.into_os_string().into_string().unwrap();

        if udev_bcachefs.contains_key(&dev_node_str) && udev_bcachefs[&dev_node_str].len() == 1 {
            let uuid_str = udev_bcachefs[&dev_node_str][0].clone();
            return Ok((Some(Uuid::parse_str(&uuid_str)?), None));
        }
    } else {
        return read_super_silent(&canonical).map_or(Ok((None, None)), |sb| {
            Ok((Some(sb.sb().uuid()), Some((canonical, sb))))
        });
    }
    Ok((None, None))
}

/// Mount a bcachefs filesystem by its UUID.
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    /// Path to passphrase/key file
    ///
    /// Precedes key_location/unlock_policy: if the filesystem can be decrypted
    /// by the specified passphrase file; it is decrypted. (i.e. Regardless
    /// if "fail" is specified for key_location/unlock_policy.)
    #[arg(short = 'f', long)]
    passphrase_file: Option<PathBuf>,

    /// Password policy to use in case of encrypted filesystem.
    ///
    /// Possible values are:
    /// "fail" - don't ask for password, fail if filesystem is encrypted;
    /// "wait" - wait for password to become available before mounting;
    /// "ask" -  prompt the user for password;
    #[arg(
        short = 'k',
        long = "key_location",
        default_value = "ask",
        verbatim_doc_comment
    )]
    unlock_policy: UnlockPolicy,

    /// Device, or UUID=\<UUID\>
    dev: String,

    /// Where the filesystem should be mounted. If not set, then the filesystem
    /// won't actually be mounted. But all steps preceeding mounting the
    /// filesystem (e.g. asking for passphrase) will still be performed.
    mountpoint: Option<PathBuf>,

    /// Mount options
    #[arg(short, default_value = "")]
    options: String,

    /// Force color on/off. Autodetect tty is used to define default:
    #[arg(short, long, action = clap::ArgAction::Set, default_value_t=stdout().is_terminal())]
    colorize: bool,

    /// Verbose mode
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

fn devs_str_sbs_from_uuid(
    udev_info: &HashMap<String, Vec<String>>,
    uuid: String,
) -> anyhow::Result<(String, Vec<bch_sb_handle>)> {
    debug!("enumerating devices with UUID {}", uuid);

    let devs_sbs = Uuid::parse_str(&uuid).map(|uuid| get_devices_by_uuid(udev_info, uuid))??;

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
    device: &std::path::PathBuf,
) -> anyhow::Result<(String, Vec<bch_sb_handle>)> {
    let (uuid, sb_info) = get_uuid_for_dev_node(udev_info, device)?;

    match (uuid, sb_info) {
        (Some(uuid), Some((path, sb))) => {
            // If we have a super block, it implies we aren't using udev db.  If we only need
            // 1 device to mount, we'll simply return it as we're done, else we'll use the uuid
            // to walk through all the block devices.
            debug!(
                "number of devices in this FS = {}",
                sb.sb().number_of_devices()
            );
            if sb.sb().number_of_devices() == 1 {
                let dev = path.into_os_string().into_string().unwrap();
                Ok((dev, vec![sb]))
            } else {
                devs_str_sbs_from_uuid(udev_info, uuid.to_string())
            }
        }
        (Some(uuid), None) => devs_str_sbs_from_uuid(udev_info, uuid.to_string()),
        _ => Ok((String::new(), Vec::new())),
    }
}

fn parse_passphrase_file_from_mount_options(options: impl AsRef<str>) -> Option<PathBuf> {
    options
        .as_ref()
        .split(",")
        .fold(None, |_, next| match next {
            x if x.starts_with("passphrase_file") => Some(PathBuf::from(x.split("=").nth(1).unwrap().to_string())),
            _ => None,
        })
}

fn cmd_mount_inner(opt: Cli) -> anyhow::Result<()> {
    // Grab the udev information once
    let udev_info = udev_bcachefs_info()?;

    let (devices, block_devices_to_mount) = if opt.dev.starts_with("UUID=") {
        let uuid = opt.dev.replacen("UUID=", "", 1);
        devs_str_sbs_from_uuid(&udev_info, uuid)?
    } else if opt.dev.starts_with("OLD_BLKID_UUID=") {
        let uuid = opt.dev.replacen("OLD_BLKID_UUID=", "", 1);
        devs_str_sbs_from_uuid(&udev_info, uuid)?
    } else {
        // If the device string contains ":" we will assume the user knows the entire list.
        // If they supply a single device it could be either the FS only has 1 device or it's
        // only 1 of a number of devices which are part of the FS. This appears to be the case
        // when we get called during fstab mount processing and the fstab specifies a UUID.
        if opt.dev.contains(':') {
            let mut block_devices_to_mount = Vec::new();

            for dev in opt.dev.split(':') {
                let dev = PathBuf::from(dev);
                block_devices_to_mount.push(read_super_silent(&dev)?);
            }

            (opt.dev, block_devices_to_mount)
        } else {
            devs_str_sbs_from_device(&udev_info, &PathBuf::from(opt.dev))?
        }
    };

    if block_devices_to_mount.is_empty() {
        Err(anyhow::anyhow!("No device found from specified parameters"))?;
    }

    let key_name = CString::new(format!(
        "bcachefs:{}",
        block_devices_to_mount[0].sb().uuid()
    ))
    .unwrap();

    // Check if the filesystem's master key is encrypted and we don't have a key
    if unsafe { bcachefs::bch2_sb_is_encrypted_and_locked(block_devices_to_mount[0].sb) }
        && !key::check_for_key(&key_name)?
    {
        // First by password_file, if available
        let fallback_to_unlock_policy = if let Some(passphrase_file) = &opt.passphrase_file {
            // Unlock by passphrase_file specified by cli
            debug!("Attempting to unlock the master key with the passphrase file specified by cli");
            attempt_unlock_master_key_with_passphrase_file(&block_devices_to_mount[0], passphrase_file)
        } else if let Some(passphrase_file) = &parse_passphrase_file_from_mount_options(&opt.options) {
            // Unlock by passphrase_file specified by mount options
            debug!("Attempting to unlock the master key with the passphrase_file specified in the mount options");
            attempt_unlock_master_key_with_passphrase_file(&block_devices_to_mount[0], passphrase_file)

        } else {
            // No passphrase_file specified, fall back to unlock_policy
            true
        };
        // If decryption by key_file was unsuccesful, prompt for passphrase (or follow key_policy)
        if fallback_to_unlock_policy {
            key::apply_key_unlocking_policy(&block_devices_to_mount[0], opt.unlock_policy)?;
        };
    }

    if let Some(mountpoint) = opt.mountpoint {
        info!(
            "mounting with params: device: {}, target: {}, options: {}",
            devices,
            mountpoint.to_string_lossy(),
            &opt.options
        );

        do_mount(devices, mountpoint, &opt.options)?;
    } else {
        info!(
            "would mount with params: device: {}, options: {}",
            devices, &opt.options
        );
    }

    Ok(())
}

fn attempt_unlock_master_key_with_passphrase_file(block_device: bch_sb_handle, passphrase_file: PathBuf) -> bool {
    match key::read_from_passphrase_file(block_device, passphrase_file.as_path()) {
        Ok(()) => {
            // Decryption succeeded
            false
        }
        Err(err) => {
            // Decryption failed, fall back to unlock_policy
            error!("Failed to decrypt using passphrase_file: {}", err);
            true
        }
    }
}

pub fn mount(mut argv: Vec<String>, symlink_cmd: Option<&str>) -> i32 {
    // If the bcachefs tool is being called as "bcachefs mount dev ..." (as opposed to via a
    // symlink like "/usr/sbin/mount.bcachefs dev ...", then we need to pop the 0th argument
    // ("bcachefs") since the CLI parser here expects the device at position 1.
    if symlink_cmd.is_none() {
        argv.remove(0);
    }

    let opt = Cli::parse_from(argv);

    // @TODO : more granular log levels via mount option
    log::set_max_level(match opt.verbose {
        0 => LevelFilter::Warn,
        1 => LevelFilter::Trace,
        2_u8..=u8::MAX => todo!(),
    });

    colored::control::set_override(opt.colorize);
    if let Err(e) = cmd_mount_inner(opt) {
        error!("Fatal error: {}", e);
        1
    } else {
        info!("Successfully mounted");
        0
    }
}
