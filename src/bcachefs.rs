mod commands;
mod key;
mod dump_stack;
mod logging;
mod wrappers;
mod device_scan;

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
            "data" => c::data_cmds(argc, argv),
            "device" => c::device_cmds(argc, argv),
            "dump" => c::cmd_dump(argc, argv),
            "undump" => c::cmd_undump(argc, argv),
            "format" => c::cmd_format(argc, argv),
            "fs" => c::fs_cmds(argc, argv),
            "fsck" => c::cmd_fsck(argc, argv),
            "recovery-pass" => c::cmd_recovery_pass(argc, argv),
            "image" => c::image_cmds(argc, argv),
            "list_journal" => c::cmd_list_journal(argc, argv),
            "kill_btree_node" => c::cmd_kill_btree_node(argc, argv),
            "migrate" => c::cmd_migrate(argc, argv),
            "migrate-superblock" => c::cmd_migrate_superblock(argc, argv),
            "mkfs" => c::cmd_format(argc, argv),
            "reconcile" => c::reconcile_cmds(argc, argv),
            "remove-passphrase" => c::cmd_remove_passphrase(argc, argv),
            "reset-counters" => c::cmd_reset_counters(argc, argv),
            "set-fs-option" => c::cmd_set_option(argc, argv),
            "set-passphrase" => c::cmd_set_passphrase(argc, argv),
            "set-file-option" => c::cmd_setattr(argc, argv),
            "show-super" => c::cmd_show_super(argc, argv),
            "recover-super" => c::cmd_recover_super(argc, argv),
            "strip-alloc" => c::cmd_strip_alloc(argc, argv),
            "unlock" => c::cmd_unlock(argc, argv),
            "version" => c::cmd_version(argc, argv),

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

    match cmd {
        "completions" => {
            commands::completions(args[1..].to_vec());
            ExitCode::SUCCESS
        }
        "list" => commands::list(args[1..].to_vec()).report(),
        "mount" => commands::mount(args, symlink_cmd),
        "subvolume" => commands::subvolume(args[1..].to_vec()).report(),
        _ => {
            let r = handle_c_command(args, symlink_cmd);

            debug!("return code from C command: {r}");
            ExitCode::from(r as u8)
        }
    }
}
