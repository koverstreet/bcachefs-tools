use core::ffi::CStr;
use core::fmt::{self, Write as _};
use core::ops::{Deref, DerefMut};

use crate::c;
use crate::c::bpos as Bpos;
use crate::util::os_str::{OsStr, OsStrExt};

/// fmt::Write goes through bch2_prt_bytes_indented(): `\t`, `\r` and `\n` are
/// tabstops and indent.
#[repr(transparent)]
pub struct Printbuf(c::printbuf);

/// Undoes Printbuf::indent() on drop.
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

/// Undoes Printbuf::atomic() on drop.
pub struct PrintbufAtomic<'a> {
    buf: &'a mut Printbuf,
}

impl Drop for PrintbufAtomic<'_> {
    fn drop(&mut self) {
        self.buf.0.atomic -= 1;
    }
}

impl Deref for PrintbufAtomic<'_> {
    type Target = Printbuf;
    fn deref(&self) -> &Printbuf { self.buf }
}

impl DerefMut for PrintbufAtomic<'_> {
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
            unsafe { CStr::from_ptr(self.0.buf.cast()) }
                .to_str()
                .unwrap_or("")
        }
    }

    /// What's been written, as a C string: to hand to C.
    pub fn as_c_str(&self) -> &CStr {
        if self.0.buf.is_null() {
            c""
        } else {
            // SAFETY: a printbuf's buffer is NUL-terminated
            unsafe { CStr::from_ptr(self.0.buf.cast()) }
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

    /// Indented until the guard drops.
    pub fn indent(&mut self, spaces: u32) -> PrintbufIndent<'_> {
        self.indent_add(spaces);
        PrintbufIndent { buf: self, spaces }
    }

    /// The lines after this one - not this one - indented by @spaces more.
    pub fn indent_add_nextline(&mut self, spaces: u32) {
        unsafe { c::bch2_printbuf_indent_add_nextline(&mut self.0, spaces) };
    }

    /// Allocating as from atomic context - under RCU, a spinlock - until the
    /// guard drops: guard(printbuf_atomic).
    pub fn atomic(&mut self) -> PrintbufAtomic<'_> {
        self.0.atomic += 1;
        PrintbufAtomic { buf: self }
    }

    /// `\t` in a format string.
    pub fn tab(&mut self) {
        unsafe { c::bch2_prt_tab(&mut self.0) };
    }

    /// `\r` in a format string: right-justify the text since the last tabstop.
    pub fn tab_rjust(&mut self) {
        unsafe { c::bch2_prt_tab_rjust(&mut self.0) };
    }

    /// Post-process the buffer, aligning columns separated by raw `\t`
    /// (left-aligned) and `\r` (right-aligned) characters. Call after
    /// writing a section of tabular data without preset tabstops.
    pub fn tabstop_align(&mut self) {
        unsafe { c::bch2_printbuf_tabstop_align(&mut self.0) };
    }

    /// Tabular data written by @f, aligned by tabstop_align().
    pub fn aligned(&mut self, f: impl FnOnce(&mut Printbuf)) {
        let mut sub = Printbuf::new();
        sub.set_human_readable(self.is_human_readable());
        f(&mut sub);
        sub.tabstop_align();
        fmt::Write::write_fmt(self, format_args!("{}", sub)).unwrap();
    }

    /// Where the text stands, for state_restore() to go back to:
    /// printbuf_state_save().
    pub fn state_save(&self) -> c::printbuf_restore {
        c::printbuf_restore {
            pos:          self.0.pos,
            last_newline: self.0.last_newline,
            last_field:   self.0.last_field,
            indent:       self.0.indent,
            cur_tabstop:  self.0.cur_tabstop,
        }
    }

    /// Undo what's been written since @s: printbuf_state_restore().
    pub fn state_restore(&mut self, s: c::printbuf_restore) {
        self.0.pos          = s.pos;
        self.0.last_newline = s.last_newline;
        self.0.last_field   = s.last_field;
        self.0.indent       = s.indent;
        self.0.cur_tabstop  = s.cur_tabstop;
    }

    pub fn indent_level(&self) -> u32 {
        self.0.indent
    }

    /// Room for @extra more bytes, allocated with @gfp: false if it
    /// couldn't. bch2_printbuf_make_room_gfp().
    pub fn make_room_gfp(&mut self, extra: u32, gfp: c::gfp_t) -> bool {
        unsafe { c::bch2_printbuf_make_room_gfp(&mut self.0, extra, gfp) == 0 }
    }

    /// `\n` in a format string.
    pub fn newline(&mut self) {
        unsafe { c::bch2_prt_newline(&mut self.0) };
    }

    /// Human-readable units if is_human_readable(), else raw.
    pub fn units_u64(&mut self, v: u64) {
        unsafe { c::bch2_prt_units_u64(&mut self.0, v) };
    }

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

    /// A UUID as printk's %pU: lowercase, hyphenated.
    pub fn uuid(&mut self, u: &c::__uuid_t) {
        for (i, b) in u.b.iter().enumerate() {
            if matches!(i, 4 | 6 | 8 | 10) {
                self.write_bytes(b"-");
            }
            write!(self, "{b:02x}");
        }
    }

    /// Empty, to write again - its buffer kept: printbuf_reset().
    pub fn reset(&mut self) {
        let b = &mut self.0;
        b.pos = 0;
        b.set_allocation_failure(false);
        b.last_newline = 0;
        b.last_field = 0;
        b.indent = 0;
        b.cur_tabstop = 0;
        b.nr_tabstops = 0;
        if b.size != 0 {
            // SAFETY: a buffer of size bytes
            unsafe { *b.buf = 0 };
        }
    }

    /// Whether an allocation failed, and what's written is cut short.
    pub fn allocation_failure(&self) -> bool {
        self.0.allocation_failure()
    }

    /// A newline at the end, unless there's one or nothing's written:
    /// bch2_printbuf_ensure_trailing_newline().
    pub fn ensure_trailing_newline(&mut self) {
        if self.as_bytes().last().is_some_and(|&b| b != b'\n') {
            self.newline();
        }
    }

    pub fn human_readable_u64(&mut self, v: u64) {
        unsafe { c::bch2_prt_human_readable_u64(&mut self.0, v) };
    }

    /// # Safety
    /// `fs` must be a valid pointer to a `bch_fs` or null.
    pub unsafe fn sb_to_text(&mut self, fs: *mut c::bch_fs, sb: &c::bch_sb,
                             layout: bool, fields: u32) {
        c::bch2_sb_to_text(&mut self.0, fs, sb as *const _ as *mut _, layout, fields);
    }

    /// As comma-separated names.
    ///
    /// # Safety
    /// `list` must be a valid null-terminated array of C string pointers.
    pub unsafe fn prt_bitflags(&mut self, list: *const *const crate::util::ffi::c_char, flags: u64) {
        c::bch2_prt_bitflags(&mut self.0, list, flags);
    }

    /// For the C prt_*() functions.
    pub fn as_raw(&mut self) -> &mut c::printbuf {
        &mut self.0
    }

    /// Shadows fmt::Write's - an inherent method wins - so write!() has no
    /// Result to discard: allocation failure is recorded in the printbuf, as
    /// prt_printf() does.
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
                s.as_ptr() as *const crate::util::ffi::c_char,
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

/// What prints itself to a Printbuf. A printbuf is a superset of a Formatter:
/// tabstops, indent and modes (human-readable units) are its state, which a
/// Display impl can't reach - so an object laid out over lines is ToText. A
/// value with nothing to lay out - a position, a name - is Display, and
/// write!() takes it directly.
pub trait ToText {
    fn to_text(&self, out: &mut Printbuf);
}

/// A message: format_args!() is one, written as given.
impl ToText for fmt::Arguments<'_> {
    fn to_text(&self, out: &mut Printbuf) {
        out.write_fmt(*self);
    }
}

/// A message, for the logging and fsck error macros: a format string and its
/// arguments, or anything ToText - a message object.
#[doc(hidden)]
#[macro_export]
macro_rules! message {
    ($fmt:literal $($args:tt)*) => {
        &format_args!($fmt $($args)*) as &dyn $crate::util::ToText
    };
    ($msg:expr) => {
        &$msg as &dyn $crate::util::ToText
    };
}

/// Display through a C *_to_text(), via a printbuf that lives only for the
/// call. Its indent and tabstops start from scratch; the `\n`, `\t` and `\r`
/// it leaves raw are interpreted by the destination, if that's a Printbuf.
///
/// Invalid UTF-8 comes out as U+FFFD without allocating, so this works
/// in-kernel too.
pub fn printbuf_to_formatter<F>(f: &mut fmt::Formatter<'_>, func: F) -> fmt::Result
where
    F: FnOnce(*mut c::printbuf),
{
    let mut buf = c::printbuf::new();

    func(&mut buf);

    if buf.buf.is_null() {
        return Ok(());
    }

    let bytes = unsafe { CStr::from_ptr(buf.buf.cast()) }.to_bytes();
    for chunk in bytes.utf8_chunks() {
        f.write_str(chunk.valid())?;
        if !chunk.invalid().is_empty() {
            f.write_char(char::REPLACEMENT_CHARACTER)?;
        }
    }
    Ok(())
}
