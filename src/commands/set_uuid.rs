use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use crate::wrappers::fs::FsExt;
use bcachefs_kernel::c;
use bcachefs_kernel::fs::Fs;
use clap::Parser;
use uuid::Uuid;

use crate::wrappers::handle::BcachefsHandle;

#[derive(Parser, Debug)]
#[command(about = "Set the user-visible filesystem UUID on an unmounted filesystem")]
pub struct Cli {
    /// New user-visible filesystem UUID, as reported by blkid and UUID= mounts
    uuid: Uuid,

    /// Any member device path, or an explicit colon-separated member list
    device: String,
}

fn cmd_set_fs_uuid(cli: Cli) -> Result<()> {
    if cli.uuid == Uuid::nil() {
        bail!("refusing to set nil filesystem UUID");
    }

    let scan_opts = c::bch_opts::default();
    let mut sbs = crate::device_scan::scan_sbs(&cli.device, &scan_opts)
        .context("scanning filesystem devices")?;

    if sbs.is_empty() && !cli.device.contains(':') && !cli.device.contains('=') {
        if let Ok(sb) = crate::device_scan::read_super_silent(Path::new(&cli.device), scan_opts) {
            sbs.push((PathBuf::from(&cli.device), sb));
        }
    }

    if sbs.is_empty() {
        bail!("no bcachefs superblocks found");
    }

    let old_uuid = sbs[0].1.sb().uuid();
    let expected = sbs[0].1.sb().number_of_devices() as usize;
    if sbs.iter().any(|(_, sb)| sb.sb().uuid() != old_uuid) {
        bail!("not all supplied devices belong to the same filesystem");
    }
    if sbs.len() != expected {
        bail!(
            "refusing to change UUID with {}/{} member devices present; pass all members explicitly",
            sbs.len(),
            expected,
        );
    }
    if old_uuid == cli.uuid {
        bail!("filesystem already has UUID {}", cli.uuid);
    }

    let devs: Vec<PathBuf> = sbs.into_iter().map(|(p, _)| p).collect();
    if BcachefsHandle::open_if_mounted_any(&devs)
        .context("checking whether filesystem is mounted")?
        .is_some()
    {
        bail!("filesystem is mounted; set-fs-uuid only operates offline");
    }

    let mut fs_opts = c::bch_opts::default();
    fs_opts.set_nostart(true);
    fs_opts.set_will_not_start(true);

    let fs = Fs::open(&devs, fs_opts).context("opening filesystem offline")?;

    {
        let mut sb = fs.sb_lock();
        sb.sb_mut().user_uuid.b = *cli.uuid.as_bytes();
        fs.write_super_ret(&mut sb).context("writing updated superblock")?;
    }
    drop(fs);

    // write_super_ret() succeeds when enough members took the write, or when
    // nothing needed writing: confirm every member now carries the new UUID.
    let stale: Vec<_> = devs.iter()
        .filter(|dev| crate::device_scan::read_super_silent(dev, c::bch_opts::default())
            .map_or(true, |sb| sb.sb().uuid() != cli.uuid))
        .map(|dev| dev.display().to_string())
        .collect();
    if !stale.is_empty() {
        bail!("superblock UUID was not updated on: {}; rerun with {} to retry",
              stale.join(", "), cli.uuid);
    }

    println!("Changed filesystem UUID from {} to {}", old_uuid, cli.uuid);
    Ok(())
}

pub const CMD: super::CmdDef = typed_cmd!("set-fs-uuid", "Set filesystem UUID", Cli, cmd_set_fs_uuid);
