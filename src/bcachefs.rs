mod commands;
mod copy_fs;
mod device_multipath;
mod device_scan;
mod key;
mod dump_stack;
mod logging;
mod qcow2;
mod util;
mod wrappers;
mod http;

use std::process::{ExitCode, Termination};

use bch_bindgen::c;

#[derive(Debug)]
pub struct ErrnoError(pub errno::Errno);
impl std::fmt::Display for ErrnoError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> Result<(), std::fmt::Error> {
        self.0.fmt(f)
    }
}

impl std::error::Error for ErrnoError {}

/// Read the running kernel's .config from /boot or /proc/config.gz.
fn read_kernel_config() -> Option<String> {
    // Try /boot/config-$(uname -r) first (most distros)
    let release = std::process::Command::new("uname").arg("-r")
        .output().ok()?;
    let release = std::str::from_utf8(&release.stdout).ok()?.trim();
    let path = format!("/boot/config-{release}");
    if let Ok(config) = std::fs::read_to_string(&path) {
        return Some(config);
    }

    // Fallback: /proc/config.gz (NixOS, CONFIG_IKCONFIG_PROC=y kernels)
    let output = std::process::Command::new("zcat")
        .arg("/proc/config.gz")
        .output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

fn kernel_config_has(config: &str, key: &str) -> bool {
    let needle = format!("{key}=y");
    config.lines().any(|l| l == needle)
}

/// Print warnings about kernel configuration issues that affect bcachefs.
/// Called before every command so users can't miss them.
fn check_kernel_warnings() {
    let Some(config) = read_kernel_config() else { return };

    if !kernel_config_has(&config, "CONFIG_RUST") {
        eprintln!("WARNING: kernel does not have CONFIG_RUST enabled; \
                   this will be required for bcachefs in the near future");
        eprintln!("         please alert your distribution or kernel developers \
                   if your kernel does not support CONFIG_RUST");
    }
}

/// Print main bcachefs usage, with commands grouped by category.
/// Descriptions are pulled from the clap command tree (build_cli).
fn bcachefs_usage() {
    let cmd = commands::build_cli();

    println!("bcachefs - tool for managing bcachefs filesystems");
    println!("usage: bcachefs <command> [<args>]\n");

    for (heading, names) in commands::COMMAND_GROUPS {
        println!("{heading}:");
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

fn escape_latex(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        // --- is an em-dash in prose: leave intact
        if b.get(i) == Some(&b'-') && b.get(i+1) == Some(&b'-') && b.get(i+2) == Some(&b'-') {
            out.push_str("---");
            i += 3;
            continue;
        }
        // -- in text mode becomes an en-dash; break ligature with empty group
        if b.get(i) == Some(&b'-') && b.get(i+1) == Some(&b'-') {
            out.push_str("-{}-");
            i += 2;
            continue;
        }
        let ch = s[i..].chars().next().unwrap();
        match ch {
            '_' => out.push_str("\\_"),
            '#' => out.push_str("\\#"),
            '%' => out.push_str("\\%"),
            '&' => out.push_str("\\&"),
            '$' => out.push_str("\\$"),
            '{' => out.push_str("\\{"),
            '}' => out.push_str("\\}"),
            '~' => out.push_str("\\textasciitilde{}"),
            '^' => out.push_str("\\textasciicircum{}"),
            _ => out.push(ch),
        }
        i += ch.len_utf8();
    }
    out
}

/// Convert long_about text to LaTeX: escape special chars, convert
/// `<<sec:LABEL>>` to `Section~\ref{sec:LABEL}`, and preserve paragraph breaks.
fn about_to_latex(text: &str) -> String {
    use std::fmt::Write;

    let mut result = String::new();
    for para in text.split("\n\n") {
        if !result.is_empty() {
            result.push('\n');
        }
        let escaped = escape_latex(para.trim());
        // Convert <<sec:label>> cross-references
        let mut s = escaped.as_str();
        while let Some(start) = s.find("<<") {
            result.push_str(&s[..start]);
            let rest = &s[start + 2..];
            if let Some(end) = rest.find(">>") {
                let label = &rest[..end];
                write!(result, "Section~\\ref{{{label}}}").unwrap();
                s = &rest[end + 2..];
            } else {
                result.push_str("<<");
                s = rest;
            }
        }
        result.push_str(s);
        result.push('\n');
    }
    result
}

/// Generate doc/generated/cli-reference.tex from the clap command tree.
fn generate_cli_doc() -> ExitCode {
    use std::fmt::Write;
    use std::path::Path;

    let cmd = commands::build_cli();
    let mut out = String::new();

    for (heading, names) in commands::COMMAND_GROUPS {
        writeln!(out, "\\subsection{{{heading}}}").unwrap();

        // Emit group-level long_about as intro text before the command list
        for name in *names {
            let Some(sub) = cmd.find_subcommand(name) else { continue };
            if sub.get_subcommands().count() > 0 {
                if let Some(long_about) = sub.get_long_about() {
                    writeln!(out).unwrap();
                    write!(out, "{}", about_to_latex(&long_about.to_string())).unwrap();
                }
            }
        }

        writeln!(out, "\\begin{{description}}").unwrap();

        for name in *names {
            let Some(sub) = cmd.find_subcommand(name) else { continue };
            let children: Vec<_> = sub.get_subcommands()
                .filter(|c| c.get_name() != "help")
                .collect();

            if !children.is_empty() {
                // Group command (device, fs, image, etc.) — list children
                for child in children {
                    let full = format!("bcachefs {name} {}", child.get_name());
                    write!(out, "\\item[{{\\tt {}}}]", escape_latex(&full)).unwrap();

                    let aliases: Vec<_> = child.get_visible_aliases().collect();
                    if !aliases.is_empty() {
                        let alias_str = aliases.into_iter()
                            .map(|a| format!("{{\\tt {}}}", escape_latex(a)))
                            .collect::<Vec<_>>()
                            .join(", ");
                        write!(out, " (alias: {alias_str})").unwrap();
                    }

                    let about = child.get_about().map(|s| s.to_string());
                    let long_about = child.get_long_about().map(|s| s.to_string());

                    if let Some(ref text) = about {
                        writeln!(out, " {}", escape_latex(text)).unwrap();
                    } else {
                        writeln!(out).unwrap();
                    }
                    if let Some(ref text) = long_about {
                        writeln!(out).unwrap();
                        write!(out, "{}", about_to_latex(text)).unwrap();
                    }
                }
            } else {
                // Leaf command
                let full = format!("bcachefs {name}");
                write!(out, "\\item[{{\\tt {}}}]", escape_latex(&full)).unwrap();

                let aliases: Vec<_> = sub.get_visible_aliases().collect();
                if !aliases.is_empty() {
                    let alias_str = aliases.into_iter()
                        .map(|a| format!("{{\\tt {}}}", escape_latex(a)))
                        .collect::<Vec<_>>()
                        .join(", ");
                    write!(out, " (alias: {alias_str})").unwrap();
                }

                let about = sub.get_about().map(|s| s.to_string());
                let long_about = sub.get_long_about().map(|s| s.to_string());

                if let Some(ref text) = about {
                    writeln!(out, " {}", escape_latex(text)).unwrap();
                } else {
                    writeln!(out).unwrap();
                }
                if let Some(ref text) = long_about {
                    writeln!(out).unwrap();
                    write!(out, "{}", about_to_latex(text)).unwrap();
                }
            }
        }

        writeln!(out, "\\end{{description}}").unwrap();
        writeln!(out).unwrap();
    }

    let dir = Path::new("doc/generated");
    if let Err(e) = std::fs::create_dir_all(dir) {
        eprintln!("error creating {}: {e}", dir.display());
        return ExitCode::FAILURE;
    }

    let path = dir.join("cli-reference.tex");
    if let Err(e) = std::fs::write(&path, &out) {
        eprintln!("error writing {}: {e}", path.display());
        return ExitCode::FAILURE;
    }

    eprintln!("wrote {}", path.display());
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    // glibc and Rust stdlib buffer stdout independently; when piped, both
    // switch to block buffering which can reorder or lose output.
    // Set both to line-buffered.
    unsafe {
        extern "C" { static stdout: *mut libc::FILE; }
        libc::setvbuf(stdout, std::ptr::null_mut(), libc::_IOLBF, 0);
    }

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

    check_kernel_warnings();

    match cmd {
        "--help" | "help" => {
            bcachefs_usage();
            ExitCode::SUCCESS
        }
        "version" => {
            let vh = include_str!("../version.h");
            println!("{}", vh.split('"').nth(1).unwrap_or("unknown"));
            let config = read_kernel_config();
            let rust_status = match config.as_deref().map(|c| kernel_config_has(c, "CONFIG_RUST")) {
                Some(true)  => "CONFIG_RUST=y",
                Some(false) => "CONFIG_RUST is not enabled",
                None        => "unable to read kernel config",
            };
            println!("kernel: {rust_status}");
            ExitCode::SUCCESS
        }
        "_doc_gen" => generate_cli_doc(),
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
        "image" => match args.get(2).map(|s| s.as_str()) {
            Some("create") => commands::cmd_image_create(args[2..].to_vec()).report(),
            Some("update") => commands::cmd_image_update(args[2..].to_vec()).report(),
            _ => { group_usage("image"); ExitCode::from(1) }
        },
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
        "migrate" => commands::cmd_migrate(args[1..].to_vec()).report(),
        "migrate-superblock" => commands::cmd_migrate_superblock(args[1..].to_vec()).report(),
        "kill_btree_node" => commands::cmd_kill_btree_node(args[1..].to_vec()).report(),
        "data-read" => commands::data_read::cmd_data_read(args[1..].to_vec()).report(),
        "unpoison" => commands::unpoison::cmd_unpoison(args[1..].to_vec()).report(),
        "dump" => commands::cmd_dump(args[1..].to_vec()).report(),
        "undump" => commands::cmd_undump(args[1..].to_vec()).report(),
        "recover-super" => commands::cmd_recover_super(args[1..].to_vec()).report(),
        "show-super" => commands::super_cmd::cmd_show_super(args[1..].to_vec()).report(),
        "strip-alloc" => commands::cmd_strip_alloc(args[1..].to_vec()).report(),
        "set-file-option" => commands::cmd_setattr(args[1..].to_vec()).report(),
        "set-fs-option" => commands::cmd_set_option(args[1..].to_vec()).report(),
        "set-passphrase" => commands::cmd_set_passphrase(args[1..].to_vec()).report(),
        "reflink-option-propagate" => commands::cmd_reflink_option_propagate(args[1..].to_vec()).report(),
        "unlock" => commands::cmd_unlock(args[1..].to_vec()).report(),
        "fusemount" => {
            let argv = if symlink_cmd.is_some() { args.clone() } else { args[1..].to_vec() };
            commands::fusemount::cmd_fusemount(argv).report()
        }
        _ => {
            println!("Unknown command {cmd}");
            bcachefs_usage();
            ExitCode::from(1)
        }
    }
}
