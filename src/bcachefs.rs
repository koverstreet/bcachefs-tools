mod commands;
mod key;
mod dump_stack;
mod logging;
mod util;
mod wrappers;
mod device_scan;
mod http;

use std::{
    ffi::{c_char, CString},
    process::{ExitCode, Termination},
};

use bch_bindgen::c;
use log::debug;

#[derive(Debug)]
pub struct ErrnoError(pub errno::Errno);
impl std::fmt::Display for ErrnoError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
        self.0.fmt(f)
    }
}

impl std::error::Error for ErrnoError {}

/// Print main bcachefs usage, with commands grouped by category.
/// Descriptions are pulled from the clap command tree (build_cli).
fn bcachefs_usage() {
    let cmd = commands::build_cli();

    let groups: &[(&str, &[&str])] = &[
        ("Superblock commands:", &[
            "format", "show-super", "recover-super",
            "set-fs-option", "reset-counters", "strip-alloc",
        ]),
        ("Images:", &["image"]),
        ("Mount:", &["mount"]),
        ("Repair:", &["fsck", "recovery-pass"]),
        ("Running filesystem:", &["fs"]),
        ("Devices:", &["device"]),
        ("Subvolumes and snapshots:", &["subvolume"]),
        ("Filesystem data:", &["reconcile", "scrub"]),
        ("Encryption:", &["unlock", "set-passphrase", "remove-passphrase"]),
        ("Migrate:", &["migrate", "migrate-superblock"]),
        ("File options:", &["set-file-option", "reflink-option-propagate"]),
        ("Debug:", &["dump", "undump", "list", "list_journal", "kill_btree_node"]),
        ("Miscellaneous:", &["completions", "version"]),
    ];

    println!("bcachefs - tool for managing bcachefs filesystems");
    println!("usage: bcachefs <command> [<args>]\n");

    for (heading, names) in groups {
        println!("{heading}");
        for name in *names {
            let Some(sub) = cmd.find_subcommand(name) else { continue };
            let children: Vec<_> = sub.get_subcommands()
                .filter(|c| c.get_name() != "help")
                .collect();
            if !children.is_empty() {
                for child in children {
                    let about = child.get_about().map(|s| s.to_string()).unwrap_or_default();
                    let full = format!("{name} {}", child.get_name());
                    println!("  {full:<26}{about}");
                }
            } else {
                let about = sub.get_about().map(|s| s.to_string()).unwrap_or_default();
                println!("  {:<26}{about}", name);
            }
        }
        println!();
    }
}

/// Print usage for a subcommand group (device, fs, data, reconcile, etc.)
/// by pulling subcommand names and descriptions from the clap tree.
fn group_usage(group: &str) {
    let cmd = commands::build_cli();
    let Some(sub) = cmd.find_subcommand(group) else { return };
    let about = sub.get_about().map(|s| s.to_string()).unwrap_or_default();
    println!("bcachefs {group} - {about}");
    println!("Usage: bcachefs {group} <command> [OPTION]...\n");
    println!("Commands:");
    for child in sub.get_subcommands() {
        if child.get_name() == "help" { continue }
        let child_about = child.get_about().map(|s| s.to_string()).unwrap_or_default();
        println!("  {:<26}{child_about}", child.get_name());
    }
}

fn c_command(args: Vec<String>, symlink_cmd: Option<&str>) -> ExitCode {
    let r = handle_c_command(args, symlink_cmd);
    debug!("return code from C command: {r}");
    ExitCode::from(r as u8)
}

fn handle_c_command(mut argv: Vec<String>, symlink_cmd: Option<&str>) -> i32 {
    let cmd = match symlink_cmd {
        Some(s) => s.to_string(),
        None => argv.remove(1),
    };

    let argc: i32 = argv.len().try_into().unwrap();

    let argv: Vec<_> = argv.into_iter().map(|s| CString::new(s).unwrap()).collect();
    let mut argv = argv
        .into_iter()
        .map(|s| Box::into_raw(s.into_boxed_c_str()).cast::<c_char>())
        .collect::<Box<[*mut c_char]>>();
    let argv = argv.as_mut_ptr();

    // The C functions will mutate argv. It shouldn't be used after this block.
    unsafe {
        match cmd.as_str() {
            "dump"              => c::cmd_dump(argc, argv),
            "image"             => c::image_cmds(argc, argv),
            "kill_btree_node"   => c::cmd_kill_btree_node(argc, argv),
            "migrate"           => c::cmd_migrate(argc, argv),
            "migrate-superblock" => c::cmd_migrate_superblock(argc, argv),
            #[cfg(feature = "fuse")]
            "fusemount"         => c::cmd_fusemount(argc, argv),
            _ => { println!("Unknown command {cmd}"); bcachefs_usage(); 1 }
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();

    let symlink_cmd: Option<&str> = if args[0].contains("mkfs") {
        Some("mkfs")
    } else if args[0].contains("fsck") {
        Some("fsck")
    } else if args[0].contains("mount.fuse") {
        Some("fusemount")
    } else if args[0].contains("mount") {
        Some("mount")
    } else {
        None
    };

    if symlink_cmd.is_none() && args.len() < 2 {
        println!("missing command");
        bcachefs_usage();
        return ExitCode::from(1);
    }

    unsafe { c::raid_init() };

    let cmd = match symlink_cmd {
        Some(s) => s,
        None => args[1].as_str(),
    };

    // fuse will call this after daemonizing, we can't create threads before - note that mount
    // may invoke fusemount, via -t bcachefs.fuse
    if cmd != "mount" && cmd != "fusemount" {
        unsafe { c::linux_shrinkers_init() };
    }

    match cmd {
        "--help" | "help" => {
            bcachefs_usage();
            ExitCode::SUCCESS
        }
        "version" => {
            let vh = include_str!("../version.h");
            println!("{}", vh.split('"').nth(1).unwrap_or("unknown"));
            ExitCode::SUCCESS
        }
        "completions" => {
            commands::completions(args[1..].to_vec());
            ExitCode::SUCCESS
        }
        "list" => commands::list(args[1..].to_vec()).report(),
        "list_journal" => commands::cmd_list_journal(args[1..].to_vec()).report(),
        "mount" => commands::mount(args, symlink_cmd),
        "scrub" => commands::scrub(args[1..].to_vec()).report(),
        "subvolume" => commands::subvolume(args[1..].to_vec()).report(),
        "data" => match args.get(2).map(|s| s.as_str()) {
            Some("scrub") => commands::scrub(args[2..].to_vec()).report(),
            _ => { group_usage("data"); ExitCode::from(1) }
        },
        "device" => match args.get(2).map(|s| s.as_str()) {
            Some("add") => commands::cmd_device_add(args[2..].to_vec()).report(),
            Some("online") => commands::cmd_device_online(args[2..].to_vec()).report(),
            Some("offline") => commands::cmd_device_offline(args[2..].to_vec()).report(),
            Some("remove") => commands::cmd_device_remove(args[2..].to_vec()).report(),
            Some("evacuate") => commands::cmd_device_evacuate(args[2..].to_vec()).report(),
            Some("set-state") => commands::cmd_device_set_state(args[2..].to_vec()).report(),
            Some("resize") => commands::cmd_device_resize(args[2..].to_vec()).report(),
            Some("resize-journal") => commands::cmd_device_resize_journal(args[2..].to_vec()).report(),
            _ => { group_usage("device"); ExitCode::SUCCESS }
        },
        "format" | "mkfs" => {
            let argv = if symlink_cmd.is_some() { args.clone() } else { args[1..].to_vec() };
            commands::cmd_format(argv).report()
        }
        "fsck" => {
            let argv = if symlink_cmd.is_some() { args.clone() } else { args[1..].to_vec() };
            commands::cmd_fsck(argv).report()
        }
        "fs" => match args.get(2).map(|s| s.as_str()) {
            Some("timestats") => commands::timestats(args[2..].to_vec()).report(),
            Some("top") => commands::top(args[2..].to_vec()).report(),
            Some("usage") => commands::fs_usage::fs_usage(args[2..].to_vec()).report(),
            _ => { group_usage("fs"); ExitCode::from(1) }
        },
        "remove-passphrase" => commands::cmd_remove_passphrase(args[1..].to_vec()).report(),
        "reset-counters" => commands::cmd_reset_counters(args[1..].to_vec()).report(),
        "recovery-pass" => commands::cmd_recovery_pass(args[1..].to_vec()).report(),
        "reconcile" => match args.get(2).map(|s| s.as_str()) {
            Some("status") => commands::cmd_reconcile_status(args[2..].to_vec()).report(),
            Some("wait") => commands::cmd_reconcile_wait(args[2..].to_vec()).report(),
            _ => { group_usage("reconcile"); ExitCode::from(1) }
        },
        "undump" => commands::cmd_undump(args[1..].to_vec()).report(),
        "recover-super" => commands::cmd_recover_super(args[1..].to_vec()).report(),
        "show-super" => commands::super_cmd::cmd_show_super(args[1..].to_vec()).report(),
        "strip-alloc" => commands::cmd_strip_alloc(args[1..].to_vec()).report(),
        "set-file-option" => commands::cmd_setattr(args[1..].to_vec()).report(),
        "set-fs-option" => commands::cmd_set_option(args[1..].to_vec()).report(),
        "set-passphrase" => commands::cmd_set_passphrase(args[1..].to_vec()).report(),
        "reflink-option-propagate" => commands::cmd_reflink_option_propagate(args[1..].to_vec()).report(),
        "unlock" => commands::cmd_unlock(args[1..].to_vec()).report(),
        _ => c_command(args, symlink_cmd),
    }
}
