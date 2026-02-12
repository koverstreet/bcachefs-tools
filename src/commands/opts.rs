use std::ffi::{CStr, CString};
use std::fmt::Write;

use anyhow::{bail, Result};
use bch_bindgen::c;
use bch_bindgen::printbuf::Printbuf;
use clap::{Arg, ArgAction, ArgMatches};

/// Leak a String to get a &'static str. Used for Clap args built from
/// runtime C strings — allocated once at startup, lives for the process.
fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// Read a C string pointer, returning None if null or invalid UTF-8.
unsafe fn c_str(p: *const std::os::raw::c_char) -> Option<&'static str> {
    if p.is_null() { return None }
    CStr::from_ptr(p).to_str().ok()
}

/// Iterate bch2_opt_table entries matching flag_filter, calling f for each.
fn for_each_opt(flag_filter: u32, mut f: impl FnMut(&'static str, &c::bch_option)) {
    unsafe {
        for i in 0..c::bch_opt_id::bch2_opts_nr as usize {
            let opt = &*c::bch2_opt_table.as_ptr().add(i);
            if opt.flags as u32 & flag_filter == 0 { continue }
            if opt.flags as u32 & c::opt_flags::OPT_HIDDEN as u32 != 0 { continue }
            let Some(name) = c_str(opt.attr.name) else { continue };
            f(name, opt);
        }
    }
}

/// Collect null-terminated C string array into Vec.
unsafe fn collect_choices(choices: *const *const std::os::raw::c_char) -> Vec<&'static str> {
    let mut v = Vec::new();
    if choices.is_null() { return v }
    let mut i = 0;
    loop {
        let p = *choices.add(i);
        if p.is_null() { break }
        if let Some(s) = c_str(p) {
            v.push(s);
        }
        i += 1;
    }
    v
}

/// Format usage text for bcachefs options matching the given flags.
///
/// `flags_all` bits must all be set, `flags_none` bits must not be set.
/// Returns a formatted multi-line string with option names, types, and help text.
pub fn opts_usage_str(flags_all: u32, flags_none: u32) -> String {
    const HELPCOL: usize = 32;
    let mut out = String::new();

    unsafe {
        for i in 0..c::bch_opt_id::bch2_opts_nr as usize {
            let opt = &*c::bch2_opt_table.as_ptr().add(i);
            if opt.flags as u32 & flags_all != flags_all { continue }
            if opt.flags as u32 & flags_none != 0 { continue }
            let Some(name) = c_str(opt.attr.name) else { continue };

            let mut col = 0;
            let s = format!("      --{name}");
            col += s.len();
            out.push_str(&s);

            match opt.type_ {
                c::opt_type::BCH_OPT_BOOL => {}
                c::opt_type::BCH_OPT_STR => {
                    out.push_str("=(");
                    col += 2;
                    let choices = collect_choices(opt.choices);
                    for (j, ch) in choices.iter().enumerate() {
                        if j > 0 { out.push('|'); col += 1; }
                        out.push_str(ch);
                        col += ch.len();
                    }
                    out.push(')');
                    col += 1;
                }
                _ => {
                    if let Some(h) = c_str(opt.hint) {
                        let _ = write!(out, "={h}");
                        col += 1 + h.len();
                    }
                }
            }

            if let Some(help) = c_str(opt.help) {
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
    }

    out
}

/// Build Clap arguments from bch2_opt_table entries matching flag_filter.
pub fn bch_option_args(flag_filter: u32) -> Vec<Arg> {
    let mut args = Vec::new();

    for_each_opt(flag_filter, |name, opt| {
        let mut arg = Arg::new(name).long(name);

        if name.contains('_') {
            arg = arg.visible_alias(leak(name.replace('_', "-")));
        }

        unsafe {
            if let Some(h) = c_str(opt.help) {
                arg = arg.help(h);
            }
        }

        match opt.type_ {
            c::opt_type::BCH_OPT_BOOL => {
                arg = arg.num_args(0..=1)
                         .default_missing_value("1")
                         .require_equals(true)
                         .action(ArgAction::Set);
            }
            c::opt_type::BCH_OPT_STR => {
                let choices = unsafe { collect_choices(opt.choices) };
                if !choices.is_empty() {
                    arg = arg.value_parser(choices);
                }
            }
            _ => {
                unsafe {
                    if let Some(h) = c_str(opt.hint) {
                        arg = arg.value_name(h);
                    }
                }
            }
        }

        args.push(arg);
    });

    args
}

/// Look up a bcachefs option by name. Returns the typed option id and reference.
pub fn bch_opt_lookup(name: &str) -> Option<(c::bch_opt_id, &'static c::bch_option)> {
    let c_name = std::ffi::CString::new(name).ok()?;
    let id = unsafe { c::bch2_opt_lookup(c_name.as_ptr()) };
    if id < 0 || id as u32 >= c::bch_opt_id::bch2_opts_nr as u32 {
        return None;
    }
    // Safety: validated in range [0, bch2_opts_nr)
    let opt_id: c::bch_opt_id = unsafe { std::mem::transmute(id as u32) };
    let opt = unsafe { &*c::bch2_opt_table.as_ptr().add(id as usize) };
    Some((opt_id, opt))
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
    for name in bch_option_names(flag_filter) {
        if let Some(val) = matches.get_one::<String>(name) {
            opts.push((name.to_string(), val.clone()));
        }
    }
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
    let c_val = CString::new(val_str)?;
    let mut v: u64 = 0;
    let mut err = Printbuf::new();
    let ret = unsafe {
        c::bch2_opt_parse(
            std::ptr::null_mut(),
            opt,
            c_val.as_ptr(),
            &mut v,
            err.as_raw(),
        )
    };

    if ret == -(c::bch_errcode::BCH_ERR_option_needs_open_fs as i32) {
        return Ok(None);
    }

    if ret != 0 {
        let msg = err.as_str();
        if msg.is_empty() {
            bail!("invalid option: {}", val_str);
        }
        bail!("invalid option: {}", msg);
    }

    Ok(Some(v))
}
