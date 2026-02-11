use std::ffi::{CStr, CString};

use anyhow::{bail, Result};
use bch_bindgen::bcachefs;
use bch_bindgen::c;
use bch_bindgen::fs::Fs;
use bch_bindgen::opt_set;
use clap::{Arg, ArgAction, Command};

use crate::commands::opts::{bch_opt_lookup, bch_option_args, bch_options_from_matches};
use crate::wrappers::handle::BcachefsHandle;
use crate::wrappers::sysfs;

fn opt_flags() -> u32 {
    c::opt_flags::OPT_FS as u32 | c::opt_flags::OPT_DEVICE as u32
}

pub fn set_option_cmd() -> Command {
    Command::new("set-fs-option")
        .about("Set a filesystem option")
        .args(bch_option_args(opt_flags()))
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

pub fn cmd_set_option(argv: Vec<String>) -> Result<()> {
    let matches = set_option_cmd().get_matches_from(argv);

    let devices: Vec<&String> = matches.get_many::<String>("devices").unwrap().collect();
    let dev_idxs: Vec<u32> = matches.get_many::<u32>("dev-idx")
        .map(|v| v.copied().collect())
        .unwrap_or_default();

    let opts = bch_options_from_matches(&matches, opt_flags());
    if opts.is_empty() {
        bail!("No options specified");
    }

    let online = devices.iter().any(|dev| sysfs::dev_mounted(dev));

    if online {
        set_option_online(&devices, &opts)
    } else {
        set_option_offline(&devices, &dev_idxs, &opts)
    }
}

fn set_option_online(devices: &[&String], opts: &[(String, String)]) -> Result<()> {
    let fs = BcachefsHandle::open(devices[0].as_str())?;

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

        if flags & c::opt_flags::OPT_FS as u32 != 0 {
            sysfs::sysfs_write_str(fs.sysfs_fd(), &format!("options/{name}"), value);
        }

        if flags & c::opt_flags::OPT_DEVICE as u32 != 0 {
            for dev in devices {
                let fs2 = BcachefsHandle::open(dev.as_str())?;
                let dev_idx = fs2.dev_idx();
                if dev_idx < 0 {
                    eprintln!("Couldn't determine device index for {dev}");
                    continue;
                }

                sysfs::sysfs_write_str(fs.sysfs_fd(), &format!("dev-{dev_idx}/{name}"), value);
            }
        }
    }

    Ok(())
}

fn set_option_offline(
    devices: &[&String],
    dev_idxs: &[u32],
    opts: &[(String, String)],
) -> Result<()> {
    let devs: Vec<std::path::PathBuf> = devices.iter().map(|d| d.as_str().into()).collect();

    let mut fs_opts = bcachefs::bch_opts::default();
    opt_set!(fs_opts, nostart, 1);

    let fs = Fs::open(&devs, fs_opts)?;

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
        let mut val: u64 = 0;
        let ret = unsafe {
            c::bch2_opt_parse(fs.raw, opt, c_value.as_ptr(), &mut val, std::ptr::null_mut())
        };
        if ret < 0 {
            eprintln!("Error parsing {name}={value}");
            continue;
        }

        // SAFETY: opt_id came from bch2_opt_lookup which validates [0, bch2_opts_nr)
        let opt_id_enum: c::bch_opt_id = unsafe { std::mem::transmute(opt_id as u32) };

        if flags & c::opt_flags::OPT_FS as u32 != 0 {
            let ret = unsafe {
                c::bch2_opt_hook_pre_set(fs.raw, std::ptr::null_mut(), 0, opt_id_enum, val, true)
            };
            if ret < 0 {
                eprintln!("Error setting {name}: {ret}");
                continue;
            }
            unsafe { c::bch2_opt_set_sb(fs.raw, std::ptr::null_mut(), opt, val); }
        }

        if flags & c::opt_flags::OPT_DEVICE as u32 != 0 {
            let indices: Vec<u32> = if !dev_idxs.is_empty() {
                dev_idxs.to_vec()
            } else {
                devices.iter().filter_map(|dev| {
                    name_to_dev_idx(fs.raw, dev).map(|i| i as u32)
                }).collect()
            };

            for idx in indices {
                let ca = unsafe { (*fs.raw).devs[idx as usize] };
                if ca.is_null() {
                    eprintln!("Couldn't look up device {idx}");
                    continue;
                }

                let ret = unsafe {
                    c::bch2_opt_hook_pre_set(fs.raw, ca, 0, opt_id_enum, val, true)
                };
                if ret < 0 {
                    eprintln!("Error setting {name}: {ret}");
                    continue;
                }
                unsafe { c::bch2_opt_set_sb(fs.raw, ca, opt, val); }
            }
        }
    }

    Ok(())
}

fn name_to_dev_idx(c: *mut c::bch_fs, name: &str) -> Option<usize> {
    unsafe {
        for i in 0..(*c).devs.len() {
            let ca = (*c).devs[i];
            if ca.is_null() { continue; }
            let ca_name = CStr::from_ptr((*ca).name.as_ptr());
            if ca_name.to_bytes() == name.as_bytes() {
                return Some(i);
            }
        }
    }
    None
}
