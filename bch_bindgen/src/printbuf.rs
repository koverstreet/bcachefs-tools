use std::ffi::CStr;
use std::fmt;
use std::ops::{Deref, DerefMut};

use crate::c;

/// Rust wrapper around `c::printbuf` providing `fmt::Write` via
/// `bch2_prt_bytes_indented`, which processes `\t`, `\r`, `\n` for
/// tabstop and indent handling.
pub struct Printbuf(c::printbuf);

/// RAII guard for printbuf indentation — calls `indent_sub` on drop.
/// Use through `Printbuf::indent()`.
pub struct PrintbufIndent<'a> {
    buf: &'a mut Printbuf,
    spaces: u32,
}

impl Drop for PrintbufIndent<'_> {
    fn drop(&mut self) {
        self.buf.indent_sub(self.spaces);
    }
}

impl Deref for PrintbufIndent<'_> {
    type Target = Printbuf;
    fn deref(&self) -> &Printbuf { self.buf }
}

impl DerefMut for PrintbufIndent<'_> {
    fn deref_mut(&mut self) -> &mut Printbuf { self.buf }
}

impl Printbuf {
    pub fn new() -> Self {
        Printbuf(c::printbuf::new())
    }

    pub fn as_str(&self) -> &str {
        if self.0.buf.is_null() {
            ""
        } else {
            unsafe { CStr::from_ptr(self.0.buf) }
                .to_str()
                .unwrap_or("")
        }
    }

    /// Add a tabstop at `spaces` columns from the previous tabstop.
    pub fn tabstop_push(&mut self, spaces: u32) {
        unsafe { c::bch2_printbuf_tabstop_push(&mut self.0, spaces) };
    }

    pub fn tabstops_reset(&mut self) {
        unsafe { c::bch2_printbuf_tabstops_reset(&mut self.0) };
    }

    /// Reset tabstops and set new ones from a slice of column widths.
    pub fn tabstops(&mut self, widths: &[u32]) {
        self.tabstops_reset();
        for &w in widths {
            self.tabstop_push(w);
        }
    }

    pub fn indent_add(&mut self, spaces: u32) {
        unsafe { c::bch2_printbuf_indent_add(&mut self.0, spaces) };
    }

    pub fn indent_sub(&mut self, spaces: u32) {
        unsafe { c::bch2_printbuf_indent_sub(&mut self.0, spaces) };
    }

    /// Add indentation, returning a guard that removes it on drop.
    /// Use the guard (which derefs to `&mut Printbuf`) for all
    /// operations within the indented scope.
    pub fn indent(&mut self, spaces: u32) -> PrintbufIndent<'_> {
        self.indent_add(spaces);
        PrintbufIndent { buf: self, spaces }
    }

    /// Advance to next tabstop (equivalent to `\t` in format string).
    pub fn tab(&mut self) {
        unsafe { c::bch2_prt_tab(&mut self.0) };
    }

    /// Right-justify previous text in current tabstop column
    /// (equivalent to `\r` in format string).
    pub fn tab_rjust(&mut self) {
        unsafe { c::bch2_prt_tab_rjust(&mut self.0) };
    }

    /// Emit newline with indent handling
    /// (equivalent to `\n` in format string).
    pub fn newline(&mut self) {
        unsafe { c::bch2_prt_newline(&mut self.0) };
    }

    /// Print a u64 value using `bch2_prt_units_u64`, which respects
    /// the `human_readable_units` flag on the printbuf.
    pub fn units_u64(&mut self, v: u64) {
        unsafe { c::bch2_prt_units_u64(&mut self.0, v) };
    }

    /// Print a sector count as bytes (sectors << 9).
    pub fn units_sectors(&mut self, sectors: u64) {
        self.units_u64(sectors << 9);
    }

    pub fn set_human_readable(&mut self, v: bool) {
        self.0.set_human_readable_units(v);
    }

    /// Print a human-readable representation of a u64 value.
    pub fn human_readable_u64(&mut self, v: u64) {
        unsafe { c::bch2_prt_human_readable_u64(&mut self.0, v) };
    }

    /// Print a bcachefs metadata version number.
    pub fn version(&mut self, v: u32) {
        // bch2_version_to_text takes an enum bcachefs_metadata_version,
        // which is #[repr(u32)]. We transmute from u32.
        unsafe { c::bch2_version_to_text(&mut self.0, std::mem::transmute::<u32, c::bcachefs_metadata_version>(v)) };
    }

    /// Print superblock contents.
    pub fn sb_to_text(&mut self, fs: *mut c::bch_fs, sb: &c::bch_sb,
                      layout: bool, fields: u32) {
        unsafe { c::bch2_sb_to_text(&mut self.0, fs, sb as *const _ as *mut _, layout, fields) };
    }

    /// Print superblock contents with field names.
    pub fn sb_to_text_with_names(&mut self, fs: *mut c::bch_fs, sb: &c::bch_sb,
                                 layout: bool, fields: u32, field_only: i32) {
        unsafe { c::bch2_sb_to_text_with_names(&mut self.0, fs, sb as *const _ as *mut _, layout, fields, field_only) };
    }

    /// Print a set of bitflags as comma-separated names.
    pub fn prt_bitflags(&mut self, list: *const *const std::os::raw::c_char, flags: u64) {
        unsafe { c::bch2_prt_bitflags(&mut self.0, list, flags) };
    }

    /// Access the underlying `c::printbuf` for calling C prt_* functions.
    pub fn as_raw(&mut self) -> &mut c::printbuf {
        &mut self.0
    }
}

impl fmt::Write for Printbuf {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        unsafe {
            c::bch2_prt_bytes_indented(
                &mut self.0,
                s.as_ptr() as *const std::os::raw::c_char,
                s.len() as std::os::raw::c_uint,
            );
        }
        Ok(())
    }
}

impl Default for Printbuf {
    fn default() -> Self { Self::new() }
}

impl fmt::Display for Printbuf {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
