use std::ffi::CString;

use anyhow::{bail, Result};
use bcachefs_kernel::c;
use bcachefs_kernel::fs::Fs;
use bcachefs_kernel::opt_set;
use clap::{Arg, ArgAction, Command};

use crate::commands::opts::{bch_opt_lookup, bch_option_args, bch_options_from_matches};
use crate::device_scan::OpenedFs;
use crate::wrappers::handle::BcachefsHandle;
use crate::wrappers::sysfs;

fn opt_flags() -> u32 {
    c::opt_flags::OPT_FS as u32 | c::opt_flags::OPT_DEVICE as u32
}

fn set_option_cmd() -> Command {
    Command::new("set-fs-option")
        .about("Set a filesystem option")
        .long_about("\
Set a filesystem or device option on a running filesystem. Changes \
are persisted to the superblock. Use -d to target a specific device \
for device-scoped options. See <<sec:options>> for the full list of \
available options.")
        .args(bch_option_args(opt_flags(), false))
        .arg(Arg::new("dev-idx")
            .short('d')
            .long("dev-idx")
            .action(ArgAction::Append)
            .value_parser(clap::value_parser!(u32))
            .help("Device index for device-specific options"))
        .arg(Arg::new("devices")
            .required(true)
            .action(ArgAction::Append)
            .help("Device path(s)"))
}

fn cmd_set_option(argv: Vec<String>) -> Result<()> {
    let matches = set_option_cmd().get_matches_from(argv);

    let devices: Vec<&String> = matches.get_many::<String>("devices").unwrap().collect();
    let dev_idxs: Vec<u32> = matches.get_many::<u32>("dev-idx")
        .map(|v| v.copied().collect())
        .unwrap_or_default();

    let opts = bch_options_from_matches(&matches, opt_flags());
    if opts.is_empty() {
        bail!("No options specified");
    }

    let devs: Vec<std::path::PathBuf> = devices.iter().map(|d| d.as_str().into()).collect();

    let mut fs_opts = c::bch_opts::default();
    opt_set!(fs_opts, nostart, 1);

    match crate::device_scan::open_online_or_offline(&devs, fs_opts)? {
        OpenedFs::Online(fs)  => set_option_online(fs, &devices, &dev_idxs, &opts),
        OpenedFs::Offline(fs) => set_option_offline(fs, &devices, &dev_idxs, &opts),
    }
}

fn set_option_online(
    fs: BcachefsHandle,
    devices: &[&String],
    dev_idxs: &[u32],
    opts: &[(String, String)],
) -> Result<()> {
    for dev in &devices[1..] {
        let fs2 = BcachefsHandle::open(dev.as_str())?;
        if fs.uuid() != fs2.uuid() {
            bail!("Filesystem mounted, but not all devices are members");
        }
    }

    for (name, value) in opts {
        let Some((_id, opt)) = bch_opt_lookup(name) else {
            eprintln!("Unknown option: {name}");
            continue;
        };
        let flags = opt.flags as u32;

        if flags & opt_flags() == 0 {
            eprintln!("Can't set option {name}");
            continue;
        }

        // The online path can only write sysfs, and the kernel creates an
        // option's attribute 0444 unless it is OPT_RUNTIME (fs/opts.c:
        // `.attr.mode = (_flags) & OPT_RUNTIME ? 0644 : 0444`). So a
        // non-runtime option has a file that cannot be opened for writing,
        // and testing OPT_FS|OPT_DEVICE above lets it through to fail.
        // Many such options *can* be set with the filesystem unmounted, so
        // say that rather than just refusing.
        if flags & c::opt_flags::OPT_RUNTIME as u32 == 0 {
            eprintln!("{name} cannot be set while the filesystem is mounted \
                       (unmount and set it offline)");
            continue;
        }

        let is_fs_opt = flags & c::opt_flags::OPT_FS as u32 != 0;
        let is_device_opt = flags & c::opt_flags::OPT_DEVICE as u32 != 0;

        if is_fs_opt && !is_device_opt {
            if let Err(e) = sysfs::sysfs_write_str(fs.sysfs_fd(), &format!("options/{name}"), value) {
                eprintln!("Error setting {name}: {e}");
            }
        }

        if is_device_opt {
            if !dev_idxs.is_empty() {
                for dev_idx in dev_idxs {
                    if let Err(e) = sysfs::sysfs_write_str(fs.sysfs_fd(), &format!("dev-{dev_idx}/{name}"), value) {
                        eprintln!("Error setting {name} on device {dev_idx}: {e}");
                    }
                }
                continue;
            }

            for dev in devices {
                let fs2 = BcachefsHandle::open(dev.as_str())?;
                let dev_idx = fs2.dev_idx();
                if dev_idx < 0 {
                    eprintln!("Couldn't determine device index for {dev}; use --dev-idx");
                    continue;
                }

                if let Err(e) = sysfs::sysfs_write_str(fs.sysfs_fd(), &format!("dev-{dev_idx}/{name}"), value) {
                    eprintln!("Error setting {name} on device {dev_idx}: {e}");
                }
            }
        }
    }

    Ok(())
}

fn set_option_offline(
    fs: Fs,
    devices: &[&String],
    dev_idxs: &[u32],
    opts: &[(String, String)],
) -> Result<()> {
    let mut modified = false;

    for (name, value) in opts {
        let Some((opt_id, opt)) = bch_opt_lookup(name) else {
            eprintln!("Unknown option: {name}");
            continue;
        };
        let flags = opt.flags as u32;

        if flags & opt_flags() == 0 {
            eprintln!("Can't set option {name}");
            continue;
        }

        let c_value = CString::new(value.as_str())?;
        let Ok(val) = bcachefs_kernel::opts::opt_parse(Some(&fs), opt, &c_value, None) else {
            eprintln!("Error parsing {name}={value}");
            continue;
        };

        if flags & c::opt_flags::OPT_FS as u32 != 0 {
            if let Err(e) = fs.opt_hook_pre_set(None, opt_id, val) {
                eprintln!("Error setting {name}: {e}");
                continue;
            }
            fs.opt_set_sb(None, opt, val, Some(&c_value));
            modified = true;
        }

        if flags & c::opt_flags::OPT_DEVICE as u32 != 0 {
            let indices: Vec<u32> = if !dev_idxs.is_empty() {
                dev_idxs.to_vec()
            } else {
                devices.iter().filter_map(|dev| {
                    name_to_dev_idx(&fs, dev).map(|i| i as u32)
                }).collect()
            };

            for idx in indices {
                let Some(ca) = fs.dev_get(idx) else {
                    eprintln!("Couldn't look up device {idx}");
                    continue;
                };

                if let Err(e) = fs.opt_hook_pre_set(Some(&ca), opt_id, val) {
                    eprintln!("Error setting {name}: {e}");
                    continue;
                }
                fs.opt_set_sb(Some(&ca), opt, val, Some(&c_value));
                modified = true;
            }
        }
    }

    if modified {
        if fs.disk_sb().sb().sb_initialized() == 0 {
            bail!("superblock not initialized (filesystem was never started): \
                   bch2_write_super would silently skip the write; mount it once first");
        }
        let _lock = fs.sb_lock();
        fs.write_super_force()
            .map_err(|e| anyhow::anyhow!("error writing superblock: {e}"))?;
    }

    Ok(())
}

fn name_to_dev_idx(fs: &Fs, name: &str) -> Option<usize> {
    (0..fs.nr_devices())
        .find(|&i| fs.dev_get(i).is_some_and(|ca| ca.name().to_bytes() == name.as_bytes()))
        .map(|i| i as usize)
}

pub const CMD: super::CmdDef = raw_cmd!("set-fs-option", "Set filesystem options", cmd_set_option);
