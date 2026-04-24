use crate::c;
use crate::fs::Fs;
use std::ffi::{CStr, CString, c_char};

/// Return the opt table as a proper slice.
///
/// bindgen generates `bch2_opt_table` as a zero-length array since it can't
/// determine the size. This wraps it safely using `bch2_opts_nr`.
pub fn opt_table() -> &'static [c::bch_option] {
    unsafe {
        std::slice::from_raw_parts(
            c::bch2_opt_table.as_ptr(),
            c::bch_opt_id::bch2_opts_nr as usize,
        )
    }
}

#[macro_export]
macro_rules! opt_set {
    ($opts:ident, $n:ident, $v:expr) => {
        bch_bindgen::paste! {
            $opts.$n = $v;
            $opts.[<set_ $n _defined>](1)
        }
    };
}

#[macro_export]
macro_rules! opt_defined {
    ($opts:ident, $n:ident) => {
        bch_bindgen::paste! {
            $opts.[< $n _defined>]()
        }
    };
}

#[macro_export]
macro_rules! opt_get {
    ($opts:ident, $n:ident) => {
        if bch_bindgen::opt_defined!($opts, $n) == 0 {
            bch_bindgen::paste! {
                unsafe {
                    bch_bindgen::bcachefs::bch2_opts_default.$n
                }
            }
        } else {
            bch_bindgen::paste! {
                $opts.$n
            }
        }
    };
}

/// Safe conversion from opt table index to bch_opt_id.
///
/// Panics if idx >= bch2_opts_nr (i.e. out of the opt_table range).
pub fn opt_id(idx: usize) -> c::bch_opt_id {
    assert!(idx < c::bch_opt_id::bch2_opts_nr as usize);
    // SAFETY: idx is in 0..bch2_opts_nr, the valid range for bch_opt_id
    unsafe { std::mem::transmute(idx as u32) }
}

/// Check whether an option is explicitly defined in the opts struct.
pub fn opt_defined_by_id(opts: &c::bch_opts, id: c::bch_opt_id) -> bool {
    unsafe { c::bch2_opt_defined_by_id(opts, id) }
}

/// Get the value of an option by id.
pub fn opt_get_by_id(opts: &c::bch_opts, id: c::bch_opt_id) -> u64 {
    unsafe { c::bch2_opt_get_by_id(opts, id) }
}

/// Reference to the default opts (C global).
pub fn opts_default() -> &'static c::bch_opts {
    unsafe { &c::bch2_opts_default }
}

/// Set a superblock option from the opt table.
pub fn opt_set_sb(sb: &mut c::bch_sb, dev_idx: i32, opt: &c::bch_option, v: u64) {
    unsafe { c::__bch2_opt_set_sb(sb, dev_idx, opt, v); }
}

/// Set an option value in a bch_opts struct by id.
pub fn opt_set_by_id(opts: &mut c::bch_opts, id: c::bch_opt_id, v: u64) {
    unsafe { c::bch2_opt_set_by_id(opts, id, v) }
}

/// Safe accessors for bch_option C string fields.
///
/// These methods provide safe access to the static C strings in the
/// option table. The strings are guaranteed to be valid for the process lifetime.
impl c::bch_option {
    /// Get the option name as a Rust string.
    ///
    /// Returns None if the name pointer is null or contains invalid UTF-8.
    pub fn name(&self) -> Option<&'static str> {
        if self.attr.name.is_null() {
            return None;
        }
        unsafe { CStr::from_ptr(self.attr.name) }
            .to_str()
            .ok()
    }

    /// Get the option hint as a Rust string.
    ///
    /// Returns None if the hint pointer is null or contains invalid UTF-8.
    pub fn hint(&self) -> Option<&'static str> {
        if self.hint.is_null() {
            return None;
        }
        unsafe { CStr::from_ptr(self.hint) }
            .to_str()
            .ok()
    }

    /// Get the option help text as a Rust string.
    ///
    /// Returns None if the help pointer is null or contains invalid UTF-8.
    pub fn help(&self) -> Option<&'static str> {
        if self.help.is_null() {
            return None;
        }
        unsafe { CStr::from_ptr(self.help) }
            .to_str()
            .ok()
    }

    /// Collect choices array into a Vec of Rust strings.
    ///
    /// The choices array is null-terminated. Returns an empty Vec if
    /// the pointer is null. Invalid UTF-8 entries are skipped.
    pub fn choices(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.choices.is_null() {
            return v;
        }

        let mut i = 0;
        loop {
            let p = unsafe { *self.choices.add(i) };
            if p.is_null() {
                break;
            }
            if let Ok(s) = unsafe { CStr::from_ptr(p) }.to_str() {
                v.push(s);
            }
            i += 1;
        }
        v
    }
}

pub fn parse_mount_opts(fs: Option<&mut Fs>, optstr: Option<&str>, ignore_unknown: bool)
        -> Result<c::bch_opts, crate::errcode::BchError> {
    let mut opts: c::bch_opts = Default::default();

    if let Some(optstr) = optstr {
        let optstr = CString::new(optstr).unwrap();
        let optstr_ptr = optstr.as_ptr();

        let ret = unsafe {
            c::bch2_parse_mount_opts(fs.map_or(std::ptr::null_mut(), |f| f.raw),
                                     &mut opts as *mut c::bch_opts,
                                     std::ptr::null_mut(),
                                     optstr_ptr as *mut c_char,
                                     ignore_unknown)
        };

        drop(optstr);

        if ret != 0 {
            return Err(crate::errcode::BchError::from_raw(-ret));
        }
    }
    Ok(opts)
}

/// Join a slice of option strings with commas and parse into bch_opts.
///
/// Convenience wrapper for callers that accumulate options as a Vec<String>.
/// Caller passes an empty slice for default opts.
pub fn parse_mount_opts_vec(opts: &[String], ignore_unknown: bool)
        -> Result<c::bch_opts, crate::errcode::BchError> {
    if opts.is_empty() {
        return parse_mount_opts(None, None, ignore_unknown);
    }
    let joined = opts.join(",");
    parse_mount_opts(None, Some(&joined), ignore_unknown)
}
