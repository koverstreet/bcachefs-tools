use std::io::Write;

use env_logger::WriteStyle;
use log::{Level, LevelFilter};
use owo_colors::{OwoColorize, Style};

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

            writeln!(
                buf,
                "[{:<5} {}:{}] {}",
                record.level().style(style),
                record.file().unwrap_or("<unknown>"),
                record.line().unwrap_or(0),
                record.args()
            )
        })
        .init();
}
