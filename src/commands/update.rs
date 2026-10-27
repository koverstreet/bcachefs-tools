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

fn update_result(wait_ret: Result<()>, reset_ret: Result<()>) -> Result<()> {
    match (wait_ret, reset_ret) {
        (Err(wait_error), Err(reset_error)) => Err(wait_error.context(format!(
            "also failed to disable further version upgrades: {reset_error:#}"
        ))),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

fn cmd_update(cli: UpdateCli) -> Result<()> {
    println!("Allowing incompatible features for {}", cli.filesystem);
    write_version_upgrade(&cli.filesystem, "incompatible")?;

    println!("Waiting for reconcile work to finish");
    let wait_ret = reconcile::wait_for_all_except_pending(&cli.filesystem);

    println!("Disabling further version upgrades");
    let reset_ret = write_version_upgrade(&cli.filesystem, "none");

    update_result(wait_ret, reset_ret)
}

pub const CMD: super::CmdDef = typed_cmd!(
    "update",
    "Upgrade a mounted filesystem",
    UpdateCli,
    cmd_update
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_reports_wait_and_reset_failures() {
        let error = update_result(
            Err(anyhow::anyhow!("reconcile accounting failed")),
            Err(anyhow::anyhow!("version_upgrade write failed")),
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("reconcile accounting failed"));
        assert!(message.contains("version_upgrade write failed"));
    }

    #[test]
    fn update_requires_both_wait_and_reset_to_succeed() {
        assert!(update_result(Ok(()), Ok(())).is_ok());
        assert_eq!(
            update_result(Err(anyhow::anyhow!("wait failed")), Ok(()))
                .unwrap_err()
                .to_string(),
            "wait failed",
        );
        assert_eq!(
            update_result(Ok(()), Err(anyhow::anyhow!("reset failed")))
                .unwrap_err()
                .to_string(),
            "reset failed",
        );
    }
}
