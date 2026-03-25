use clap::{Command, CommandFactory, Subcommand};

/// Command groups for help output and documentation generation.
/// Each entry: (group heading, list of top-level subcommand names).
pub const COMMAND_GROUPS: &[(&str, &[&str])] = &[
    ("Superblock commands", &[
        "format", "show-super", "recover-super",
        "set-fs-option", "reset-counters", "strip-alloc",
    ]),
    ("Images", &["image"]),
    ("Mount", &["mount"]),
    ("Repair", &["fsck", "recovery-pass"]),
    ("Running filesystem", &["fs"]),
    ("Devices", &["device"]),
    ("Subvolumes and snapshots", &["subvolume"]),
    ("Filesystem data", &["reconcile", "scrub"]),
    ("Encryption", &["unlock", "set-passphrase", "remove-passphrase"]),
    ("Migrate", &["migrate", "migrate-superblock"]),
    ("File options", &["set-file-option", "reflink-option-propagate"]),
    ("Debug", &["dump", "undump", "list", "list_journal", "kill_btree_node", "data-read", "unpoison"]),
    ("Miscellaneous", &["completions", "version"]),
];

pub mod attr;
pub mod completions;
pub mod counters;
pub mod device;
pub mod dump;
pub mod format;
pub mod format_util;
pub mod fs_usage;
pub mod image;
pub mod fsck;
pub mod key;
pub mod kill_btree_node;
pub mod list;
pub mod list_journal;
pub mod migrate;
pub mod mount;
pub mod opts;
pub mod data_read;
pub mod unpoison;
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
pub mod fusemount;

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
pub use migrate::{cmd_migrate, cmd_migrate_superblock};
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
            .long_about("\
Reset persistent counters stored in the superblock. Operates on an unmounted \
device. By default all counters are reset; use --counters to reset specific \
ones. See <<sec:counters>> for a list of available counters.")
            .arg(clap::Arg::new("fs").required(true)))
        .subcommand(Command::new("version")
            .about("Display version"));

    // Additional commands not in the derive-based Cli above
    // (list, mount, completions, subvolume come from Subcommands derive)
    cmd = cmd
        .subcommand(Command::new("data").about("Manage filesystem data")
            .subcommand(scrub::Cli::command().name("scrub")))
        .subcommand(Command::new("device").about("Manage devices within a filesystem")
            .subcommand(device::device_add_cmd()
                .long_about("\
Formats a new device and adds it to a running filesystem. The device \
is formatted with matching block size and btree node size, then \
joined to the filesystem. Filesystem options such as --discard and \
--durability can be set per-device."))
            .subcommand(device::OnlineCli::command().name("online"))
            .subcommand(device::OfflineCli::command().name("offline"))
            .subcommand(device::RemoveCli::command().name("remove")
                .long_about("\
Removes a device from a filesystem. Without --force, removal will \
fail if the device still has data that hasn't been evacuated---it is \
always safe to run without --force. Use --force (-f) to allow data \
loss, or --force-metadata (-F) to also allow metadata loss."))
            .subcommand(device::EvacuateCli::command().name("evacuate")
                .long_about("\
Sets the device state to evacuating and waits for reconcile to move \
all data off the device. Multiple devices can be evacuated \
simultaneously. Progress is displayed as remaining data on the \
device. Once evacuation completes, the device can be removed."))
            .subcommand(device::SetStateCli::command().name("set-state")
                .long_about("\
Device states: rw (read-write, normal operation), ro (read-only, no \
new allocations), evacuating (reconcile actively moves data off), \
spare (not used for any data). State changes interact with \
reconcile: setting a device to evacuating triggers data migration, \
and setting it to ro prevents new writes while keeping existing data \
accessible.\n\n\
Devices are automatically set to ro if too many IO errors are \
detected, preventing further damage. Use -o to change the state of \
an offline device."))
            .subcommand(device::ResizeCli::command().name("resize")
                .long_about("\
Resizes the filesystem on a device, either growing or shrinking to \
match a new device size. If no size is given, grows to fill the \
entire device (useful after expanding the underlying block device \
or partition). Can be run on a mounted filesystem."))
            .subcommand(device::ResizeJournalCli::command().name("resize-journal")
                .long_about("\
Resizes the journal on a device. A larger journal allows more \
write batching and reduces metadata write amplification, improving \
runtime performance. However, recovery currently scans the journal \
linearly, so a larger journal means longer mount times after an \
unclean shutdown.")))
        .subcommand(dump::DumpCli::command().name("dump")
            .long_about("\
Dumps filesystem metadata (not data) to a qcow2 sparse image. The \
resulting file is typically small enough to attach to bug reports, \
and can be inspected offline with bcachefs list or restored with \
bcachefs undump. Use -s to sanitize: -s data zeros inline data \
extents, -s filenames also scrambles directory entry names. \
Sanitized dumps are safe to share publicly."))
        .subcommand(Command::new("format").visible_alias("mkfs")
            .about("Format a new filesystem")
            .long_about("\
Formats one or more devices as a new bcachefs filesystem. Supports \
multi-device filesystems with configurable replication, erasure \
coding, encryption (chacha20/poly1305), and compression. All \
filesystem options from <<sec:options>> can be set at format \
time. Per-device options (labels, durability, data_allowed) can be \
set independently for each device."))
        .subcommand(Command::new("fs").about("Manage a running filesystem")
            .subcommand(fs_usage::Cli::command())
            .subcommand(top::Cli::command().name("top")
                .long_about("\
Interactive TUI showing live filesystem performance counters: IO rates, \
btree operations, journal writes, and more. Counters are grouped by \
category and displayed as a delta/second. Press 'q' to exit, up/down to \
scroll. See <<sec:counters>> for counter descriptions."))
            .subcommand(timestats::Cli::command().name("timestats")
                .long_about("\
Interactive TUI showing latency and frequency statistics for filesystem \
operations: reads, writes, btree node allocation, journal flushes, and \
more. Displays min/max/mean/stddev with quantile breakdowns. Press 'q' \
to exit, up/down to scroll, 'c' to clear. See <<sec:timestats>> for \
the full list of tracked operations.")))
        .subcommand(fsck::FsckCli::command().name("fsck")
            .long_about("\
Runs both online and offline filesystem checks. When given a mountpoint \
or a device that is already mounted, fsck runs online via ioctl. When \
given an unmounted device, it runs offline. The in-kernel fsck \
implementation is preferred when the kernel and filesystem versions \
match; use -k to force kernel fsck or -K to force userspace.\n\n\
Not all fsck passes are available online yet; offline fsck runs the \
full set of recovery passes."))
        .subcommand(Command::new("image").about("Filesystem image commands")
            .long_about("\
bcachefs can generate compact, read-only filesystem images from a \
directory tree in a single command. Images support all filesystem \
features including compression, and are generally competitive with \
EROFS in size.\n\n\
By default, allocation metadata is stripped from the image and \
regenerated on first read-write mount (requires kernel 6.16+); \
regeneration is fast enough to be unnoticeable on typical image \
sizes. Use --keep-alloc to preserve it.")
            .subcommand(Command::new("create").about("Create a filesystem image")
                .long_about("\
Creates a new filesystem image file from a source directory. The image \
is written sequentially using a temporary metadata device, then \
compacted and truncated to its final size."))
            .subcommand(image::ImageUpdateCli::command().name("update")
                .long_about("\
Updates an existing filesystem image to match a source directory with \
minimal on-disk changes. Only modified data is rewritten, making this \
suitable for delta transfer workflows (e.g. rsync the image after \
update).")))
        .subcommand(kill_btree_node::KillBtreeNodeCli::command().name("kill_btree_node"))
        .subcommand(list_journal::Cli::command().name("list_journal")
            .long_about("\
Lists journal entries from an unmounted filesystem. The journal is \
bcachefs's write-ahead log, recording every btree modification as a \
sequence of keyed entries. Output includes entry headers (sequence \
number, byte/sector size, version, last_seq, flush status, device \
locations) and optionally the contained btree keys.\n\n\
Filtering options narrow output to specific btrees (-b), key ranges \
(-k), transaction functions (-t), or sequence ranges (-s). Use \
-D for a compact timeline showing only datetime stamps, -H for \
headers only, or -L for log-message-only transactions. Blacklisted \
entries (from aborted journal replays) are hidden by default; use \
-B to include them."))
        .subcommand(Command::new("migrate")
            .about("Migrate an existing filesystem to bcachefs in place")
            .long_about("\
Converts an existing filesystem to bcachefs in place without copying \
data. Supported source filesystems: ext2/3/4, XFS, and single-device \
bcachefs. bcachefs metadata is written into free space alongside the \
existing data, and a bcachefs superblock is created at a non-default \
location. Run bcachefs migrate-superblock afterwards to move the \
superblock to the standard location.\n\n\
btrfs is not currently supported because its FIEMAP implementation \
does not report which device an extent resides on."))
        .subcommand(migrate::MigrateSuperblockCli::command().name("migrate-superblock")
            .long_about("\
After an in-place migration, the bcachefs superblock is at a \
non-default offset (to avoid overwriting the ext superblock). This \
command creates a standard superblock at the default location so the \
filesystem can be mounted normally."))
        .subcommand(Command::new("reconcile").about("Reconcile filesystem data")
            .long_about("\
Reconcile is the background process that ensures all data and metadata \
matches configured IO path options: replication count, checksum type, \
compression, erasure coding, and device targets. Work enters the \
system when filesystem or inode options change, when devices are \
added or removed, or when data is written with options that don't \
match the current configuration.")
            .subcommand(reconcile::StatusCli::command().name("status")
                .long_about("\
Shows pending reconcile work broken down by category, with data and \
metadata sectors remaining for each. Categories: replicas (replication \
count), checksum (checksum type), compression, erasure_code, target \
(device/target group placement), stripes (incomplete EC stripes), and \
high_priority (urgent work such as device evacuation). The pending \
category tracks work that cannot currently be fulfilled---for example, \
replicas=3 on a 2-device filesystem---and is automatically retried \
when filesystem configuration changes (e.g. a device is added).\n\n\
Scan pending indicates whether reconcile still needs to scan the \
extents btree for new work. The reconcile thread status section shows \
whether the thread is actively processing, waiting for IO clock \
pacing, or idle. While processing, progress indicators show IO wait \
duration and remaining work."))
            .subcommand(reconcile::WaitCli::command().name("wait")
                .long_about("\
Waits for all pending reconcile work to complete, displaying live \
progress. In a terminal, shows an interactive TUI with per-category \
counters updating as work is processed; press 'q' to exit early. \
In non-interactive mode (pipes, scripts), polls silently and exits \
when complete.\n\n\
By default waits on all categories except pending (since pending \
work may be impossible to complete with current configuration). \
Use -t to wait on specific categories only.\n\n\
Note: with erasure coding enabled, there will always be some data \
in the stripes category from incomplete stripes waiting for \
foreground writes to fill them. This is normal as long as it stays \
in the tens-of-megabytes range. Currently this prevents reconcile \
wait from exiting on EC-enabled filesystems; use -t to wait on \
specific categories instead.")))
        .subcommand(recover_super::RecoverSuperCli::command().name("recover-super")
            .long_about("\
Attempts to find and restore a bcachefs superblock from backup copies on \
the device. The device is scanned for valid superblock signatures and the \
most recently mounted copy is selected. In multi-device filesystems, \
the superblock can also be recovered from another member device using \
--src_device."))
        .subcommand(recovery_pass::RecoveryPassCli::command().name("recovery-pass")
            .long_about("\
Lists recovery passes scheduled to run at next mount, and allows \
forcing specific passes to run or clearing scheduled passes. \
Recovery passes handle everything from btree topology repair to \
allocation consistency checks; see the recovery passes table for \
the full list."))
        .subcommand(scrub::Cli::command().name("scrub")
            .long_about("\
Reads all data blocks on a running filesystem and verifies their \
checksums. When a checksum mismatch is found and a valid redundant \
copy exists (from replication or erasure coding), the corrupted copy \
is repaired automatically."))
        .subcommand(set_option::set_option_cmd())
        .subcommand(key::SetPassphraseCli::command().name("set-passphrase"))
        .subcommand(key::RemovePassphraseCli::command().name("remove-passphrase"))
        .subcommand(super_cmd::ShowSuperCli::command().name("show-super")
            .long_about("\
Prints superblock information for a bcachefs device. By default shows \
the core superblock header, member device information, and error \
counters. Use -f to select specific superblock sections (comma-separated) \
or -f all to display every section. Use -F for scripting: prints a single \
named field with no header. Use --layout to display the superblock layout \
(backup locations and sizes)."))
        .subcommand(key::UnlockCli::command().name("unlock")
            .long_about("\
Loads the encryption key for a bcachefs filesystem into the kernel \
keyring, allowing subsequent mount or fsck operations without a \
passphrase prompt. The mount command does this automatically when \
needed, so unlock is primarily useful for scripted workflows or \
when running fsck on an encrypted filesystem. Use -c to check \
whether a device is encrypted without unlocking. Use -f to read \
the passphrase from a file instead of prompting interactively."))
        .subcommand(Command::new("strip-alloc")
            .about("Strip alloc info on a filesystem to be used read-only")
            .long_about("\
Removes allocation metadata and journal from a filesystem intended for \
read-only use. Alloc info is regenerated on first read-write mount. \
Currently unsafe on large (multi-TB) filesystems due to memory \
requirements of alloc info reconstruction; this limitation will be \
lifted when online check_allocations is complete."))
        .subcommand(dump::UndumpCli::command().name("undump"));

    cmd
}
