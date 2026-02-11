use clap::{Command, CommandFactory, Subcommand};

pub mod attr;
pub mod completions;
pub mod counters;
pub mod device;
pub mod fs_usage;
pub mod key;
pub mod list;
pub mod mount;
pub mod opts;
pub mod reconcile;
pub mod scrub;
pub mod subvolume;
pub mod super_cmd;
pub mod timestats;
pub mod top;

pub use completions::completions;
pub use attr::cmd_setattr;
pub use attr::cmd_reflink_option_propagate;
pub use counters::cmd_reset_counters;
pub use device::{
    cmd_device_online, cmd_device_offline, cmd_device_remove, cmd_device_evacuate,
    cmd_device_set_state, cmd_device_resize, cmd_device_resize_journal,
};
pub use key::{cmd_unlock, cmd_set_passphrase, cmd_remove_passphrase};
pub use list::list;
pub use mount::mount;
pub use reconcile::{cmd_reconcile_status, cmd_reconcile_wait};
pub use scrub::scrub;
pub use subvolume::subvolume;
pub use timestats::timestats;
pub use top::top;

#[derive(clap::Parser, Debug)]
#[command(name = "bcachefs")]
pub struct Cli {
    #[command(subcommand)]
    subcommands: Subcommands,
}

#[derive(Subcommand, Debug)]
enum Subcommands {
    List(list::Cli),
    Mount(mount::Cli),
    Completions(completions::Cli),
    #[command(visible_aliases = ["subvol"])]
    Subvolume(subvolume::Cli),
}

/// Build full command tree for completions and help.
/// Includes both Rust commands (with full arg specs) and C commands (stubs).
pub fn build_cli() -> Command {
    let mut cmd = Cli::command();

    // Rust commands with full Clap specs
    cmd = cmd
        .subcommand(attr::setattr_cmd())
        .subcommand(attr::reflink_option_propagate_cmd())
        .subcommand(Command::new("reset-counters")
            .about("Reset filesystem counters")
            .arg(clap::Arg::new("fs").required(true)))
        .subcommand(Command::new("version")
            .about("Display version"));

    // C commands — stubs for completions/help
    // (list, mount, completions, subvolume already come from the derive-based Cli)
    cmd = cmd
        .subcommand(Command::new("data").about("Manage filesystem data")
            .subcommand(scrub::Cli::command().name("scrub")))
        .subcommand(Command::new("device").about("Manage devices within a filesystem")
            .subcommand(device::OnlineCli::command().name("online"))
            .subcommand(device::OfflineCli::command().name("offline"))
            .subcommand(device::RemoveCli::command().name("remove"))
            .subcommand(device::EvacuateCli::command().name("evacuate"))
            .subcommand(device::SetStateCli::command().name("set-state"))
            .subcommand(device::ResizeCli::command().name("resize"))
            .subcommand(device::ResizeJournalCli::command().name("resize-journal")))
        .subcommand(Command::new("dump").about("Dump filesystem metadata to a qcow2 image"))
        .subcommand(Command::new("format").visible_alias("mkfs")
            .about("Format a new filesystem"))
        .subcommand(Command::new("fs").about("Manage a running filesystem")
            .subcommand(fs_usage::Cli::command())
            .subcommand(top::Cli::command().name("top"))
            .subcommand(timestats::Cli::command().name("timestats")))
        .subcommand(Command::new("fsck").about("Check an existing filesystem for errors"))
        .subcommand(Command::new("image").about("Filesystem image commands"))
        .subcommand(Command::new("migrate")
            .about("Migrate an existing ext2/3/4 filesystem to bcachefs in place"))
        .subcommand(Command::new("reconcile").about("Reconcile filesystem data")
            .subcommand(reconcile::StatusCli::command().name("status"))
            .subcommand(reconcile::WaitCli::command().name("wait")))
        .subcommand(Command::new("recovery-pass")
            .about("Run a specific recovery pass"))
        .subcommand(Command::new("set-fs-option")
            .about("Set a filesystem option"))
        .subcommand(key::SetPassphraseCli::command().name("set-passphrase"))
        .subcommand(key::RemovePassphraseCli::command().name("remove-passphrase"))
        .subcommand(super_cmd::ShowSuperCli::command().name("show-super"))
        .subcommand(key::UnlockCli::command().name("unlock"));

    cmd
}
