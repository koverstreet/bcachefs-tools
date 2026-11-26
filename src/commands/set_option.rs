
use anyhow::{bail, Result};
use bcachefs_kernel::c;
use bcachefs_kernel::fs::{Fs, OpenFs};
use bcachefs_kernel::init::passes::{passes_to_stable, RecoveryPass};
use bcachefs_kernel::opts::OptId;
use bcachefs_kernel::sb::io::SbResize;
use bcachefs_kernel::util::printbuf::Printbuf;
use bcachefs_kernel::util::rcu;
use clap::{Arg, ArgAction, Command};

use crate::commands::opts::{bch_opt_lookup, bch_option_args, bch_options_from_matches};
use crate::device_scan::OpenedFs;
use crate::wrappers::handle::BcachefsHandle;
use crate::wrappers::sysfs;

fn opt_flags() -> u32 {
    c::OptFlags::fs.bits() | c::OptFlags::device.bits()
}

fn reconcile_stable_bit() -> u64 {
    passes_to_stable(1u64 << RecoveryPass::set_fs_needs_reconcile as u64)
}

/// The options whose change makes existing data need a reconcile scan:
/// opt_hook_io()'s, which only runs on a started filesystem.
fn changes_io_options(id: OptId) -> bool {
    use OptId as O;
    matches!(id,
        O::foreground_target | O::background_target | O::promote_target |
        O::compression | O::background_compression | O::data_checksum |
        O::data_replicas | O::erasure_code | O::nocow |
        O::metadata_target | O::metadata_checksum | O::metadata_replicas)
}

/// Bit-math shared by preset/restore so the unit tests pin it down.
fn with_marker_bit(cur: u64, set: bool) -> u64 {
    let bit = reconcile_stable_bit();
    if set { cur | bit } else { cur & !bit }
}

/// Set (or restore) the reconcile marker in the superblock's recovery passes
/// required: returns its previous state.
fn set_reconcile_marker(fs: &Fs, set: bool) -> Result<bool> {
    let mut sb = fs.sb_lock();
    let ext_u64s = (std::mem::size_of::<c::bch_sb_field_ext>() / std::mem::size_of::<u64>()) as u32;
    let mut disk_sb = fs.sb_resize(&mut sb);
    let ext = disk_sb.field_get_minsize::<c::bch_sb_field_ext>(ext_u64s)
        .ok_or_else(|| anyhow::anyhow!("Error getting sb_field_ext"))?;
    let r = &mut ext.recovery_passes_required[0];
    let was_set = r.get() & reconcile_stable_bit() != 0;
    r.set(with_marker_bit(r.get(), set));
    Ok(was_set)
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
    fs_opts.set_nostart(true);
    fs_opts.set_will_not_start(true);

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

    let mut failed = std::collections::BTreeSet::new();

    for (name, value) in opts {

        let Some((_id, opt)) = bch_opt_lookup(name) else {
            eprintln!("Unknown option: {name}");
            failed.insert(name.as_str());
            continue;
        };
        let flags = opt.flags.bits();

        if flags & opt_flags() == 0 {
            eprintln!("Can't set option {name}");
            failed.insert(name.as_str());
            continue;
        }

        // The online path can only write sysfs, and the kernel creates an
        // option's attribute 0444 unless it is a runtime option (OPT_FILES,
        // fs/debug/sysfs.rs). So a
        // non-runtime option has a file that cannot be opened for writing,
        // and testing fs|device above lets it through to fail.
        // Many such options *can* be set with the filesystem unmounted, so
        // say that rather than just refusing.
        if flags & c::OptFlags::runtime.bits() == 0 {
            eprintln!("{name} cannot be set while the filesystem is mounted \
                       (unmount and set it offline)");
            failed.insert(name.as_str());
            continue;
        }

        let is_fs_opt = flags & c::OptFlags::fs.bits() != 0;
        let is_device_opt = flags & c::OptFlags::device.bits() != 0;

        if is_fs_opt && !is_device_opt {
            if let Err(e) = sysfs::sysfs_write_str(fs.sysfs_fd(), &format!("options/{name}"), value) {
                eprintln!("Error setting {name}: {e}");
                failed.insert(name.as_str());
            }
        }

        if is_device_opt {
            if !dev_idxs.is_empty() {
                for dev_idx in dev_idxs {
                    if let Err(e) = sysfs::sysfs_write_str(fs.sysfs_fd(), &format!("dev-{dev_idx}/{name}"), value) {
                        eprintln!("Error setting {name} on device {dev_idx}: {e}");
                        failed.insert(name.as_str());
                    }
                }
                continue;
            }

            for dev in devices {
                let fs2 = BcachefsHandle::open(dev.as_str())?;
                let dev_idx = fs2.dev_idx();
                if dev_idx < 0 {
                    eprintln!("Couldn't determine device index for {dev}; use --dev-idx");
                    failed.insert(name.as_str());
                    continue;
                }

                if let Err(e) = sysfs::sysfs_write_str(fs.sysfs_fd(), &format!("dev-{dev_idx}/{name}"), value) {
                    eprintln!("Error setting {name} on device {dev_idx}: {e}");
                    failed.insert(name.as_str());
                }
            }
        }
    }

    // A configuration command that could not do what it was asked must not
    // report success: scripts have no other way to tell.
    if !failed.is_empty() {
        bail!("{} of {} option(s) could not be set: {}",
              failed.len(), opts.len(),
              failed.into_iter().collect::<Vec<_>>().join(", "));
    }

    Ok(())
}

fn set_option_offline(
    fs: OpenFs,
    devices: &[&String],
    dev_idxs: &[u32],
    opts: &[(String, String)],
) -> Result<()> {
    let mut modified = false;
    let mut failed = std::collections::BTreeSet::new();
    let mut needs_reconcile = false;
    let mut marker_preset = false;
    let mut marker_was_set = false;

    for (name, value) in opts {

        let Some((opt_id, opt)) = bch_opt_lookup(name) else {
            eprintln!("Unknown option: {name}");
            failed.insert(name.as_str());
            continue;
        };
        let flags = opt.flags.bits();

        if flags & opt_flags() == 0 {
            eprintln!("Can't set option {name}");
            failed.insert(name.as_str());
            continue;
        }

        let mut err = Printbuf::new();
        let Ok(val) = bcachefs_kernel::opts::opt_parse(Some(&fs), opt, Some(value.as_bytes()), Some(&mut err)) else {
            match err.as_str().trim_end() {
                ""  => eprintln!("Error parsing {name}={value}"),
                msg => eprintln!("Error parsing {name}={value}: {msg}"),
            }
            failed.insert(name.as_str());
            continue;
        };

        if flags & c::OptFlags::fs.bits() != 0 {
            if let Err(e) = fs.opt_hook_pre_set(None, opt_id, val) {
                eprintln!("Error setting {name}: {e}");
                failed.insert(name.as_str());
                continue;
            }
            // opt_set_sb() writes the superblock itself when the value
            // changed, so pre-set the reconcile marker first: then the option
            // change and the marker persist in the same write, and a crash
            // between them can't leave the option without its scan. When the
            // value is unchanged the marker is restored below.
            let is_inode_opt = changes_io_options(opt_id);
            if is_inode_opt && !marker_preset {
                marker_was_set = set_reconcile_marker(&fs, true)?;
                marker_preset = true;
            }
            match fs.opt_set_sb(None, opt, val, Some(value.as_bytes())) {
                Err(e) => {
                    eprintln!("Error setting {name}={value}: {e}");
                    failed.insert(name.as_str());
                    continue;
                }
                Ok(changed) => {
                    modified |= changed;
                    needs_reconcile |= changed && is_inode_opt;
                }
            }
        }

        if flags & c::OptFlags::device.bits() != 0 {
            let indices: Vec<u32> = if !dev_idxs.is_empty() {
                dev_idxs.to_vec()
            } else {
                // Don't silently drop a device we couldn't resolve: with every
                // name unresolved this list is empty, the loop below never
                // runs, and the command reports success having done nothing.
                let mut idxs = Vec::with_capacity(devices.len());
                for dev in devices {
                    match name_to_dev_idx(&fs, dev) {
                        Some(i) => idxs.push(i as u32),
                        None => {
                            eprintln!("Couldn't find device {dev} in this filesystem");
                            failed.insert(name.as_str());
                        }
                    }
                }
                idxs
            };

            if indices.is_empty() {
                eprintln!("No devices to set {name} on");
                failed.insert(name.as_str());
            }

            for idx in indices {
                let Some(ca) = fs.dev_get(idx) else {
                    eprintln!("Couldn't look up device {idx}");
                    failed.insert(name.as_str());
                    continue;
                };

                if let Err(e) = fs.opt_hook_pre_set(Some(&*ca), opt_id, val) {
                    eprintln!("Error setting {name}: {e}");
                    failed.insert(name.as_str());
                    continue;
                }
                if let Err(e) = fs.opt_set_sb(Some(&*ca), opt, val, Some(value.as_bytes())) {
                    eprintln!("Error setting {name}={value} on device {idx}: {e}");
                    failed.insert(name.as_str());
                    continue;
                }
                modified = true;
            }
        }
    }

    if marker_preset && !needs_reconcile {
        set_reconcile_marker(&fs, marker_was_set)?;
    }

    if modified {
        fs.write_super_ret(&mut fs.sb_lock())
            .map_err(|e| anyhow::anyhow!("error writing superblock: {e}"))?;
    }

    if !failed.is_empty() {
        bail!("{} of {} option(s) could not be set: {}",
              failed.len(), opts.len(),
              failed.into_iter().collect::<Vec<_>>().join(", "));
    }

    Ok(())
}

fn name_to_dev_idx(fs: &Fs, name: &str) -> Option<usize> {
    fs.devs_rcu(&rcu::read_lock())
        .find(|ca| ca.name().to_bytes() == name.as_bytes())
        .map(|ca| ca.dev_idx as usize)
}

pub const CMD: super::CmdDef = raw_cmd!("set-fs-option", "Set filesystem options", cmd_set_option);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconcile_marker_bit_is_nonzero_and_stable() {
        let pass = 1u64 << RecoveryPass::set_fs_needs_reconcile as u64;
        let want = passes_to_stable(pass);
        assert_ne!(want, 0);
        assert_eq!(with_marker_bit(0, true), want);
    }

    #[test]
    fn reconcile_marker_set_is_idempotent_and_keeps_other_bits() {
        let once = with_marker_bit(1, true);
        assert_eq!(with_marker_bit(once, true), once);
        assert_eq!(once & 1, 1);
    }

    #[test]
    fn reconcile_marker_restore_clears_only_our_bit() {
        let bit = reconcile_stable_bit();
        assert_eq!(with_marker_bit(bit | 1, false), 1);
        assert_eq!(with_marker_bit(1, true) & 1, 1);
    }
}
