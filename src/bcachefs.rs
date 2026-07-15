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

use std::process::ExitCode;
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
fn bcachefs_usage() {
    use commands::{CmdKind, COMMAND_GROUPS};

    println!("bcachefs - tool for managing bcachefs filesystems");
    println!("usage: bcachefs <command> [<args>]\n");

    for group in COMMAND_GROUPS {
        println!("{}:", group.heading);
        for cmd in group.commands {
            match &cmd.kind {
                CmdKind::Group { children } => {
                    for child in *children {
                        let full = format!("{} {}", cmd.name, child.name);
                        println!("  {full:<26}{}", child.about);
                    }
                }
                _ => println!("  {:<26}{}", cmd.name, cmd.about),
            }
        }
        println!();
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

    for group in commands::COMMAND_GROUPS {
        writeln!(out, "\\subsection{{{}}}", group.heading).unwrap();

        // Emit group-level long_about as intro text
        for def in group.commands {
            if let Some(sub) = cmd.find_subcommand(def.name) {
                if sub.get_subcommands().count() > 0 {
                    if let Some(long_about) = sub.get_long_about() {
                        writeln!(out).unwrap();
                        write!(out, "{}", about_to_latex(&long_about.to_string())).unwrap();
                    }
                }
            }
        }

        writeln!(out, "\\begin{{description}}").unwrap();

        for def in group.commands {
            let Some(sub) = cmd.find_subcommand(def.name) else { continue };
            let children: Vec<_> = sub.get_subcommands()
                .filter(|c| c.get_name() != "help")
                .collect();

            if !children.is_empty() {
                for child in children {
                    let full = format!("bcachefs {} {}", def.name, child.get_name());
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
                let full = format!("bcachefs {}", def.name);
                write!(out, "\\item[{{\\tt {}}}]", escape_latex(&full)).unwrap();

                if !def.aliases.is_empty() {
                    let alias_str = def.aliases.iter()
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
    // panic = "abort" in Cargo.toml ensures Rust panics can't be swallowed
    // by catch_unwind; this installs C-side handlers for SIGABRT/SIGSEGV/
    // SIGILL/SIGBUS/SIGFPE so a panic or fatal fault prints a unified
    // backtrace via bch2_prt_task_backtrace before the kernel core-dumps.
    unsafe { c::bch2_install_fatal_signal_handlers(); }

    // glibc and Rust stdlib buffer stdout independently; when piped, both
    // switch to block buffering which can reorder or lose output.
    // Set both to line-buffered.
    unsafe {
        extern "C" { static stdout: *mut libc::FILE; }
        libc::setvbuf(stdout, std::ptr::null_mut(), libc::_IOLBF, 0);
    }

    let args: Vec<String> = std::env::args().collect();

    // Handle symlink invocations (mkfs.bcachefs, fsck.bcachefs, mount.bcachefs, etc.)
    let symlink_cmd: Option<&str> = if args[0].contains("mkfs") {
        Some("format")
    } else if args[0].contains("fsck") {
        Some("fsck")
    } else if args[0].contains("mount.fuse") {
        Some("fusemount")
    } else if args[0].contains("mount") {
        Some("mount")
    } else {
        None
    };

    // Handle top-level help and missing command
    if symlink_cmd.is_none() {
        match args.get(1).map(|s| s.as_str()) {
            None => {
                println!("missing command");
                bcachefs_usage();
                return ExitCode::from(1);
            }
            Some("--help" | "-h" | "help") => {
                bcachefs_usage();
                return ExitCode::SUCCESS;
            }
            Some("_doc_gen") => return generate_cli_doc(),
            _ => {}
        }
    }

    unsafe { c::raid_init() };

    let (cmd_name, argv) = if let Some(cmd) = symlink_cmd {
        let mut v = vec![cmd.to_string()];
        v.extend_from_slice(&args[1..]);
        (cmd, v)
    } else {
        (args[1].as_str(), args[1..].to_vec())
    };

    // fuse will call this after daemonizing, we can't create threads before
    // note that mount may invoke fusemount, via -t bcachefs.fuse
    if !commands::defers_shrinkers(cmd_name) {
        unsafe { c::linux_shrinkers_init() };
    }

    check_kernel_warnings();

    match commands::dispatch(cmd_name, argv) {
        Some(code) => code,
        None => {
            println!("Unknown command {cmd_name}");
            bcachefs_usage();
            ExitCode::from(1)
        }
    }
}
