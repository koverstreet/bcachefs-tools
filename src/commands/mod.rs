// commands/mod.rs — command table, dispatch, and help
//
// Single source of truth: COMMAND_GROUPS defines every command.
// Each command module exports its CmdDef(s). This file groups them.

use std::process::ExitCode;

// ── Command table types (must precede mod declarations for macro access) ──

pub struct CmdDef {
    pub name:    &'static str,
    pub about:   &'static str,
    pub aliases: &'static [&'static str],
    pub kind:    CmdKind,
}

pub enum CmdKind {
    Typed {
        cmd: fn() -> clap::Command,
        run: fn(Vec<String>) -> ExitCode,
    },
    Raw {
        run: fn(Vec<String>) -> ExitCode,
    },
    Group {
        children: &'static [&'static CmdDef],
    },
}

pub struct GroupDef {
    pub heading:  &'static str,
    pub commands: &'static [&'static CmdDef],
}

/// Define a typed command (clap-parsed args).
#[macro_export]
macro_rules! typed_cmd {
    ($name:literal, $about:literal, $cli:ty, $handler:expr) => {
        typed_cmd!($name, $about, aliases: [], $cli, $handler)
    };
    ($name:literal, $about:literal, aliases: [$($alias:literal),*], $cli:ty, $handler:expr) => {{
        fn __cmd() -> clap::Command { <$cli as clap::CommandFactory>::command() }
        fn __run(argv: Vec<String>) -> std::process::ExitCode {
            use std::process::Termination;
            $handler(<$cli as clap::Parser>::parse_from(argv)).report()
        }
        $crate::commands::CmdDef {
            name: $name, about: $about, aliases: &[$($alias),*],
            kind: $crate::commands::CmdKind::Typed { cmd: __cmd, run: __run },
        }
    }};
}

/// Define a raw command (manual arg parsing).
#[macro_export]
macro_rules! raw_cmd {
    ($name:literal, $about:literal, $handler:expr) => {
        raw_cmd!($name, $about, aliases: [], $handler)
    };
    ($name:literal, $about:literal, aliases: [$($alias:literal),*], $handler:expr) => {{
        fn __run(argv: Vec<String>) -> std::process::ExitCode {
            use std::process::Termination;
            $handler(argv).report()
        }
        $crate::commands::CmdDef {
            name: $name, about: $about, aliases: &[$($alias),*],
            kind: $crate::commands::CmdKind::Raw { run: __run },
        }
    }};
}

// ── Subcommand modules ───────────────────────────────────────────────

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

// ── Dispatch and help ────────────────────────────────────────────────

impl CmdDef {
    pub fn dispatch(&self, argv: Vec<String>) -> ExitCode {
        match &self.kind {
            CmdKind::Typed { run, .. } | CmdKind::Raw { run } => run(argv),
            CmdKind::Group { children } => {
                // argv[0] is the group name, argv[1] is the subcommand
                let subcmd = argv.get(1).map(|s| s.as_str());
                for child in *children {
                    if subcmd == Some(child.name) ||
                       child.aliases.iter().any(|a| subcmd == Some(*a)) {
                        return child.dispatch(argv[1..].to_vec());
                    }
                }
                println!("bcachefs {} - {}", self.name, self.about);
                println!("Usage: bcachefs {} <COMMAND>\n", self.name);
                println!("Commands:");
                for child in *children {
                    println!("  {:<26}{}", child.name, child.about);
                }
                if matches!(subcmd, Some("--help" | "-h" | "help") | None) {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::from(1)
                }
            }
        }
    }

    pub fn clap_command(&self) -> clap::Command {
        match &self.kind {
            CmdKind::Typed { cmd, .. } => cmd().name(self.name).about(self.about),
            CmdKind::Raw { .. } => clap::Command::new(self.name).about(self.about),
            CmdKind::Group { children } => {
                let mut cmd = clap::Command::new(self.name).about(self.about);
                for child in *children {
                    cmd = cmd.subcommand(child.clap_command());
                }
                cmd
            }
        }
    }

    fn matches(&self, name: &str) -> bool {
        self.name == name || self.aliases.iter().any(|a| *a == name)
    }
}

pub fn dispatch(name: &str, argv: Vec<String>) -> Option<ExitCode> {
    for group in COMMAND_GROUPS {
        for cmd in group.commands {
            if cmd.matches(name) {
                return Some(cmd.dispatch(argv));
            }
        }
    }
    None
}

pub fn build_cli() -> clap::Command {
    let mut cmd = clap::Command::new("bcachefs");
    for group in COMMAND_GROUPS {
        for def in group.commands {
            cmd = cmd.subcommand(def.clap_command());
        }
    }
    cmd
}

pub fn defers_shrinkers(name: &str) -> bool {
    name == "mount" || name == "fusemount"
}

// ── Cross-module groups (assembled here) ─────────────────────────────

static FS_CMD: CmdDef = CmdDef {
    name: "fs", about: "Manage a running filesystem", aliases: &[],
    kind: CmdKind::Group { children: &[&fs_usage::CMD, &top::CMD, &timestats::CMD] },
};

// ── Version (no module, trivial) ─────────────────────────────────────

static VERSION_CMD: CmdDef = {
    fn __run(_argv: Vec<String>) -> ExitCode {
        let vh = include_str!("../../version.h");
        println!("{}", vh.split('"').nth(1).unwrap_or("unknown"));
        ExitCode::SUCCESS
    }
    CmdDef { name: "version", about: "Display version", aliases: &[],
             kind: CmdKind::Raw { run: __run } }
};

// ── Command groups ───────────────────────────────────────────────────

pub const COMMAND_GROUPS: &[GroupDef] = &[
    GroupDef { heading: "Superblock commands", commands: &[
        &format::CMD, &super_cmd::CMD, &recover_super::CMD,
        &set_option::CMD, &counters::CMD, &strip_alloc::CMD,
    ]},
    GroupDef { heading: "Images",                   commands: &[&image::CMD] },
    GroupDef { heading: "Mount",                    commands: &[&mount::CMD, &fusemount::CMD] },
    GroupDef { heading: "Repair",                   commands: &[&fsck::CMD, &recovery_pass::CMD] },
    GroupDef { heading: "Running filesystem",       commands: &[&FS_CMD] },
    GroupDef { heading: "Devices",                  commands: &[&device::CMD] },
    GroupDef { heading: "Subvolumes and snapshots", commands: &[&subvolume::CMD] },
    GroupDef { heading: "Filesystem data",          commands: &[&reconcile::CMD, &scrub::CMD] },
    GroupDef { heading: "Encryption",               commands: &[&key::CMD_UNLOCK, &key::CMD_SET_PASSPHRASE, &key::CMD_REMOVE_PASSPHRASE] },
    GroupDef { heading: "Migrate",                  commands: &[&migrate::CMD_MIGRATE, &migrate::CMD_MIGRATE_SUPERBLOCK] },
    GroupDef { heading: "File options",             commands: &[&attr::CMD_SETATTR, &attr::CMD_REFLINK_PROPAGATE] },
    GroupDef { heading: "Debug", commands: &[
        &dump::CMD_DUMP, &dump::CMD_UNDUMP, &list::CMD, &list_journal::CMD,
        &kill_btree_node::CMD, &data_read::CMD, &unpoison::CMD,
    ]},
    GroupDef { heading: "Miscellaneous",            commands: &[&completions::CMD, &VERSION_CMD] },
];

