mod commands;
mod key;
mod wrappers;

use std::ffi::CString;

use bch_bindgen::c;
use commands::cmd_completions::cmd_completions;
use commands::cmd_list::cmd_list;
use commands::cmd_mount::cmd_mount;
use commands::cmd_subvolume::cmd_subvolumes;
use commands::logger::SimpleLogger;

#[derive(Debug)]
pub struct ErrnoError(pub errno::Errno);
impl std::fmt::Display for ErrnoError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
        self.0.fmt(f)
    }
}

impl std::error::Error for ErrnoError {}

fn to_mut<T>(p: *const T) -> *mut T {
    p as *mut T
}

fn handle_c_command(args: Vec<String>, symlink_cmd: Option<&str>) -> i32 {
    let mut argv: Vec<_> = args.clone();

    let cmd = match symlink_cmd {
        Some(s) => s.to_string(),
        None => argv.remove(1),
    };

    let argc: i32 = argv.len().try_into().unwrap();

    let argv: Vec<_> = argv
        .iter()
        .map(|s| CString::new(s.as_str()).unwrap())
        .collect();
    let mut argv: Vec<_> = argv.iter().map(|s| to_mut(s.as_ptr())).collect();
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
            "format" => c::cmd_format(argc, argv),
            "fs" => c::fs_cmds(argc, argv),
            "fsck" => c::cmd_fsck(argc, argv),
            "list_journal" => c::cmd_list_journal(argc, argv),
            "kill_btree_node" => c::cmd_kill_btree_node(argc, argv),
            "migrate" => c::cmd_migrate(argc, argv),
            "migrate-superblock" => c::cmd_migrate_superblock(argc, argv),
            "mkfs" => c::cmd_format(argc, argv),
            "remove-passphrase" => c::cmd_remove_passphrase(argc, argv),
            "reset-counters" => c::cmd_reset_counters(argc, argv),
            "set-option" => c::cmd_set_option(argc, argv),
            "set-passphrase" => c::cmd_set_passphrase(argc, argv),
            "setattr" => c::cmd_setattr(argc, argv),
            "show-super" => c::cmd_show_super(argc, argv),
            "unlock" => c::cmd_unlock(argc, argv),
            "version" => c::cmd_version(argc, argv),

            #[cfg(fuse)]
            "fusemount" => c::cmd_fusemount(argc, argv),

            _ => {
                println!("Unknown command {}", cmd);
                c::bcachefs_usage();
                1
            }
        }
    }
}

fn main() {
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
        std::process::exit(1);
    }

    unsafe { c::raid_init() };

    log::set_boxed_logger(Box::new(SimpleLogger)).unwrap();
    log::set_max_level(log::LevelFilter::Warn);

    let cmd = match symlink_cmd {
        Some(s) => s,
        None => args[1].as_str(),
    };

    let ret = match cmd {
        "completions" => cmd_completions(args[1..].to_vec()),
        "list" => cmd_list(args[1..].to_vec()),
        "mount" => cmd_mount(args, symlink_cmd),
        "subvolume" => cmd_subvolumes(args[1..].to_vec()),
        _ => handle_c_command(args, symlink_cmd),
    };

    if ret != 0 {
        std::process::exit(1);
    }
}
