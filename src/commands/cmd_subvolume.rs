use std::path::PathBuf;

use bch_bindgen::c::BCH_SUBVOL_SNAPSHOT_RO;
use clap::{Parser, Subcommand};

use crate::wrappers::admin_handle::AdminHandle;

#[derive(Parser, Debug)]
pub struct Cli {
    #[command(subcommand)]
    subcommands: Subcommands,
}

/// Subvolumes-related commands
#[derive(Subcommand, Debug)]
enum Subcommands {
    Create {
        /// Paths
        targets: Vec<PathBuf>
    },
    Delete {
        /// Path
        target: PathBuf
    },
    Snapshot {
        /// Make snapshot read only
        #[arg(long, short = 'r')]
        read_only: bool,
        source: PathBuf,
        dest: PathBuf
    }
}

pub fn cmd_subvolumes(argv: Vec<String>) -> i32 {
    let args = Cli::parse_from(argv);

    match args.subcommands {
        Subcommands::Create { targets } => {
            for target in targets {
                if let Some(dirname) = target.parent() {
                    let fs = unsafe { AdminHandle::open(dirname) };
                    fs.create_subvolume(target).expect("Failed to create the subvolume");
                }
            }
        }
        ,
        Subcommands::Delete { target } => {
            if let Some(dirname) = target.parent() {
                let fs = unsafe { AdminHandle::open(dirname) };
                fs.delete_subvolume(target).expect("Failed to delete the subvolume");
            }
        },
        Subcommands::Snapshot { read_only, source, dest } => {
            if let Some(dirname) = dest.parent() {
                let fs = unsafe { AdminHandle::open(dirname) };

                fs.snapshot_subvolume(if read_only { BCH_SUBVOL_SNAPSHOT_RO } else { 0x0 }, source, dest).expect("Failed to snapshot the subvolume");
            }
        }
    }

    0
}
