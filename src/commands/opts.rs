use std::ffi::CStr;

use bch_bindgen::c;
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
