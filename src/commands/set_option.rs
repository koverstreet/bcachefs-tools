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

fn has_flag(flags: u32, flag: c::opt_flags) -> bool {
    flags & flag as u32 != 0
}

fn write_online_fs_option(flags: u32) -> bool {
    has_flag(flags, c::opt_flags::OPT_FS) && !has_flag(flags, c::opt_flags::OPT_DEVICE)
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

        if write_online_fs_option(flags) {
            sysfs::sysfs_write_str(fs.sysfs_fd(), &format!("options/{name}"), value);
        }

        if has_flag(flags, c::opt_flags::OPT_DEVICE) {
            if !dev_idxs.is_empty() {
                for dev_idx in dev_idxs {
                    sysfs::sysfs_write_str(fs.sysfs_fd(), &format!("dev-{dev_idx}/{name}"), value);
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

                sysfs::sysfs_write_str(fs.sysfs_fd(), &format!("dev-{dev_idx}/{name}"), value);
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

        if has_flag(flags, c::opt_flags::OPT_FS) {
            let ret = unsafe {
                c::bch2_opt_hook_pre_set(fs.raw, std::ptr::null_mut(), 0, opt_id, val, true, std::ptr::null_mut())
            };
            if ret < 0 {
                eprintln!("Error setting {name}: {ret}");
                continue;
            }
            unsafe { c::bch2_opt_set_sb(fs.raw, std::ptr::null_mut(), opt, val, c_value.as_ptr()); }
        }

        if has_flag(flags, c::opt_flags::OPT_DEVICE) {
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
                    c::bch2_opt_hook_pre_set(fs.raw, ca, 0, opt_id, val, true, std::ptr::null_mut())
                };
                if ret < 0 {
                    eprintln!("Error setting {name}: {ret}");
                    continue;
                }
                unsafe { c::bch2_opt_set_sb(fs.raw, ca, opt, val, c_value.as_ptr()); }
            }
        }
    }

    Ok(())
}

fn name_to_dev_idx(c: *mut c::bch_fs, name: &str) -> Option<usize> {
    let devs_len = unsafe { (*c).devs.len() };
    for i in 0..devs_len {
        let ca = unsafe { (*c).devs[i] };
        if ca.is_null() { continue; }
        // bch_dev.name is a [c_char; 32] array, not a pointer
        let ca_name_bytes = unsafe { &(*ca).name };
        // Find the null terminator
        let len = ca_name_bytes.iter().position(|&b| b == 0).unwrap_or(32);
        // c_char is i8, but from_utf8 wants u8 - use from_raw_parts to reinterpret
        let ca_name_bytes_u8 = unsafe {
            std::slice::from_raw_parts(ca_name_bytes[..len].as_ptr() as *const u8, len)
        };
        let ca_name = std::str::from_utf8(ca_name_bytes_u8).ok()?;
        if ca_name == name {
            return Some(i);
        }
    }
    None
}

pub const CMD: super::CmdDef = raw_cmd!("set-fs-option", "Set filesystem options", cmd_set_option);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn online_sysfs_path_skips_fs_entry_for_device_scoped_options() {
        let fs = c::opt_flags::OPT_FS as u32;
        let dev = c::opt_flags::OPT_DEVICE as u32;

        assert!(write_online_fs_option(fs));
        assert!(!write_online_fs_option(dev));
        assert!(!write_online_fs_option(fs | dev));
    }
}
