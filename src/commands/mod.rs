use clap::{Command, CommandFactory, Subcommand};

pub mod attr;
pub mod completions;
pub mod counters;
pub mod device;
pub mod dump;
pub mod format;
pub mod fs_usage;
pub mod image;
pub mod fsck;
pub mod key;
pub mod kill_btree_node;
pub mod list;
pub mod list_journal;
pub mod mount;
pub mod opts;
pub mod reconcile;
pub mod recover_super;
pub mod recovery_pass;
pub mod scrub;
pub mod set_option;
pub mod strip_alloc;
pub mod subvolume;
pub mod super_cmd;
pub mod timestats;
pub mod top;

pub use completions::completions;
pub use attr::cmd_setattr;
pub use attr::cmd_reflink_option_propagate;
pub use counters::cmd_reset_counters;
pub use device::{
    cmd_device_add,
    cmd_device_online, cmd_device_offline, cmd_device_remove, cmd_device_evacuate,
    cmd_device_set_state, cmd_device_resize, cmd_device_resize_journal,
};
pub use key::{cmd_unlock, cmd_set_passphrase, cmd_remove_passphrase};
pub use list::list;
pub use list_journal::cmd_list_journal;
pub use mount::mount;
pub use dump::cmd_dump;
pub use dump::cmd_undump;
pub use kill_btree_node::cmd_kill_btree_node;
pub use format::cmd_format;
pub use image::{cmd_image_create, cmd_image_update};
pub use fsck::cmd_fsck;
pub use reconcile::{cmd_reconcile_status, cmd_reconcile_wait};
pub use recover_super::cmd_recover_super;
pub use recovery_pass::cmd_recovery_pass;
pub use scrub::scrub;
pub use set_option::cmd_set_option;
pub use strip_alloc::cmd_strip_alloc;
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

    // Additional commands not in the derive-based Cli above
    // (list, mount, completions, subvolume come from Subcommands derive)
    cmd = cmd
        .subcommand(Command::new("data").about("Manage filesystem data")
            .subcommand(scrub::Cli::command().name("scrub")))
        .subcommand(Command::new("device").about("Manage devices within a filesystem")
            .subcommand(device::device_add_cmd())
            .subcommand(device::OnlineCli::command().name("online"))
            .subcommand(device::OfflineCli::command().name("offline"))
            .subcommand(device::RemoveCli::command().name("remove"))
            .subcommand(device::EvacuateCli::command().name("evacuate"))
            .subcommand(device::SetStateCli::command().name("set-state"))
            .subcommand(device::ResizeCli::command().name("resize"))
            .subcommand(device::ResizeJournalCli::command().name("resize-journal")))
        .subcommand(dump::DumpCli::command().name("dump"))
        .subcommand(Command::new("format").visible_alias("mkfs")
            .about("Format a new filesystem"))
        .subcommand(Command::new("fs").about("Manage a running filesystem")
            .subcommand(fs_usage::Cli::command())
            .subcommand(top::Cli::command().name("top"))
            .subcommand(timestats::Cli::command().name("timestats")))
        .subcommand(fsck::FsckCli::command().name("fsck"))
        .subcommand(Command::new("image").about("Filesystem image commands")
            .subcommand(Command::new("create").about("Create a filesystem image"))
            .subcommand(image::ImageUpdateCli::command().name("update")))
        .subcommand(kill_btree_node::KillBtreeNodeCli::command().name("kill_btree_node"))
        .subcommand(list_journal::Cli::command().name("list_journal"))
        .subcommand(Command::new("migrate")
            .about("Migrate an existing ext2/3/4 filesystem to bcachefs in place"))
        .subcommand(Command::new("migrate-superblock")
            .about("Migrate superblock to standard location"))
        .subcommand(Command::new("reconcile").about("Reconcile filesystem data")
            .subcommand(reconcile::StatusCli::command().name("status"))
            .subcommand(reconcile::WaitCli::command().name("wait")))
        .subcommand(recover_super::RecoverSuperCli::command().name("recover-super"))
        .subcommand(recovery_pass::RecoveryPassCli::command().name("recovery-pass"))
        .subcommand(scrub::Cli::command().name("scrub"))
        .subcommand(set_option::set_option_cmd())
        .subcommand(key::SetPassphraseCli::command().name("set-passphrase"))
        .subcommand(key::RemovePassphraseCli::command().name("remove-passphrase"))
        .subcommand(super_cmd::ShowSuperCli::command().name("show-super"))
        .subcommand(key::UnlockCli::command().name("unlock"))
        .subcommand(Command::new("strip-alloc")
            .about("Strip alloc info on a filesystem to be used read-only"))
        .subcommand(dump::UndumpCli::command().name("undump"));

    cmd
}
