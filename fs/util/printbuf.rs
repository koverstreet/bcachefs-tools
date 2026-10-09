use core::ffi::CStr;
use core::fmt::{self, Write as _};
use core::ops::{Deref, DerefMut};

use crate::c;
use crate::c::bpos as Bpos;
use crate::util::os_str::{OsStr, OsStrExt};

/// Rust wrapper around `c::printbuf` providing `fmt::Write` via
/// `bch2_prt_bytes_indented`, which processes `\t`, `\r`, `\n` for
/// tabstop and indent handling.
#[repr(transparent)]
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

    /// A C caller's printbuf, to write to - for a to_text C calls. Borrowed:
    /// it's the caller's to free.
    ///
    /// # Safety
    /// @raw is a valid printbuf nothing else touches for 'a.
    pub unsafe fn borrow_raw<'a>(raw: *mut c::printbuf) -> &'a mut Printbuf {
        unsafe { &mut *(raw as *mut Printbuf) }
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

    /// What's been written, as bytes - which needn't be UTF-8, after
    /// write_bytes().
    pub fn as_bytes(&self) -> &[u8] {
        if self.0.buf.is_null() {
            &[]
        } else {
            unsafe { core::slice::from_raw_parts(self.0.buf as *const u8, self.0.pos as usize) }
        }
    }

    /// What's been written, as a name: for building one.
    pub fn as_os_str(&self) -> &OsStr {
        OsStr::from_bytes(self.as_bytes())
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

    /// Post-process the buffer, aligning columns separated by raw `\t`
    /// (left-aligned) and `\r` (right-aligned) characters. Call after
    /// writing a section of tabular data without preset tabstops.
    pub fn tabstop_align(&mut self) {
        unsafe { c::bch2_printbuf_tabstop_align(&mut self.0) };
    }

    /// Write a section of tabular data using automatic column alignment.
    /// Creates a sub-buffer with the same human_readable setting, passes
    /// it to `f`, calls `tabstop_align()`, then appends the result.
    pub fn aligned(&mut self, f: impl FnOnce(&mut Printbuf)) {
        let mut sub = Printbuf::new();
        sub.set_human_readable(self.is_human_readable());
        f(&mut sub);
        sub.tabstop_align();
        fmt::Write::write_fmt(self, format_args!("{}", sub)).unwrap();
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

    pub fn is_human_readable(&self) -> bool {
        self.0.human_readable_units()
    }

    pub fn set_human_readable(&mut self, v: bool) {
        self.0.set_human_readable_units(v);
    }

    /// Whether this message is not to be printed: C's printbuf.suppress, for
    /// a message that's printed only if what it reports turns out to need
    /// it - scheduling a recovery pass, for one, clears it.
    pub fn is_suppressed(&self) -> bool {
        self.0.suppress()
    }

    pub fn set_suppressed(&mut self, v: bool) {
        self.0.set_suppress(v);
    }

    /// Print a human-readable representation of a u64 value.
    pub fn human_readable_u64(&mut self, v: u64) {
        unsafe { c::bch2_prt_human_readable_u64(&mut self.0, v) };
    }

    /// Print a bcachefs metadata version number.
    /// Print superblock contents.
    ///
    /// # Safety
    /// `fs` must be a valid pointer to a `bch_fs` or null.
    pub unsafe fn sb_to_text(&mut self, fs: *mut c::bch_fs, sb: &c::bch_sb,
                             layout: bool, fields: u32) {
        c::bch2_sb_to_text(&mut self.0, fs, sb as *const _ as *mut _, layout, fields);
    }

    /// Print a set of bitflags as comma-separated names.
    ///
    /// # Safety
    /// `list` must be a valid null-terminated array of C string pointers.
    pub unsafe fn prt_bitflags(&mut self, list: *const *const core::ffi::c_char, flags: u64) {
        c::bch2_prt_bitflags(&mut self.0, list, flags);
    }

    /// Access the underlying `c::printbuf` for calling C prt_* functions.
    pub fn as_raw(&mut self) -> &mut c::printbuf {
        &mut self.0
    }

    /// What write!() and writeln!() call: as fmt::Write's, but infallible -
    /// writing to a printbuf can't fail (allocation failure is recorded in
    /// the printbuf, as C's prt_printf() does), so there's no Result to
    /// discard. Inherent methods take precedence over trait methods, so this
    /// is the one write!() finds.
    pub fn write_fmt(&mut self, args: fmt::Arguments<'_>) {
        let _ = fmt::Write::write_fmt(self, args);
    }

    /// Write @bytes as they are - a name, which needn't be UTF-8, and whose
    /// newlines and tabs aren't indent or tabstops: as prt_bytes().
    pub fn write_bytes(&mut self, bytes: &[u8]) {
        unsafe {
            c::rust_prt_bytes(&mut self.0, bytes.as_ptr() as *const core::ffi::c_void,
                              bytes.len() as core::ffi::c_uint);
        }
    }
}

impl fmt::Write for Printbuf {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        unsafe {
            c::bch2_prt_bytes_indented(
                &mut self.0,
                s.as_ptr() as *const core::ffi::c_char,
                s.len() as core::ffi::c_uint,
            );
        }
        Ok(())
    }
}

impl Default for Printbuf {
    fn default() -> Self { Self::new() }
}

impl fmt::Display for Printbuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl c::printbuf {
    pub fn new() -> c::printbuf {
        let mut buf: c::printbuf = Default::default();

        buf.set_heap_allocated(true);
        buf
    }
}

impl Drop for c::printbuf {
    fn drop(&mut self) {
        unsafe { c::bch2_printbuf_exit(self) }
    }
}

impl fmt::Display for Bpos {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        printbuf_to_formatter(f, |buf| unsafe { c::bch2_bpos_to_text(buf, *self) })
    }
}

/// Display through a C *_to_text(): render into a printbuf made for this call,
/// then write it out. The printbuf only exists while something is actually
/// formatting. Its indentation and tabstops start from scratch, but `\n`,
/// `\t` and `\r` it leaves raw are interpreted by the destination, when that's
/// a Printbuf (see its fmt::Write).
///
/// Invalid UTF-8 comes out as U+FFFD, as to_string_lossy() would, without
/// allocating - so this works in-kernel too.
pub fn printbuf_to_formatter<F>(f: &mut fmt::Formatter<'_>, func: F) -> fmt::Result
where
    F: FnOnce(*mut c::printbuf),
{
    let mut buf = c::printbuf::new();

    func(&mut buf);

    if buf.buf.is_null() {
        return Ok(());
    }

    let bytes = unsafe { CStr::from_ptr(buf.buf) }.to_bytes();
    for chunk in bytes.utf8_chunks() {
        f.write_str(chunk.valid())?;
        if !chunk.invalid().is_empty() {
            f.write_char(char::REPLACEMENT_CHARACTER)?;
        }
    }
    Ok(())
}
