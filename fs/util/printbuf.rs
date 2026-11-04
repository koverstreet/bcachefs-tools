#![allow(non_camel_case_types)]

use core::ffi::{CStr, c_int, c_uint, c_ulong};
use core::fmt::{self, Write as _};
use core::ops::{Deref, DerefMut};
use crate::c;
use crate::util::alloc::{flags::GFP_KERNEL, KVVec};
use crate::util::kernel::PAGE_SIZE;
use crate::util::os_str::{OsStr, OsStrExt};
use crate::util::ffi::c_char;
use cstruct_macros::{CStruct, bitfield, c_const, c_enum, c_export, c_extern, c_name};
use nestify::nest;
use crate::format::Bpos;

c_enum! {
    #[closed]
    pub enum printbuf_si: u32 {
        PRINTBUF_UNITS_2,       /* use binary powers of 2^10 */
        PRINTBUF_UNITS_10,      /* use powers of 10^3 (standard SI) */
    }
}

c_const! {
    #[c_int]
    pub const PRINTBUF_INLINE_TABSTOPS: u32 = 8;
}

nest! {
    /// Through fmt::Write, `\t`, `\r` and `\n` are tabstops and indent.
    #[c_name(printbuf)]
    #[repr(C)]
    #[derive(CStruct)]
    pub struct Printbuf {
        pub buf: *mut c_char,
        pub size: c_uint,
        pub pos: c_uint,
        pub last_newline: c_uint,
        pub last_field: c_uint,
        pub indent: c_uint,
        /*
         * If nonzero, allocations will be done with GFP_ATOMIC:
         */
        pub atomic: u8,
        #[c_bitfield]
        #>[bitfield(u8)]
        pub bits: pub struct printbuf_bits {
            pub allocation_failure: bool,
            heap_allocated:         bool,
            overflow:               bool,
            /* Ratelimited or already printed */
            suppress:               bool,
            #[bits(1)] pub si_units: c::printbuf_si,
            human_readable_units:   bool,
            has_indent_or_tabstops: bool,
            pub may_vmalloc:        bool,
        },
        pub nr_tabstops: u8,

        /*
         * Do not modify directly: use printbuf_tabstop_add(),
         * printbuf_tabstop_get()
         */
        pub cur_tabstop: u8,
        pub _tabstops: [u8; c::PRINTBUF_INLINE_TABSTOPS as usize],
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default, CStruct)]
pub struct printbuf_restore {
    pub pos: c_uint,
    pub last_newline: c_uint,
    pub last_field: c_uint,
    pub indent: c_uint,
    pub cur_tabstop: u8,
}

c_extern! {
    #[c_printf(2, 3)]
    pub fn bch2_prt_printf(out: *mut Printbuf, fmt: *const c_char, ...);
}

unsafe impl Send for Printbuf {}

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
        self.buf.atomic -= 1;
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
    const fn empty() -> Self {
        Printbuf {
            buf: core::ptr::null_mut(), size: 0, pos: 0, last_newline: 0, last_field: 0, indent: 0, atomic: 0,
            bits: printbuf_bits::new(), nr_tabstops: 0, cur_tabstop: 0,
            _tabstops: [0; c::PRINTBUF_INLINE_TABSTOPS as usize],
        }
    }

    pub fn new() -> Self {
        let mut buf = Self::empty();
        buf.set_heap_allocated(true);
        buf
    }

    /// A printbuf over @size bytes at @buf that it doesn't own: it never
    /// grows past them - what doesn't fit is cut off - or frees them.
    ///
    /// # Safety
    /// @buf is @size writable bytes, for as long as this lives.
    pub unsafe fn new_extern(buf: *mut crate::util::ffi::c_char, size: usize) -> Self {
        Printbuf { buf, size: size as u32, ..Self::empty() }
    }

    pub fn as_str(&self) -> &str {
        if self.buf.is_null() {
            ""
        } else {
            unsafe { CStr::from_ptr(self.buf.cast()) }
                .to_str()
                .unwrap_or("")
        }
    }

    /// What's been written, as a C string: to hand to C.
    pub fn as_c_str(&self) -> &CStr {
        if self.buf.is_null() {
            c""
        } else {
            unsafe { CStr::from_ptr(self.buf.cast()) }
        }
    }

    /// What's written, cut back to @len bytes.
    pub fn truncate(&mut self, len: usize) {
        if len < self.pos() {
            self.rewind(len);
            self.nul_terminate_reserved();
        }
    }

    /// What's been written, as bytes - which needn't be UTF-8, after
    /// write_bytes().
    pub fn as_bytes(&self) -> &[u8] {
        if self.buf.is_null() {
            &[]
        } else {
            unsafe { core::slice::from_raw_parts(self.buf as *const u8, self.pos as usize) }
        }
    }

    /// What's been written, as a name: for building one.
    pub fn as_os_str(&self) -> &OsStr {
        OsStr::from_bytes(self.as_bytes())
    }

    /*
     * Add a tabstop, n spaces from the previous tabstop.
     *
     * In the future this function may allocate memory if setting more than
     * PRINTBUF_INLINE_TABSTOPS or setting tabstops more than 255 spaces from start
     * of line.
     */
    pub fn tabstop_push(&mut self, spaces: u32) -> bool {
        let nr = self.nr_tabstops as usize;
        let prev_tabstop = if nr != 0 { u32::from(self._tabstops[nr - 1]) } else { 0 };

        if crate::warn_on_once!(nr >= self._tabstops.len()) {
            return false;
        }

        self._tabstops[nr] = prev_tabstop.wrapping_add(spaces) as u8;
        self.nr_tabstops += 1;
        self.set_has_indent_or_tabstops(true);
        true
    }

    pub fn tabstop_pop(&mut self) {
        self.nr_tabstops = self.nr_tabstops.saturating_sub(1);
    }

    pub fn tabstops_reset(&mut self) {
        self.nr_tabstops = 0;
    }

    pub fn tabstops(&mut self, widths: &[u32]) {
        self.tabstops_reset();
        for &w in widths {
            self.tabstop_push(w);
        }
    }

    /*
     * Subsequent lines, and the current line if the output position is at the start
     * of the current line, will be indented by @spaces more spaces.
     */
    pub fn indent_add(&mut self, spaces: u32) {
        let indent = self.indent;
        self.indent_add_nextline(spaces);
        self.prt_chars(b' ', (self.indent - indent) as usize);
    }

    /*
     * Subsequent lines, and the current line if the output position is at the start
     * of the current line, will be indented by @spaces less spaces.
     */
    pub fn indent_sub(&mut self, spaces: u32) {
        let spaces = if crate::warn_on_once!(spaces > self.indent) { self.indent } else { spaces };

        if self.last_newline.wrapping_add(self.indent) == self.pos {
            self.rewind(self.pos() - spaces as usize);
            self.nul_terminate_reserved();
        }
        self.indent -= spaces;

        if self.indent == 0 && self.nr_tabstops == 0 {
            self.set_has_indent_or_tabstops(false);
        }
    }

    /// Indented until the guard drops.
    pub fn indent(&mut self, spaces: u32) -> PrintbufIndent<'_> {
        self.indent_add(spaces);
        PrintbufIndent { buf: self, spaces }
    }

    /*
     * Subsequent lines - not the current line - will be indented by @spaces more
     * spaces.
     */
    pub fn indent_add_nextline(&mut self, spaces: u32) {
        let spaces = if crate::warn_on_once!(self.indent.checked_add(spaces).is_none()) { 0 } else { spaces };

        self.indent += spaces;
        self.set_has_indent_or_tabstops(true);
    }

    /// Allocating as from atomic context - under RCU, a spinlock - until the
    /// guard drops.
    pub fn atomic(&mut self) -> PrintbufAtomic<'_> {
        self.atomic += 1;
        PrintbufAtomic { buf: self }
    }

    /*
     * Advance output to the next tabstop by printing spaces.
     */
    pub fn tab(&mut self) {
        if self.cur_tabstop() == 0 {
            self.write_bytes(b"\t");
            return;
        }

        self.prt_chars(b' ', self.cur_tabstop().saturating_sub(self.linelen_at(self.pos())));
        self.next_field(self.pos());
    }

    /*
     * Advance output to the next tabstop by inserting spaces immediately after the
     * previous tabstop, right justifying previously outputted text.
     */
    pub fn tab_rjust(&mut self) {
        if self.cur_tabstop() == 0 {
            self.write_bytes(b"\r");
            return;
        }

        let pad = self.cur_tabstop().saturating_sub(self.linelen_at(self.pos()));
        self.insert_spaces(self.last_field as usize, pad);
        self.next_field(self.pos());
    }

    /*
     * Post-processing pass that aligns columns separated by raw \t and \r
     * characters that weren't consumed by preset tabstops. \t columns are
     * left-aligned, \r columns are right-aligned.
     *
     * Can coexist with preset tabstops: preset tabstops handle early columns
     * during printing, this function handles any remaining raw tab characters.
     */
    pub fn tabstop_align(&mut self) {
        if self.pos == 0 {
            return;
        }

        let text = self.as_bytes();
        let is_sep = |c: &u8| matches!(c, b'\t' | b'\r' | b'\n');
        // each column's text and what ends it: the last line's, \n
        let cols = || text.split(is_sep).zip(text.iter().copied().filter(is_sep).chain([b'\n']));

        /* First pass: measure max column widths */
        let mut col_widths: KVVec<usize> = KVVec::new();
        let mut col = 0;

        for (s, c) in cols() {
            if col == col_widths.len() && col_widths.push(0, GFP_KERNEL).is_err() {
                return;
            }
            col_widths[col] = col_widths[col].max(s.len());
            col = if c == b'\n' { 0 } else { col + 1 };
        }

        /* Second pass: rebuild with aligned columns */
        let mut aligned = Printbuf::new();
        col = 0;

        for (s, c) in cols() {
            let pad = col_widths[col] - s.len();

            if c == b'\r' {
                aligned.prt_chars(b' ', pad);
            }

            aligned.write_bytes(s);

            if c == b'\t' {
                aligned.prt_chars(b' ', pad);
            }

            if c == b'\n' {
                aligned.write_bytes(b"\n");
                col = 0;
            } else {
                aligned.prt_chars(b' ', 2);
                col += 1;
            }
        }

        // swapped: aligned's drop frees the old buffer, if it was ours
        core::mem::swap(&mut self.buf, &mut aligned.buf);
        core::mem::swap(&mut self.size, &mut aligned.size);
        core::mem::swap(&mut self.pos, &mut aligned.pos);
        let heap_allocated = self.heap_allocated();
        self.set_heap_allocated(aligned.heap_allocated());
        aligned.set_heap_allocated(heap_allocated);

        // the rebuilt text ends with a newline
        self.last_newline = self.pos;
        self.last_field = self.pos;
        self.cur_tabstop = 0;
    }

    /// Tabular data written by @f, aligned by tabstop_align().
    pub fn aligned(&mut self, f: impl FnOnce(&mut Printbuf)) {
        let mut sub = Printbuf::new();
        sub.set_human_readable(self.is_human_readable());
        f(&mut sub);
        sub.tabstop_align();
        fmt::Write::write_fmt(self, format_args!("{}", sub)).unwrap();
    }

    /// Where the text stands, for state_restore() to go back to.
    pub fn state_save(&self) -> c::printbuf_restore {
        c::printbuf_restore {
            pos:          self.pos,
            last_newline: self.last_newline,
            last_field:   self.last_field,
            indent:       self.indent,
            cur_tabstop:  self.cur_tabstop,
        }
    }

    /// Undo what's been written since @s.
    pub fn state_restore(&mut self, s: c::printbuf_restore) {
        self.pos          = s.pos;
        self.last_newline = s.last_newline;
        self.last_field   = s.last_field;
        self.indent       = s.indent;
        self.cur_tabstop  = s.cur_tabstop;
    }

    pub fn nr_tabstops(&self) -> u32 {
        self.nr_tabstops.into()
    }

    pub fn indent_level(&self) -> u32 {
        self.indent
    }

    /// Room for @extra more bytes, allocated with @gfp: false if it
    /// couldn't.
    pub fn make_room_gfp(&mut self, extra: u32, gfp: c::gfp_t) -> bool {
        self.grow(extra as usize, gfp).is_ok()
    }

    fn make_room(&mut self, extra: usize) -> bool {
        self.grow(extra, c::GFP_KERNEL).is_ok()
    }

    /// Room for @extra more bytes and the nul: ENOSPC if the buffer isn't ours
    /// to grow, ENOMEM if it can't be.
    fn grow(&mut self, extra: usize, gfp: c::gfp_t) -> Result<(), u32> {
        /* Reserved space for terminating nul: */
        let need = self.pos() + extra + 1;

        if need <= self.size as usize {
            return Ok(());
        }

        if !self.heap_allocated() {
            self.set_overflow(true);
            return Err(c::ENOSPC);
        }

        let new_size = (self.size as usize + extra + 1).next_power_of_two();

        let may_vmalloc = self.may_vmalloc() && self.atomic == 0;

        /* Sanity check... */
        let max = if may_vmalloc { i32::MAX as usize } else { PAGE_SIZE << c::MAX_PAGE_ORDER };

        /*
         * Note: output buffer must be freeable with kfree(), it's not required
         * that the user use printbuf_exit().
         */
        let gfp = if self.atomic == 0 { gfp } else { c::GFP_NOWAIT };
        // what may be vmalloc()'d already is kvrealloc()'d atomic or not:
        // krealloc() of a vmalloc() address is a bug
        let buf = if new_size <= max {
            unsafe { realloc(self.buf, new_size, gfp, self.may_vmalloc()) }
        } else {
            core::ptr::null_mut()
        };

        if buf.is_null() {
            self.set_allocation_failure(true);
            self.set_overflow(true);
            return Err(c::ENOMEM);
        }

        self.buf = buf;
        self.size = new_size as u32;
        Ok(())
    }

    /// `\n` in a format string.
    pub fn newline(&mut self) {
        let indent = self.indent as usize;
        self.make_room(1 + indent);

        self.write_bytes(b"\n");
        self.last_newline = self.pos;
        self.prt_chars(b' ', indent);

        self.last_field = self.pos;
        self.cur_tabstop = 0;
    }

    pub fn is_human_readable(&self) -> bool {
        self.human_readable_units()
    }

    pub fn set_human_readable(&mut self, v: bool) {
        self.set_human_readable_units(v);
    }

    /// Whether this message is not to be printed: for a message that's
    /// printed only if what it reports turns out to need it - scheduling a
    /// recovery pass, for one, clears it.
    pub fn is_suppressed(&self) -> bool {
        self.suppress()
    }

    pub fn set_suppressed(&mut self, v: bool) {
        self.set_suppress(v);
    }

    /// Empty, to write again - its buffer kept.
    pub fn reset(&mut self) {
        let b = self;
        b.pos = 0;
        b.set_allocation_failure(false);
        b.last_newline = 0;
        b.last_field = 0;
        b.indent = 0;
        b.cur_tabstop = 0;
        b.nr_tabstops = 0;
        b.nul_terminate_reserved();
    }

    /// Drop the first @n bytes written, moving the rest - and the positions in
    /// it - to the front.
    pub fn drop_front(&mut self, n: usize) {
        let pos = self.as_bytes().len();
        assert!(n <= pos, "Printbuf::drop_front({n}) of {pos} bytes");
        if n == 0 {
            return;
        }

        let b = self;
        unsafe { core::ptr::copy(b.buf.add(n), b.buf, pos - n + 1) };

        let n = n as u32;
        b.pos -= n;
        b.last_newline = b.last_newline.saturating_sub(n);
        b.last_field = b.last_field.saturating_sub(n);
    }

    /*
     * Terminate @out with a newline if it doesn't already have one, for callers
     * that embed a printbuf in a larger message: a to_text/validate helper appends
     * its reason without a trailing newline - correct, since the caller frames it -
     * and unterminated it runs into whatever gets printed next.
     */
    pub fn ensure_trailing_newline(&mut self) {
        if self.as_bytes().last().is_some_and(|&b| b != b'\n') {
            self.newline();
        }
    }

    pub fn strip_trailing_newline(&mut self) {
        let written = self.as_bytes();
        if let Some(p) = written.iter().rposition(|&b| b != b' ').filter(|&p| written[p] == b'\n') {
            self.rewind(p);
        }

        self.nul_terminate_reserved();
    }

    /// As comma-separated names.
    ///
    /// # Safety
    /// `list` must be a valid null-terminated array of C string pointers.
    pub unsafe fn prt_bitflags(&mut self, list: *const *const c_char, flags: u64) {
        self.set_names(unsafe { c_strs(list) }.map(CStr::to_bytes), |i| i < 64 && flags & 1 << i != 0);
    }

    /// The names of the bits set in @flags, comma separated - a bit past the
    /// end of @names has none, and isn't printed.
    pub fn bitflags(&mut self, names: &[&str], flags: u64) {
        self.set_names(names.iter().map(|s| s.as_bytes()), |i| i < 64 && flags & 1 << i != 0);
    }

    /// The @names whose bit is @set, comma separated.
    fn set_names<'a>(&mut self, names: impl Iterator<Item = &'a [u8]>, set: impl Fn(usize) -> bool) {
        for (n, (_, name)) in names.enumerate().filter(|&(i, _)| set(i)).enumerate() {
            if n != 0 {
                self.write_bytes(b",");
            }
            self.write_bytes_indented(name);
        }
    }

    /// For the C prt_*() functions.
    pub fn as_raw(&mut self) -> *mut Printbuf {
        self
    }

    /// Shadows fmt::Write's - an inherent method wins - so write!() has no
    /// Result to discard: allocation failure is recorded in the printbuf.
    pub fn write_fmt(&mut self, args: fmt::Arguments<'_>) {
        let _ = fmt::Write::write_fmt(self, args);
    }

    /// Write @bytes as they are - a name, which needn't be UTF-8, and whose
    /// newlines and tabs aren't indent or tabstops.
    pub fn write_bytes(&mut self, bytes: &[u8]) {
        self.make_room(bytes.len());

        let n = bytes.len().min(self.remaining());
        let pos = self.pos();
        self.buffer()[pos..pos + n].copy_from_slice(&bytes[..n]);
        self.pos += n as u32;

        self.nul_terminate_reserved();
    }

    /*
     * Print an array of chars, handling embedded control characters:
     *   \n: prt_newline	newline that obeys current indent level
     *   \t: prt_tab	advance to next tabstop
     *   \r: prt_tab_rjust	advance to next tabstop, with right justification
     */
    pub fn write_bytes_indented(&mut self, bytes: &[u8]) {
        let indent_pos = self.pos();
        self.write_bytes(bytes);
        self.do_indent(indent_pos);
    }

    /*
     * Exit a printbuf, freeing memory it owns, and leave it empty: exiting it
     * again does nothing - as when C's teardown exits one that Rust's Drop then
     * exits too.
     */
    fn exit(&mut self) {
        if self.heap_allocated() {
            unsafe { c::rust_kvfree(self.buf.cast()) };
            // not *self =: that would drop what's freed
            unsafe { core::ptr::write(self, Printbuf::new()) };
        }
    }

    /*
     * Print out a u64 in human readable units: of 2^10 (default) or 10^3, as
     * @out->si_units says.
     */
    fn human_readable_u64(&mut self, mut v: u64) {
        self.make_room(10);

        const UNITS: &[u8] = b"\0kMGTPEZY";
        let base: u64 = if matches!(self.si_units(), c::printbuf_si::PRINTBUF_UNITS_10) { 1000 } else { 1024 };
        let (mut u, mut r) = (0, 0);

        while u + 1 < UNITS.len() && v >= base {
            r = v % base;
            v /= base;
            u += 1;
        }

        if u == 0 {
            write!(self, "{v}");
            return;
        }

        /*
         * Up to three significant digits, decimals rounded to nearest -
         * not truncated, and not rounded up like df -h. Remainders from
         * earlier divisions (under a thousandth of the last digit shown)
         * are ignored:
         */
        let prec_of = |v: u64| if v < 10 { 2 } else if v < 100 { 1 } else { 0 };
        let mut prec = prec_of(v);
        let scale = [1, 10, 100][prec];
        let mut frac = (r * scale + base / 2) / base;

        if frac >= scale {	/* rounded all the way up: carry */
            frac = 0;
            v += 1;
            if v >= base && u + 1 < UNITS.len() {
                v = 1;
                u += 1;
            }
            prec = prec_of(v);
        }

        let unit = UNITS[u] as char;
        if prec != 0 {
            write!(self, "{v}.{frac:0prec$}{unit}");
        } else {
            write!(self, "{v}{unit}");
        }
    }

    /*
     * Print out a u64 according to printbuf unit options: raw (default), or
     * human reabable units (@buf->human_readable_units)
     */
    fn units_u64(&mut self, v: u64) {
        if self.human_readable_units() {
            self.human_readable_u64(v);
        } else {
            write!(self, "{v}");
        }
    }
}

// ── The buffer ───────────────────────────────────────────────────────────

impl Printbuf {
    /// All of it, written or not.
    fn buffer(&mut self) -> &mut [u8] {
        if self.buf.is_null() {
            &mut []
        } else {
            unsafe { core::slice::from_raw_parts_mut(self.buf.cast(), self.size as usize) }
        }
    }

    fn pos(&self) -> usize {
        self.pos as usize
    }

    /// Back to @pos, last_newline and last_field kept at or before it: a
    /// line's length is pos - last_newline, and tab_rjust() moves the text
    /// from last_field on.
    fn rewind(&mut self, pos: usize) {
        self.pos = pos as u32;
        self.last_field = self.last_field.min(self.pos);
        if self.last_newline > self.pos {
            self.last_newline = self.as_bytes().iter().rposition(|&c| c == b'\n').map_or(0, |i| i + 1) as u32;
        }
    }

    /// Back inside the buffer, a nul's room left, with a warning.
    fn clamp_pos(&mut self) {
        if crate::warn_on!(self.size != 0 && self.pos >= self.size) {
            self.pos = self.size - 1;
        }
    }

    /*
     * Returns number of characters we can print to the output buffer - i.e.
     * excluding the terminating nul:
     */
    fn remaining(&mut self) -> usize {
        self.clamp_pos();
        (self.size as usize).saturating_sub(self.pos() + 1)
    }

    fn nul_terminate_reserved(&mut self) {
        self.clamp_pos();
        let pos = self.pos();
        if let Some(c) = self.buffer().get_mut(pos) {
            *c = 0;
        }
    }

    fn nul_terminate(&mut self) {
        self.make_room(1);
        self.nul_terminate_reserved();
    }

    fn prt_chars(&mut self, c: u8, n: usize) {
        self.make_room(n);

        let n = n.min(self.remaining());
        let pos = self.pos();
        self.buffer()[pos..pos + n].fill(c);
        self.pos += n as u32;

        self.nul_terminate_reserved();
    }

    /// @nr spaces at @at, what's written after it moved up - what doesn't fit
    /// cut off.
    fn insert_spaces(&mut self, at: usize, nr: usize) {
        if nr == 0 {
            return;
        }

        let mv = self.pos().wrapping_sub(at);

        self.make_room(nr);

        let size = self.size as usize;
        if at + nr < size {
            self.buffer().copy_within(at..at + mv.min(size - 1 - at - nr), at + nr);
        }
        if at < size {
            self.buffer()[at..(at + nr).min(size)].fill(b' ');
        }

        self.pos += nr.min(self.remaining()) as u32;
        self.nul_terminate_reserved();
    }

    /// The byte at @at taken out, what follows it - the nul too - moved down.
    fn remove_byte(&mut self, at: usize) {
        let end = self.pos();
        self.buffer().copy_within(at + 1..end + 1, at);
        self.pos -= 1;
    }

    fn linelen_at(&self, at: usize) -> usize {
        at.wrapping_sub(self.last_newline as usize)
    }

    /*
     * Returns spaces from start of line, if set, or 0 if unset:
     */
    fn cur_tabstop(&self) -> usize {
        if self.cur_tabstop < self.nr_tabstops {
            self._tabstops[self.cur_tabstop as usize].into()
        } else {
            0
        }
    }

    /// The field after the one ending at @at.
    fn next_field(&mut self, at: usize) {
        self.last_field = at as u32;
        self.cur_tabstop = self.cur_tabstop.wrapping_add(1);
    }

    /// What's written since @pos: \n indented, \t and \r to tabstops.
    fn do_indent(&mut self, pos: usize) {
        if self.has_indent_or_tabstops() {
            self.do_indent_from(pos);
        }
    }

    fn do_indent_from(&mut self, mut pos: usize) {
        loop {
            let tabstops = self.cur_tabstop() != 0;
            let Some(i) = self.as_bytes()[pos..].iter()
                .position(|&c| c == b'\n' || tabstops && (c == b'\r' || c == b'\t')) else { break };
            pos += i;

            match self.as_bytes()[pos] {
                b'\n' => {
                    pos += 1;
                    self.last_newline = pos as u32;

                    let indent = self.indent as usize;
                    self.insert_spaces(pos, indent);

                    pos = (pos + indent).min(self.pos());
                    self.last_field = pos as u32;
                    self.cur_tabstop = 0;
                    continue;
                }
                b'\r' => {
                    self.remove_byte(pos);
                    let pad = self.cur_tabstop().saturating_sub(self.linelen_at(pos));
                    self.insert_spaces(self.last_field as usize, pad);
                    pos += pad;
                }
                _ => match self.cur_tabstop().checked_sub(self.linelen_at(pos) + 1).filter(|&pad| pad > 0) {
                    Some(pad) => {
                        self.buffer()[pos] = b' ';
                        self.insert_spaces(pos, pad - 1);
                        pos += pad;
                    }
                    None => self.remove_byte(pos),
                },
            }

            pos = pos.min(self.pos());
            self.next_field(pos);
        }
    }
}

/// krealloc(), or kvrealloc() if @vmalloc, as @gfp says: what C's kfree() or
/// kvfree() takes back. Userspace's are both krealloc().
///
/// # Safety
/// @p is null, or this's, and not used after unless this fails.
unsafe fn realloc(p: *mut c_char, size: usize, gfp: c::gfp_t, vmalloc: bool) -> *mut c_char {
    #[cfg(kernel)]
    unsafe {
        use kernel::bindings::{krealloc_node_align, kvrealloc_node_align, NUMA_NO_NODE};

        if vmalloc {
            kvrealloc_node_align(p.cast(), size, 1, gfp, NUMA_NO_NODE).cast()
        } else {
            krealloc_node_align(p.cast(), size, 1, gfp, NUMA_NO_NODE).cast()
        }
    }
    #[cfg(not(kernel))]
    {
        let _ = vmalloc;
        unsafe { bcachefs_shim::alloc::krealloc(p.cast(), size, gfp) }.cast()
    }
}

/// The strings of a NULL-terminated C list: a name table's.
///
/// # Safety
/// @list is one, its strings live as long as 'a.
unsafe fn c_strs<'a>(list: *const *const c_char) -> impl Iterator<Item = &'a CStr> {
    (0..).map(move |i| unsafe { *list.add(i) })
        .take_while(|p| !p.is_null())
        .map(|p| unsafe { CStr::from_ptr(p.cast()) })
}

// ── For C ────────────────────────────────────────────────────────────────

/// C's name for a method: `export!(bch2_prt_tab() => tab);`.
macro_rules! export {
    ($name:ident($($arg:ident: $t:ty),*) $(-> $r:ty)? => $method:ident) => {
        #[c_export]
        pub fn $name(out: &mut Printbuf $(, $arg: $t)*) $(-> $r)? { out.$method($($arg),*) }
    };
}

export!(bch2_printbuf_exit() => exit);
export!(bch2_printbuf_tabstops_reset() => tabstops_reset);
export!(bch2_printbuf_tabstop_pop() => tabstop_pop);
export!(bch2_printbuf_indent_add(spaces: c_uint) => indent_add);
export!(bch2_printbuf_indent_add_nextline(spaces: c_uint) => indent_add_nextline);
export!(bch2_printbuf_indent_sub(spaces: c_uint) => indent_sub);
export!(bch2_prt_newline() => newline);
export!(bch2_printbuf_ensure_trailing_newline() => ensure_trailing_newline);
export!(bch2_printbuf_strip_trailing_newline() => strip_trailing_newline);
export!(bch2_prt_tab() => tab);
export!(bch2_prt_tab_rjust() => tab_rjust);
export!(bch2_prt_human_readable_u64(v: u64) => human_readable_u64);
export!(bch2_prt_units_u64(v: u64) => units_u64);
export!(bch2_printbuf_tabstop_align() => tabstop_align);
export!(bch2_printbuf_reset() => reset);

export!(bch2_printbuf_nul_terminate() => nul_terminate);

#[c_export]
pub fn bch2_printbuf_remaining(out: &mut Printbuf) -> c_uint {
    out.remaining() as c_uint
}

#[c_export]
pub fn bch2_prt_chars(out: &mut Printbuf, c: u8, n: c_uint) {
    out.prt_chars(c, n as usize)
}

/// Contiguous hex dump, same output as printk's %*phN - which the userspace
/// build can't use: libc vsnprintf prints kernel %p extensions as raw
/// pointers.
///
/// # Safety
/// @b is @n bytes.
#[c_export]
pub unsafe fn bch2_prt_hex_bytes(out: &mut Printbuf, b: *const u8, n: c_uint) {
    for b in unsafe { core::slice::from_raw_parts(b, n as usize) } {
        write!(out, "{b:02x}");
    }
}

/// For prt_printf(), which stays C: what vsnprintf() wrote since @pos.
#[c_export]
pub fn bch2_printbuf_do_indent(out: &mut Printbuf, pos: c_uint) {
    out.do_indent(pos as usize)
}

#[c_export]
pub fn bch2_printbuf_make_room(out: &mut Printbuf, extra: c_uint) -> c_int {
    out.grow(extra as usize, c::GFP_KERNEL).map_or_else(|e| -(e as c_int), |()| 0)
}

/*
 * Returns printbuf's buf as a C string, guaranteed to be null terminated.
 */
#[c_export]
pub fn bch2_printbuf_str(buf: &Printbuf) -> *const c_char {
    /*
     * If we've written to a printbuf then it's guaranteed to be a null
     * terminated string - but if we haven't, then we might not have
     * allocated a buffer at all:
     */
    if buf.pos != 0 { buf.buf } else { c"".as_ptr().cast() }
}

#[c_export]
pub fn bch2_printbuf_tabstop_push(buf: &mut Printbuf, spaces: c_uint) -> c_int {
    if buf.tabstop_push(spaces) { 0 } else { -(c::EINVAL as c_int) }
}

/// # Safety
/// @str is @count bytes.
#[c_export]
pub unsafe fn bch2_prt_bytes_indented(out: &mut Printbuf, str: *const c_char, count: c_uint) {
    out.write_bytes_indented(unsafe { core::slice::from_raw_parts(str.cast(), count as usize) })
}

#[c_export]
pub fn bch2_prt_human_readable_s64(out: &mut Printbuf, v: i64) {
    out.write_bytes(if v < 0 { b"-" } else { b"" });
    out.human_readable_u64(v.unsigned_abs())
}

#[c_export]
pub fn bch2_prt_units_s64(out: &mut Printbuf, v: i64) {
    out.write_bytes(if v < 0 { b"-" } else { b"" });
    out.units_u64(v.unsigned_abs())
}

/// # Safety
/// @list is NULL-terminated.
#[c_export]
pub unsafe fn bch2_prt_string_option(out: &mut Printbuf, list: *const *const c_char, selected: usize) {
    for (i, s) in unsafe { c_strs(list) }.enumerate() {
        let (open, close): (&[u8], &[u8]) = if i == selected { (b"[", b"] ") } else { (b"", b" ") };
        out.write_bytes(open);
        out.write_bytes_indented(s.to_bytes());
        out.write_bytes(close);
    }
}

/// # Safety
/// @list is NULL-terminated.
#[c_export]
pub unsafe fn bch2_prt_bitflags(out: &mut Printbuf, list: *const *const c_char, flags: u64) {
    unsafe { out.prt_bitflags(list, flags) }
}

/// # Safety
/// @list is NULL-terminated, and @v @nr bits.
#[c_export]
pub unsafe fn bch2_prt_bitflags_vector(out: &mut Printbuf, list: *const *const c_char, v: *mut c_ulong, nr: c_uint) {
    let bits = c_ulong::BITS as usize;
    out.set_names(unsafe { c_strs(list) }.take(nr as usize).map(CStr::to_bytes),
                  |i| unsafe { *v.add(i / bits) } >> (i % bits) & 1 != 0);
}

/// @bdev's name: as the kernel's %pg prints it, its disk's and partition's;
/// in userspace, the device's path, its last component. NULL is "(null)", as
/// %pg prints it: C names the filesystem's superblock's device, which has
/// none.
#[cstruct_macros::c_export]
pub fn prt_bdevname(out: &mut Printbuf, bdev: Option<&c::block_device>) {
    let Some(bdev) = bdev else {
        out.write_bytes(b"(null)");
        return;
    };
    #[cfg(kernel)]
    unsafe { c::bch2_prt_printf(out.as_raw(), c"%pg".as_ptr().cast(), core::ptr::from_ref(bdev)) };
    #[cfg(not(kernel))]
    {
        let name = bdev.name.map(|c| c as u8);
        out.write_bytes(CStr::from_bytes_until_nul(&name).map_or(&name[..], CStr::to_bytes));
    }
}

impl fmt::Write for Printbuf {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.atomic & Printbuf::FMT_JOB != 0 {
            self.atomic &= !Printbuf::FMT_JOB;
            // after Probe::Job, pass_through()'s empty str: its address is
            // that of a job on pass_through()'s stack, exposed
            let job = unsafe {
                *core::ptr::with_exposed_provenance::<&dyn Fn(&mut Printbuf)>(s.as_ptr().addr())
            };
            job(self);
            return Ok(());
        }

        match Probe::of(s) {
            Some(Probe::IsPrintbuf) => return Err(fmt::Error),
            Some(Probe::Job) => {
                self.atomic |= Printbuf::FMT_JOB;
                return Ok(());
            }
            None => {}
        }

        self.write_bytes_indented(s.as_bytes());
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

impl Drop for Printbuf {
    fn drop(&mut self) {
        self.exit()
    }
}

impl ToText for Bpos {
    fn to_text(&self, out: &mut Printbuf) {
        unsafe { c::bch2_bpos_to_text(out.as_raw(), *self) }
    }
}
crate::display_via_to_text!(Bpos);

/// As printk's %pU: lowercase, hyphenated.
impl ToText for c::__uuid_t {
    fn to_text(&self, out: &mut Printbuf) {
        for (i, b) in self.b.iter().enumerate() {
            write!(out, "{}{b:02x}", if matches!(i, 4 | 6 | 8 | 10) { "-" } else { "" });
        }
    }
}

// ── Values that print themselves ─────────────────────────────────────────
//
// A number's unit says how it's printed - a size in units, a time as a date
// - so it goes in a format string as what it is: `prt!(out, "size:\t{}\n",
// Sectors(s))`, with nothing printed piecemeal around it.

/// A size in bytes: in units if the printbuf is human-readable, else the
/// number.
pub struct Bytes(pub u64);

/// A size in 512-byte sectors, printed as Bytes.
pub struct Sectors(pub u64);

/// A size in bytes, in human-readable units whatever the printbuf's mode -
/// u64 or i64.
pub struct HumanReadable<T = u64>(pub T);

/// A time, in seconds since the epoch.
pub struct DateTime(pub i64);

/// A duration in nanoseconds, in the largest unit it has two of, truncated:
/// util/time_stats.rs's.
pub struct Nsecs(pub u64);

impl ToText for Bytes {
    fn to_text(&self, out: &mut Printbuf) {
        out.units_u64(self.0)
    }
}

impl ToText for Sectors {
    fn to_text(&self, out: &mut Printbuf) {
        Bytes(self.0 << 9).to_text(out)
    }
}

impl ToText for HumanReadable<u64> {
    fn to_text(&self, out: &mut Printbuf) {
        out.human_readable_u64(self.0)
    }
}

impl ToText for HumanReadable<i64> {
    fn to_text(&self, out: &mut Printbuf) {
        if self.0 < 0 {
            write!(out, "-");
        }
        HumanReadable(self.0.unsigned_abs()).to_text(out)
    }
}

impl ToText for DateTime {
    fn to_text(&self, out: &mut Printbuf) {
        unsafe { c::bch2_prt_datetime(out, self.0) };
    }
}

/// @iter's items with @sep between each: `prt!(out, "{}", Join(", ", offsets))`.
pub struct Join<'s, I>(pub &'s str, pub I);

impl<I: Iterator<Item: fmt::Display> + Clone> ToText for Join<'_, I> {
    fn to_text(&self, out: &mut Printbuf) {
        for (i, x) in self.1.clone().enumerate() {
            write!(out, "{}{x}", if i != 0 { self.0 } else { "" });
        }
    }
}

crate::display_via_to_text!(impl[I: Iterator<Item: fmt::Display> + Clone] Join<'_, I>);
crate::display_via_to_text!(Bytes);
crate::display_via_to_text!(Sectors);
crate::display_via_to_text!(HumanReadable<u64>);
crate::display_via_to_text!(HumanReadable<i64>);
crate::display_via_to_text!(DateTime);
crate::display_via_to_text!(Nsecs);

/// What prints itself to a Printbuf. A printbuf is a superset of a Formatter:
/// tabstops, indent and modes (human-readable units) are its state, which a
/// Display impl can't reach - so an object laid out over lines is ToText. A
/// value with nothing to lay out - a position, a name - is Display, and
/// write!() takes it directly. A message's format string, or prt!()'s, takes
/// either: what's ToText is to_text()'d into the printbuf.
pub trait ToText {
    fn to_text(&self, out: &mut Printbuf);
}

/// `#[derive(ToText)]`: a fieldless enum, printed as its variant's name, or
/// its `#[label("..")]` - and Display through it. With the trait, one name.
pub use cstruct_macros::ToText;

/// Display for a type that's ToText, from its to_text(), for a format string
/// that isn't a message: text_fmt() - into the printbuf, for write!() into
/// one; through a printbuf of its own for format!(), println!(). (Not a
/// blanket impl: Display is core's.) A generic type's parameters go first:
/// `display_via_to_text!(impl[V: BkeyVal] BkeySCToText<'_, '_, V>)`.
#[macro_export]
macro_rules! display_via_to_text {
    (impl[$($g:tt)*] $t:ty) => {
        impl<$($g)*> ::core::fmt::Display for $t {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                $crate::util::printbuf::text_fmt(self, f)
            }
        }
    };
    ($t:ty) => {
        $crate::display_via_to_text!(impl[] $t);
    };
}

/// A message to the writer under a Formatter, which Display can't otherwise
/// see: f.write_str() of an empty str whose address is the message. Every
/// writer gets it as a write of nothing; Printbuf's write_str() alone knows
/// the address. The addresses are inside PROBES, so no other empty str has
/// them: one off the end of an object next to it is at PROBES[0] or past its
/// end.
#[derive(Clone, Copy)]
enum Probe {
    /// Answered with Err - the one bit write_str() returns - by a Printbuf.
    IsPrintbuf,
    /// The next write is pass_through()'s job: Printbuf::FMT_JOB.
    Job,
}

static PROBES: [u8; 3] = [0; 3];

impl Probe {
    const ALL: [Probe; 2] = [Probe::IsPrintbuf, Probe::Job];

    fn as_str(self) -> &'static str {
        let i = self as usize + 1;
        core::str::from_utf8(&PROBES[i..i]).unwrap()
    }

    fn of(s: &str) -> Option<Probe> {
        Probe::ALL.into_iter().find(|q| core::ptr::eq(s.as_ptr(), q.as_str().as_ptr()))
    }
}

impl Printbuf {
    /// Set by Probe::Job, until the job's write: the top bit of atomic, a
    /// nesting count that never gets near it. Set only between two writes
    /// that allocate nothing, so C's test of atomic never sees it.
    const FMT_JOB: u8 = 1 << 7;
}

/// Whether @f writes to a Printbuf - not through a Formatter's PadAdapter
/// (`{:#?}`), which drops empty writes.
pub fn is_printbuf(f: &mut fmt::Formatter<'_>) -> bool {
    f.write_str(Probe::IsPrintbuf.as_str()).is_err()
}

/// Run @job on the Printbuf @f writes to: Probe::Job, then an empty str at
/// the job's address - exposed, as an empty str carries no provenance.
fn pass_through(f: &mut fmt::Formatter<'_>, job: &dyn Fn(&mut Printbuf)) -> fmt::Result {
    f.write_str(Probe::Job.as_str())?;

    let addr = core::ptr::from_ref(&job).expose_provenance();
    let s = unsafe {
        core::str::from_utf8_unchecked(core::slice::from_raw_parts(core::ptr::without_provenance::<u8>(addr), 0))
    };
    f.write_str(s)
}

/// @t as Display: to_text() into the destination, if that's a Printbuf - its
/// indent, tabstops and atomic as they are; else via a printbuf of its own.
#[doc(hidden)]
pub fn text_fmt(t: &(impl ToText + ?Sized), f: &mut fmt::Formatter<'_>) -> fmt::Result {
    if is_printbuf(f) {
        pass_through(f, &|out| t.to_text(out))
    } else {
        text_fmt_via_printbuf(t, f)
    }
}

/// For a destination that isn't a Printbuf: a printbuf that lives only for
/// the call - out of line, so text_fmt() into one doesn't have it on its
/// stack. Its indent and tabstops start from scratch. Invalid UTF-8 comes out
/// as U+FFFD without allocating, so this works in-kernel too.
#[inline(never)]
fn text_fmt_via_printbuf(t: &(impl ToText + ?Sized), f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let mut buf = Printbuf::new();
    t.to_text(&mut buf);

    for chunk in buf.as_bytes().utf8_chunks() {
        f.write_str(chunk.valid())?;
        if !chunk.invalid().is_empty() {
            f.write_char(char::REPLACEMENT_CHARACTER)?;
        }
    }
    Ok(())
}

impl<T: ToText + ?Sized> ToText for &T {
    fn to_text(&self, out: &mut Printbuf) {
        (**self).to_text(out)
    }
}

impl<T: ToText + ?Sized> ToText for &mut T {
    fn to_text(&self, out: &mut Printbuf) {
        (**self).to_text(out)
    }
}

/// What's been written, as a message - C's, formatted before Rust gets it -
/// indented as @out is, as if formatted there.
impl ToText for Printbuf {
    fn to_text(&self, out: &mut Printbuf) {
        out.write_bytes_indented(self.as_bytes())
    }
}

/// A message made by message!() from a format string: what writes it.
#[doc(hidden)]
pub struct Msg<F>(pub F);

impl<F: Fn(&mut Printbuf)> ToText for Msg<F> {
    fn to_text(&self, out: &mut Printbuf) {
        (self.0)(out)
    }
}

/// How __prt!() writes an argument with no format spec: to_text() if it's
/// ToText, else as Display - picked at compile time, by method resolution
/// trying Arg before &Arg.
#[doc(hidden)]
pub mod prt_arg {
    use super::{Printbuf, ToText};
    use core::fmt;

    pub struct Arg<'a, T: ?Sized>(pub &'a T);

    pub trait ViaText {
        fn prt(&self, out: &mut Printbuf);
    }

    impl<T: ToText + ?Sized> ViaText for Arg<'_, T> {
        fn prt(&self, out: &mut Printbuf) {
            self.0.to_text(out)
        }
    }

    pub trait ViaDisplay {
        fn prt(&self, out: &mut Printbuf);
    }

    impl<T: fmt::Display + ?Sized> ViaDisplay for &Arg<'_, T> {
        fn prt(&self, out: &mut Printbuf) {
            write!(out, "{}", self.0)
        }
    }
}

/// write!() into a printbuf, with what's ToText to_text()'d into it, as in a
/// message!(): @out derefs to the Printbuf - `out`, `&mut buf`, a LogMsg.
#[macro_export]
macro_rules! prt {
    ($out:expr, $($fmt:tt)*) => {
        $crate::__prt!($crate, $out, $($fmt)*)
    };
}

/// A message, for the logging and fsck error macros: a format string and its
/// arguments, written into the message's printbuf by __prt!() - an argument
/// that's ToText to_text()'d into it, with no printbuf of its own; or anything
/// ToText, a message object.
#[doc(hidden)]
#[macro_export]
macro_rules! message {
    ($fmt:literal $($args:tt)*) => {
        &$crate::util::printbuf::Msg(|out: &mut $crate::util::Printbuf| {
            $crate::__prt!($crate, out, $fmt $($args)*)
        }) as &dyn $crate::util::ToText
    };
    ($msg:expr) => {
        &$msg as &dyn $crate::util::ToText
    };
}
