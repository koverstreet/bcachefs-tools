use std::fs;

use anyhow::{Context, Result};
use clap::Parser;

use crate::commands::reconcile;
use crate::wrappers::handle::BcachefsHandle;
use crate::wrappers::sysfs;

/// Upgrade a mounted filesystem to the current incompatible feature set
#[derive(Parser, Debug)]
#[command(about = "Upgrade a mounted filesystem to the current incompatible feature set")]
pub struct UpdateCli {
    /// Filesystem mountpoint
    #[arg(default_value = ".")]
    filesystem: String,
}

fn write_version_upgrade(filesystem: &str, value: &str) -> Result<()> {
    let handle = BcachefsHandle::open(filesystem)
        .map_err(|e| anyhow::anyhow!("opening filesystem '{}': {}", filesystem, e))?;
    let sysfs_path = sysfs::sysfs_path_from_fd(handle.sysfs_fd())?;
    fs::write(sysfs_path.join("options/version_upgrade"), value)
        .with_context(|| format!("setting version_upgrade={value} on {filesystem}"))
}

fn cmd_update(cli: UpdateCli) -> Result<()> {
    println!("Allowing incompatible features for {}", cli.filesystem);
    write_version_upgrade(&cli.filesystem, "incompatible")?;

    println!("Waiting for reconcile work to finish");
    let wait_ret = reconcile::wait_for_all_except_pending(&cli.filesystem);

    println!("Disabling further version upgrades");
    let reset_ret = write_version_upgrade(&cli.filesystem, "none");

    wait_ret?;
    reset_ret?;

    Ok(())
}

pub const CMD: super::CmdDef = typed_cmd!(
    "update",
    "Upgrade a mounted filesystem",
    UpdateCli,
    cmd_update
);
