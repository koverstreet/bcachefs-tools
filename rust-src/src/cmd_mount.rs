use crate::key;
use crate::key::KeyLoc;
use atty::Stream;
use bch_bindgen::{bcachefs, bcachefs::bch_sb_handle, debug, error, info};
use clap::Parser;
use colored::Colorize;
use std::ffi::{c_char, c_int, c_void, CStr, CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use uuid::Uuid;

fn mount_inner(
    src: String,
    target: impl AsRef<std::path::Path>,
    fstype: &str,
    mountflags: u64,
    data: Option<String>,
) -> anyhow::Result<()> {
    // bind the CStrings to keep them alive
    let src = CString::new(src)?;
    let target = CString::new(target.as_ref().as_os_str().as_bytes())?;
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
fn parse_mount_options(options: impl AsRef<str>) -> (Option<String>, u64) {
    use either::Either::*;
    debug!("parsing mount options: {}", options.as_ref());
    let (opts, flags) = options
        .as_ref()
        .split(",")
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
            o @ _ => Right(o),
        })
        .fold((Vec::new(), 0), |(mut opts, flags), next| match next {
            Left(f) => (opts, flags | f),
            Right(o) => {
                opts.push(o);
                (opts, flags)
            }
        });

    use itertools::Itertools;
    (
        if opts.len() == 0 {
            None
        } else {
            Some(opts.iter().join(","))
        },
        flags,
    )
}

fn mount(
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

fn read_super_silent(path: &std::path::PathBuf) -> anyhow::Result<bch_sb_handle> {
    // Stop libbcachefs from spamming the output
    let _gag = gag::BufferRedirect::stdout().unwrap();

    bch_bindgen::rs::read_super(&path)
}

fn get_devices_by_uuid(uuid: Uuid) -> anyhow::Result<Vec<(PathBuf, bch_sb_handle)>> {
    debug!("enumerating udev devices");
    let mut udev = udev::Enumerator::new()?;

    udev.match_subsystem("block")?;

    let devs = udev
        .scan_devices()?
        .into_iter()
        .filter_map(|dev| dev.devnode().map(ToOwned::to_owned))
        .map(|dev| (dev.clone(), read_super_silent(&dev)))
        .filter_map(|(dev, sb)| sb.ok().map(|sb| (dev, sb)))
        .filter(|(_, sb)| sb.sb().uuid() == uuid)
        .collect();
    Ok(devs)
}

fn stdout_isatty() -> &'static str {
    if atty::is(Stream::Stdout) {
        "true"
    } else {
        "false"
    }
}

/// Mount a bcachefs filesystem by its UUID.
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Where the password would be loaded from.
    ///
    /// Possible values are:
    /// "fail" - don't ask for password, fail if filesystem is encrypted;
    /// "wait" - wait for password to become available before mounting;
    /// "ask" -  prompt the user for password;
    #[arg(short, long, default_value = "ask", verbatim_doc_comment)]
    key_location: KeyLoc,

    /// Device, or UUID=<UUID>
    dev: String,

    /// Where the filesystem should be mounted. If not set, then the filesystem
    /// won't actually be mounted. But all steps preceeding mounting the
    /// filesystem (e.g. asking for passphrase) will still be performed.
    mountpoint: std::path::PathBuf,

    /// Mount options
    #[arg(short, default_value = "")]
    options: String,

    /// Force color on/off. Default: autodetect tty
    #[arg(short, long, action = clap::ArgAction::Set, default_value=stdout_isatty())]
    colorize: bool,

    #[arg(short = 'v', long, action = clap::ArgAction::Count)]
    verbose: u8,
}

fn cmd_mount_inner(opt: Cli) -> anyhow::Result<()> {
    let (devs, sbs) = if opt.dev.starts_with("UUID=") {
        let uuid = opt.dev.replacen("UUID=", "", 1);
        let uuid = Uuid::parse_str(&uuid)?;
        let devs_sbs = get_devices_by_uuid(uuid)?;

        let devs_strs: Vec<_> = devs_sbs
            .iter()
            .map(|(dev, _)| dev.clone().into_os_string().into_string().unwrap())
            .collect();
        let devs_str = devs_strs.join(":");
        let sbs = devs_sbs.iter().map(|(_, sb)| *sb).collect();

        (devs_str, sbs)
    } else {
        let mut sbs = Vec::new();

        for dev in opt.dev.split(':') {
            let dev = PathBuf::from(dev);
            sbs.push(bch_bindgen::rs::read_super(&dev)?);
        }

        (opt.dev, sbs)
    };

    if unsafe { bcachefs::bch2_sb_is_encrypted(sbs[0].sb) } {
        let key = opt
            .key_location
            .0
            .ok_or_else(|| anyhow::anyhow!("no keyoption specified for locked filesystem"))?;

        key::prepare_key(&sbs[0], key)?;
    }

    mount(devs, &opt.mountpoint, &opt.options)?;
    Ok(())
}

#[no_mangle]
pub extern "C" fn cmd_mount(argc: c_int, argv: *const *const c_char) {
    let argv: Vec<_> = (0..argc)
        .map(|i| unsafe { CStr::from_ptr(*argv.add(i as usize)) })
        .map(|i| OsStr::from_bytes(i.to_bytes()))
        .collect();

    let opt = Cli::parse_from(argv);
    bch_bindgen::log::set_verbose_level(opt.verbose + bch_bindgen::log::ERROR);
    colored::control::set_override(opt.colorize);
    if let Err(e) = cmd_mount_inner(opt) {
        error!("Fatal error: {}", e);
    }
}
