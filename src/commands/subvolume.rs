use std::path::PathBuf;

use bch_bindgen::c::BCH_SUBVOL_SNAPSHOT_RO;
use clap::{Parser, Subcommand};

use crate::wrappers::handle::BcachefsHandle;

#[derive(Parser, Debug)]
pub struct Cli {
    #[command(subcommand)]
    subcommands: Subcommands,
}

/// Subvolumes-related commands
#[derive(Subcommand, Debug)]
enum Subcommands {
    #[command(visible_aliases = ["new"])]
    Create {
        /// Paths
        targets: Vec<PathBuf>,
    },

    #[command(visible_aliases = ["del"])]
    Delete {
        /// Path
        target: PathBuf,
    },

    #[command(allow_missing_positional = true, visible_aliases = ["snap"])]
    Snapshot {
        /// Make snapshot read only
        #[arg(long, short)]
        read_only: bool,
        source:    Option<PathBuf>,
        dest:      PathBuf,
    },
}

pub fn subvolume(argv: Vec<String>) -> i32 {
    let args = Cli::parse_from(argv);

    match args.subcommands {
        Subcommands::Create { targets } => {
            for target in targets {
                if let Some(dirname) = target.parent() {
                    let fs = unsafe { BcachefsHandle::open(dirname) };
                    fs.create_subvolume(target)
                        .expect("Failed to create the subvolume");
                }
            }
        }
        Subcommands::Delete { target } => {
            if let Some(dirname) = target.parent() {
                let fs = unsafe { BcachefsHandle::open(dirname) };
                fs.delete_subvolume(target)
                    .expect("Failed to delete the subvolume");
            }
        }
        Subcommands::Snapshot {
            read_only,
            source,
            dest,
        } => {
            if let Some(dirname) = dest.parent() {
                let dot = PathBuf::from(".");
                let dir = if dirname.as_os_str().is_empty() {
                    &dot
                } else {
                    dirname
                };
                let fs = unsafe { BcachefsHandle::open(dir) };

                fs.snapshot_subvolume(
                    if read_only {
                        BCH_SUBVOL_SNAPSHOT_RO
                    } else {
                        0x0
                    },
                    source,
                    dest,
                )
                .expect("Failed to snapshot the subvolume");
            }
        }
    }

    0
}
