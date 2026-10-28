use std::fmt::Write;

use anyhow::{bail, Result};
use bch_bindgen::c;
use bcachefs_kernel::errcode::bch_errcode;
use bcachefs_kernel::opts::{Opt, OptType};
use bcachefs_kernel::util::printbuf::Printbuf;
use clap::{Arg, ArgAction, ArgMatches};

/// Leak a String to get a &'static str. Used for Clap args built at
/// runtime — allocated once at startup, lives for the process.
fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// Iterate the options matching flag_filter, calling f for each.
fn for_each_opt(flag_filter: u32, mut f: impl FnMut(&'static str, &'static Opt)) {
    for opt in &bcachefs_kernel::opts::OPTS {
        if opt.flags.bits() & flag_filter == 0 { continue }
        if opt.flags.bits() & c::opt_flags::OPT_HIDDEN.bits() != 0 { continue }
        f(opt.name, opt);
    }
}

/// Format usage text for bcachefs options matching the given flags.
///
/// `flags_all` bits must all be set, `flags_none` bits must not be set.
/// Returns a formatted multi-line string with option names, types, and help text.
pub fn opts_usage_str(flags_all: u32, flags_none: u32) -> String {
    const HELPCOL: usize = 32;
    let mut out = String::new();

    for opt in &bcachefs_kernel::opts::OPTS {
        if opt.flags.bits() & flags_all != flags_all { continue }
        if opt.flags.bits() & flags_none != 0 { continue }
        let name = opt.name;

        let mut col = 0;
        let s = if matches!(opt.ty, OptType::Bool) {
            format!("      --{name}, --no{name}")
        } else {
            format!("      --{name}")
        };
        col += s.len();
        out.push_str(&s);

        match opt.ty {
            OptType::Bool => {}
            OptType::Str(choices) => {
                out.push_str("=(");
                col += 2;
                for (j, ch) in choices.iter().enumerate() {
                    if j > 0 { out.push('|'); col += 1; }
                    out.push_str(ch);
                    col += ch.len();
                }
                out.push(')');
                col += 1;
            }
            _ => {
                if let Some(h) = opt.hint {
                    let _ = write!(out, "={h}");
                    col += 1 + h.len();
                }
            }
        }

        if let Some(help) = opt.help {
            for (j, line) in help.split('\n').enumerate() {
                if line.is_empty() && j > 0 { break; }
                // help text starts at HELPCOL - 1: wrap unless there's room
                // for at least one space before it
                if j > 0 || col >= HELPCOL - 1 {
                    out.push('\n');
                    col = 0;
                }
                while col < HELPCOL - 1 {
                    out.push(' ');
                    col += 1;
                }
                out.push_str(line);
                out.push('\n');
                col = 0;
            }
        } else {
            out.push('\n');
        }
    }

    out
}

/// Build Clap arguments from the options matching flag_filter.
///
/// `allow_remove` adds "-" as an accepted value for choice-typed (BCH_OPT_STR)
/// options — the sentinel set-file-option uses to delete a per-file option.
/// Without it, clap's choice validation rejects "-" before the command sees it.
/// Commands with no removal semantics (set-option, device add) pass false.
pub fn bch_option_args(flag_filter: u32, allow_remove: bool) -> Vec<Arg> {
    let mut args = Vec::new();

    for_each_opt(flag_filter, |name, opt| {
        let mut arg = Arg::new(name).long(name);

        if name.contains('_') {
            arg = arg.visible_alias(leak(name.replace('_', "-")));
        }

        if let Some(h) = opt.help {
            arg = arg.help(h);
        }

        match opt.ty {
            OptType::Bool => {
                arg = arg.num_args(0..=1)
                         .default_missing_value("1")
                         .require_equals(true)
                         .action(ArgAction::Set);

                let no_name = leak(format!("no{name}"));
                let mut no_arg = Arg::new(no_name)
                    .long(no_name)
                    .num_args(0)
                    .action(ArgAction::SetTrue)
                    .hide(true);

                if name.contains('_') {
                    no_arg = no_arg.alias(leak(format!("no{}", name.replace('_', "-"))))
                                   .alias(leak(format!("no-{}", name.replace('_', "-"))));
                } else {
                    no_arg = no_arg.alias(leak(format!("no-{name}")));
                }

                args.push(no_arg);
            }
            OptType::Str(choices) => {
                let mut choices = choices.to_vec();
                if !choices.is_empty() {
                    if allow_remove {
                        choices.push("-");
                    }
                    arg = arg.value_parser(choices);
                }
            }
            OptType::Bitfield { .. } => {
                if let Some(h) = opt.hint {
                    arg = arg.value_name(h);
                }
                arg = arg.allow_hyphen_values(true);
            }
            _ => {
                if let Some(h) = opt.hint {
                    arg = arg.value_name(h);
                }
            }
        }

        args.push(arg);
    });

    args
}

/// Look up a bcachefs option by name, handling --nooption negation for booleans.
/// Returns (opt_id, opt_ref, negated).
pub fn bch_opt_lookup_negated(name: &str) -> Option<(c::bch_opt_id, &'static Opt, bool)> {
    if let Some(r) = bch_opt_lookup(name) {
        return Some((r.0, r.1, false));
    }
    let rest = name.strip_prefix("no_").or_else(|| name.strip_prefix("no"))?;
    let (id, opt) = bch_opt_lookup(rest)?;
    matches!(opt.ty, OptType::Bool).then_some((id, opt, true))
}

/// Look up a bcachefs option by name. Returns the typed option id and reference.
pub fn bch_opt_lookup(name: &str) -> Option<(c::bch_opt_id, &'static Opt)> {
    bcachefs_kernel::opts::opt_lookup(name).map(|opt| (opt.id, opt))
}

/// Option names matching the filter.
pub fn bch_option_names(flag_filter: u32) -> Vec<&'static str> {
    let mut names = Vec::new();
    for_each_opt(flag_filter, |name, _| names.push(name));
    names
}

/// Extract bcachefs option (name, value) pairs from ArgMatches.
pub fn bch_options_from_matches(matches: &ArgMatches, flag_filter: u32) -> Vec<(String, String)> {
    let mut opts = Vec::new();
    for_each_opt(flag_filter, |name, opt| {
        if let Some(val) = matches.get_one::<String>(name) {
            opts.push((name.to_string(), val.clone()));
        } else if matches!(opt.ty, OptType::Bool) {
            let no_name = format!("no{name}");
            if matches.get_flag(&no_name) {
                opts.push((name.to_string(), "0".to_string()));
            }
        }
    });
    opts
}

/// Parse a bcachefs option value string using the C option table.
///
/// Returns:
///   `Ok(None)` — option needs an open filesystem, should be deferred
///   `Ok(Some(v))` — parsed value, ready to set with `bch_opts::set()`
///   `Err(...)` — parse error
pub(crate) fn parse_opt_val(
    opt: &Opt,
    val_str: &str,
) -> Result<Option<u64>> {
    let mut err = Printbuf::new();
    match bcachefs_kernel::opts::opt_parse(None, opt, Some(val_str.as_bytes()), Some(&mut err)) {
        Ok(v) => Ok(Some(v)),
        Err(e) if e == bch_errcode::BCH_ERR_option_needs_open_fs.into() => Ok(None),
        Err(_) => {
            let msg = err.as_str();
            if msg.is_empty() {
                bail!("invalid option: {}", val_str);
            }
            bail!("invalid option: {}", msg);
        }
    }
}
