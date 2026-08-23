//! Where our own messages go, as against the filesystem's.
//!
//! Two audiences, and the split is by level rather than by subsystem. Warnings
//! and errors are on by default, which means they are what a person sees when
//! a boot goes wrong: they read like a mount helper talking, `mount.bcachefs:
//! warning: ...`, because that is what util-linux and everything else in the
//! boot does, and because "which of our source files said it" is no use to
//! someone whose disk is missing.
//!
//! Info and below are only reachable with -v, which is a developer or someone
//! being talked through a problem, and there file:line is the whole point.

use std::io::Write;

use env_logger::WriteStyle;
use log::{Level, LevelFilter};
use owo_colors::{OwoColorize, Style};

/// How we were invoked, which at boot is `mount.bcachefs` rather than
/// `bcachefs` - the more useful of the two to see in a journal.
fn prog() -> String {
    std::env::args()
        .next()
        .and_then(|a| {
            std::path::Path::new(&a)
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "bcachefs".to_string())
}

pub fn setup(verbose: u8, color: bool) {
    let level_filter = match verbose {
        0 => LevelFilter::Warn,
        1 => LevelFilter::Info,
        2 => LevelFilter::Debug,
        _ => LevelFilter::Trace,
    };

    let style = if color {
        WriteStyle::Always
    } else {
        WriteStyle::Never
    };

    let prog = prog();

    env_logger::Builder::new()
        .filter_level(level_filter)
        .write_style(style)
        .parse_env("BCACHEFS_LOG")
        .format(move |buf, record| {
            let style = if style == WriteStyle::Never {
                Style::new()
            } else {
                match record.level() {
                    Level::Trace => Style::new().cyan(),
                    Level::Debug => Style::new().blue(),
                    Level::Info => Style::new().green(),
                    Level::Warn => Style::new().yellow(),
                    Level::Error => Style::new().red().bold(),
                }
            };

            match record.level() {
                Level::Error => writeln!(buf, "{}: {}", prog, record.args()),
                Level::Warn  => writeln!(buf, "{}: {}: {}",
                                         prog, "warning".style(style), record.args()),
                _ => writeln!(
                    buf,
                    "[{:<5} {}:{}] {}",
                    record.level().style(style),
                    record.file().unwrap_or("<unknown>"),
                    record.line().unwrap_or(0),
                    record.args()
                ),
            }
        })
        .init();
}
