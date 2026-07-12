use std::ffi::CString;
use std::fmt::Write;

use anyhow::{bail, Result};
use bch_bindgen::c;
use bcachefs_kernel::util::printbuf::Printbuf;
use clap::{Arg, ArgAction, ArgMatches};

/// Leak a String to get a &'static str. Used for Clap args built from
/// runtime C strings — allocated once at startup, lives for the process.
fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// Iterate bch2_opt_table entries matching flag_filter, calling f for each.
fn for_each_opt(flag_filter: u32, mut f: impl FnMut(&'static str, &c::bch_option)) {
    for opt in bcachefs_kernel::opts::opt_table() {
        if opt.flags as u32 & flag_filter == 0 { continue }
        if opt.flags as u32 & c::opt_flags::OPT_HIDDEN as u32 != 0 { continue }
        let Some(name) = opt.name() else { continue };
        f(name, opt);
    }
}

/// Format usage text for bcachefs options matching the given flags.
///
/// `flags_all` bits must all be set, `flags_none` bits must not be set.
/// Returns a formatted multi-line string with option names, types, and help text.
pub fn opts_usage_str(flags_all: u32, flags_none: u32) -> String {
    const HELPCOL: usize = 32;
    let mut out = String::new();

    for opt in bcachefs_kernel::opts::opt_table() {
        if opt.flags as u32 & flags_all != flags_all { continue }
        if opt.flags as u32 & flags_none != 0 { continue }
        let Some(name) = opt.name() else { continue };
        if name == "fs_label" && flags_all & c::opt_flags::OPT_FORMAT as u32 != 0 { continue }

        let mut col = 0;
        let s = format!("      --{name}");
        col += s.len();
        out.push_str(&s);

        match opt.type_ {
            c::opt_type::BCH_OPT_BOOL => {}
            c::opt_type::BCH_OPT_STR => {
                out.push_str("=(");
                col += 2;
                let choices = opt.choices();
                for (j, ch) in choices.iter().enumerate() {
                    if j > 0 { out.push('|'); col += 1; }
                    out.push_str(ch);
                    col += ch.len();
                }
                out.push(')');
                col += 1;
            }
            _ => {
                if let Some(h) = opt.hint() {
                    let _ = write!(out, "={h}");
                    col += 1 + h.len();
                }
            }
        }

        if let Some(help) = opt.help() {
            for (j, line) in help.split('\n').enumerate() {
                if line.is_empty() && j > 0 { break; }
                if j > 0 || col > HELPCOL {
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

/// Build Clap arguments from bch2_opt_table entries matching flag_filter.
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

        if let Some(h) = opt.help() {
            arg = arg.help(h);
        }

        match opt.type_ {
            c::opt_type::BCH_OPT_BOOL => {
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
            c::opt_type::BCH_OPT_STR => {
                let mut choices = opt.choices();
                if !choices.is_empty() {
                    if allow_remove {
                        choices.push("-");
                    }
                    arg = arg.value_parser(choices);
                }
            }
            c::opt_type::BCH_OPT_BITFIELD => {
                if let Some(h) = opt.hint() {
                    arg = arg.value_name(h);
                }
                arg = arg.allow_hyphen_values(true);
            }
            _ => {
                if let Some(h) = opt.hint() {
                    arg = arg.value_name(h);
                }
            }
        }

        if name == "fs_label" {
            let max_label_bytes = c::BCH_SB_LABEL_SIZE as usize - 1;
            arg = arg.value_parser(move |value: &str| {
                if value.len() > max_label_bytes {
                    Err(format!("fs_label: too long (max {max_label_bytes} bytes)"))
                } else {
                    Ok(value.to_owned())
                }
            });
        }

        args.push(arg);
    });

    args
}

/// Look up a bcachefs option by name, handling --nooption negation for booleans.
/// Returns (opt_id, opt_ref, negated).
pub fn bch_opt_lookup_negated(name: &str) -> Option<(c::bch_opt_id, &'static c::bch_option, bool)> {
    if let Some(r) = bch_opt_lookup(name) {
        return Some((r.0, r.1, false));
    }
    let rest = name.strip_prefix("no_").or_else(|| name.strip_prefix("no"))?;
    let (id, opt) = bch_opt_lookup(rest)?;
    (opt.type_ == c::opt_type::BCH_OPT_BOOL).then_some((id, opt, true))
}

/// Look up a bcachefs option by name. Returns the typed option id and reference.
pub fn bch_opt_lookup(name: &str) -> Option<(c::bch_opt_id, &'static c::bch_option)> {
    let c_name = std::ffi::CString::new(name).ok()?;
    bcachefs_kernel::opts::opt_lookup(&c_name)
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
        } else if opt.type_ == c::opt_type::BCH_OPT_BOOL {
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
///   `Ok(Some(v))` — parsed value, ready to set with `bch2_opt_set_by_id`
///   `Err(...)` — parse error
pub(crate) fn parse_opt_val(
    opt: &c::bch_option,
    val_str: &str,
) -> Result<Option<u64>> {
    let fs_label = opt.name() == Some("fs_label");
    let c_val = CString::new(val_str)?;
    let mut err = Printbuf::new();
    let parsed = match bcachefs_kernel::opts::opt_parse(None, opt, &c_val, Some(&mut err)) {
        Ok(v) => v,
        Err(e) if e == -(c::bch_errcode::BCH_ERR_option_needs_open_fs as i32) => return Ok(None),
        Err(_) => {
            let msg = err.as_str();
            if msg.is_empty() {
                bail!("invalid option: {}", val_str);
            }
            bail!("invalid option: {}", msg);
        }
    };

    if fs_label {
        // The parsed value is a pointer into c_val, which is only needed for
        // validation here. Keep the owned string in bch_opt_strs instead.
        return Ok(None);
    }

    Ok(Some(parsed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fs_label_parse_does_not_escape_into_scalar_opts() {
        let (id, opt) = bch_opt_lookup("fs_label").unwrap();
        let label = b"owned-superblock-label";
        let label_len = label.len();
        let source = unsafe { libc::malloc(label_len + 1) as *mut libc::c_char };
        assert!(!source.is_null());
        unsafe {
            std::ptr::copy_nonoverlapping(label.as_ptr(), source as *mut u8, label_len);
            *source.add(label_len) = 0;
        }
        let mut parsed = 0;

        assert_eq!(
            unsafe {
                c::bch2_opt_parse(
                    std::ptr::null_mut(),
                    opt,
                    source,
                    &mut parsed,
                    std::ptr::null_mut(),
                )
            },
            0
        );

        let mut scalar_opts = c::bch_opts::default();
        unsafe { c::bch2_opt_set_by_id(&mut scalar_opts, id, parsed) };
        assert_eq!(unsafe { c::bch2_opt_get_by_id(&scalar_opts, id) }, 0);
        let scalar_opts_copy = scalar_opts;
        assert_eq!(unsafe { c::bch2_opt_get_by_id(&scalar_opts_copy, id) }, 0);

        let mut sb = c::bch_sb_handle::default();
        assert_eq!(unsafe { c::bch2_sb_realloc(&mut sb, 0) }, 0);
        assert!(unsafe { c::__bch2_opt_set_sb(sb.sb, -1, opt, parsed) });

        unsafe { libc::free(source as *mut libc::c_void) };
        let poison = unsafe { libc::malloc(label_len + 1) as *mut libc::c_char };
        assert_eq!(poison, source);
        unsafe {
            std::ptr::write_bytes(poison as *mut u8, b'X', label_len);
            *poison.add(label_len) = 0;
        }
        assert_eq!(unsafe { c::bch2_sb_realloc(&mut sb, 1024) }, 0);

        let mut from_sb = c::bch_opts::default();
        assert_eq!(unsafe { c::bch2_opts_from_sb(&mut from_sb, sb.sb) }, 0);
        assert_eq!(unsafe { c::bch2_opt_get_by_id(&from_sb, id) }, 0);

        let mut no_sb = Printbuf::new();
        unsafe {
            c::bch2_opt_to_text(
                no_sb.as_raw(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                opt,
                parsed,
                0,
            )
        };
        assert_eq!(no_sb.as_str(), "");

        let mut rendered = Printbuf::new();
        unsafe {
            c::bch2_opt_to_text(
                rendered.as_raw(),
                std::ptr::null_mut(),
                sb.sb,
                opt,
                parsed,
                0,
            )
        };
        assert_eq!(rendered.as_str(), "owned-superblock-label");
        assert_eq!(unsafe { *poison }, b'X' as libc::c_char);

        unsafe {
            libc::free(poison as *mut libc::c_void);
            c::bch2_free_super(&mut sb);
        }
    }
}
