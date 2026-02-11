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
            "--help" => {
                c::bcachefs_usage();
                0
            }
            "device" => c::device_cmds(argc, argv),
            "dump" => c::cmd_dump(argc, argv),
            "undump" => c::cmd_undump(argc, argv),
            "format" => c::cmd_format(argc, argv),
            // fs subcommand dispatch is fully in Rust now
            "fsck" => c::cmd_fsck(argc, argv),
            "recovery-pass" => c::cmd_recovery_pass(argc, argv),
            "image" => c::image_cmds(argc, argv),
            "list_journal" => c::cmd_list_journal(argc, argv),
            "kill_btree_node" => c::cmd_kill_btree_node(argc, argv),
            "migrate" => c::cmd_migrate(argc, argv),
            "migrate-superblock" => c::cmd_migrate_superblock(argc, argv),
            "mkfs" => c::cmd_format(argc, argv),
            // reconcile handled in Rust dispatch
            // remove-passphrase handled in Rust dispatch
            // reset-counters handled in Rust dispatch
            "set-fs-option" => c::cmd_set_option(argc, argv),
            // set-passphrase handled in Rust dispatch
            // set-file-option handled in Rust dispatch
            // show-super handled in Rust dispatch
            "recover-super" => c::cmd_recover_super(argc, argv),
            "strip-alloc" => c::cmd_strip_alloc(argc, argv),
            // unlock handled in Rust dispatch
            #[cfg(feature = "fuse")]
            "fusemount" => c::cmd_fusemount(argc, argv),

            _ => {
                println!("Unknown command {cmd}");
                c::bcachefs_usage();
                1
            }
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
        unsafe { c::bcachefs_usage() };
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
        "mount" => commands::mount(args, symlink_cmd),
        "scrub" => commands::scrub(args[1..].to_vec()).report(),
        "subvolume" => commands::subvolume(args[1..].to_vec()).report(),
        "data" => match args.get(2).map(|s| s.as_str()) {
            Some("scrub") => commands::scrub(args[2..].to_vec()).report(),
            _ => {
                println!("bcachefs data - manage filesystem data");
                println!("Usage: bcachefs data <scrub> [OPTION]...\n");
                println!("Commands:");
                println!("  scrub                        Verify checksums and correct errors");
                ExitCode::from(1)
            }
        },
        "device" => match args.get(2).map(|s| s.as_str()) {
            Some("online") => commands::cmd_device_online(args[2..].to_vec()).report(),
            Some("offline") => commands::cmd_device_offline(args[2..].to_vec()).report(),
            Some("remove") => commands::cmd_device_remove(args[2..].to_vec()).report(),
            Some("evacuate") => commands::cmd_device_evacuate(args[2..].to_vec()).report(),
            Some("set-state") if !args.iter().any(|a| a == "--offline" || a == "-o") =>
                commands::cmd_device_set_state(args[2..].to_vec()).report(),
            Some("resize") => match commands::cmd_device_resize(args[2..].to_vec()) {
                Ok(true) => ExitCode::SUCCESS,
                Ok(false) => c_command(args, symlink_cmd),
                Err(e) => { eprintln!("Error: {e:#}"); ExitCode::FAILURE }
            },
            Some("resize-journal") => match commands::cmd_device_resize_journal(args[2..].to_vec()) {
                Ok(true) => ExitCode::SUCCESS,
                Ok(false) => c_command(args, symlink_cmd),
                Err(e) => { eprintln!("Error: {e:#}"); ExitCode::FAILURE }
            },
            _ => c_command(args, symlink_cmd),
        },
        "fs" => match args.get(2).map(|s| s.as_str()) {
            Some("timestats") => commands::timestats(args[2..].to_vec()).report(),
            Some("top") => commands::top(args[2..].to_vec()).report(),
            Some("usage") => commands::fs_usage::fs_usage(args[2..].to_vec()).report(),
            _ => {
                println!("bcachefs fs - manage a running filesystem");
                println!("Usage: bcachefs fs <usage|top|timestats> [OPTION]...\n");
                println!("Commands:");
                println!("  usage                        Display detailed filesystem usage");
                println!("  top                          Show runtime performance information");
                println!("  timestats                    Show filesystem time statistics");
                ExitCode::from(1)
            }
        },
        "remove-passphrase" => commands::cmd_remove_passphrase(args[1..].to_vec()).report(),
        "reset-counters" => commands::cmd_reset_counters(args[1..].to_vec()).report(),
        "reconcile" => match args.get(2).map(|s| s.as_str()) {
            Some("status") => commands::cmd_reconcile_status(args[2..].to_vec()).report(),
            Some("wait") => commands::cmd_reconcile_wait(args[2..].to_vec()).report(),
            _ => {
                println!("bcachefs reconcile - manage data reconcile");
                println!("Usage: bcachefs reconcile <status|wait> [OPTION]...\n");
                println!("Commands:");
                println!("  status                       Show status of background data processing");
                println!("  wait                         Wait on background data processing to complete");
                ExitCode::from(1)
            }
        },
        "show-super" => commands::super_cmd::cmd_show_super(args[1..].to_vec()).report(),
        "set-file-option" => commands::cmd_setattr(args[1..].to_vec()).report(),
        "set-passphrase" => commands::cmd_set_passphrase(args[1..].to_vec()).report(),
        "reflink-option-propagate" => commands::cmd_reflink_option_propagate(args[1..].to_vec()).report(),
        "unlock" => commands::cmd_unlock(args[1..].to_vec()).report(),
        _ => c_command(args, symlink_cmd),
    }
}
