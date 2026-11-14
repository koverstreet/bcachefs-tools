#![allow(non_camel_case_types)]

use core::ffi::{CStr, c_uint, c_void};
use core::fmt::{self, Write as _};
use core::ops::{Deref, DerefMut};
use crate::c;
use crate::errcode::BchError;
use crate::util::alloc::KVVec;
use crate::os::kernel::PAGE_SIZE;
use crate::util::os_str::{OsStr, OsStrExt};
use crate::util::ffi::c_char;
use cstruct_macros::{MaybeZeroable, bitfield, bch_enum, BitfieldMembers};
use nestify::nest;
use crate::format::Bpos;

#[bch_enum(closed)]
#[repr(u32)]
pub enum PrintbufSi {
    PRINTBUF_UNITS_2,       /* use binary powers of 2^10 */
    PRINTBUF_UNITS_10,      /* use powers of 10^3 (standard SI) */
}

pub const PRINTBUF_INLINE_TABSTOPS: u32 = 8;

nest! {
    /// Through fmt::Write, `\t`, `\r` and `\n` are tabstops and indent.
    #[repr(C)]
    #[derive(MaybeZeroable, BitfieldMembers)]
    pub struct Printbuf {
        pub buf: *mut c_char,
        pub size: c_uint,
        pub pos: c_uint,
        pub last_newline: c_uint,
        pub last_field: c_uint,
        pub indent: c_uint,
        /// Set by Probe::Job, until the job's write.
        fmt_job: bool,
        #[members]
        #>[bitfield(u8)]
        pub bits: pub struct printbuf_bits {
            pub allocation_failure: bool,
            heap_allocated:         bool,
            overflow:               bool,
            /* Ratelimited or already printed */
            suppress:               bool,
            #[bits(1)] pub si_units: c::PrintbufSi,
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
#[derive(Clone, Copy, Default, MaybeZeroable)]
pub struct printbuf_restore {
    pub pos: c_uint,
    pub last_newline: c_uint,
    pub last_field: c_uint,
    pub indent: c_uint,
    pub cur_tabstop: u8,
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

impl Printbuf {
    const fn empty() -> Self {
        Printbuf {
            buf: core::ptr::null_mut(), size: 0, pos: 0, last_newline: 0, last_field: 0, indent: 0, fmt_job: false,
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
            if col == col_widths.len() && col_widths.push(0).is_err() {
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

    /// Room for @extra more bytes: false if it couldn't.
    pub fn make_room(&mut self, extra: usize) -> bool {
        self.grow(extra).is_ok()
    }

    /// Room for @extra more bytes and the nul: ENOSPC if the buffer isn't ours
    /// to grow, ENOMEM if it can't be.
    fn grow(&mut self, extra: usize) -> Result<(), u32> {
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

        /* Sanity check... */
        let max = if self.may_vmalloc() { i32::MAX as usize } else { PAGE_SIZE << c::MAX_PAGE_ORDER };

        /*
         * Note: output buffer must be freeable with kfree(), it's not required
         * that the user use printbuf_exit().
         */
        let buf = if new_size <= max {
            unsafe { realloc(self.buf, new_size, self.may_vmalloc()) }
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

    /// The names of the bits set in @flags, comma separated - a bit past the
    /// end of @names has none, and isn't printed.
    pub fn bitflags(&mut self, names: &[&str], flags: u64) {
        let set = names.iter().enumerate().filter(|&(i, _)| i < 64 && flags & 1 << i != 0);
        for (n, (_, name)) in set.enumerate() {
            if n != 0 {
                self.write_bytes(b",");
            }
            self.write_bytes_indented(name.as_bytes());
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

    /// What @f writes in place at the end, given room for @max bytes: it
    /// returns how many it wrote. False, nothing written, if there's no room.
    ///
    /// # Safety
    /// @f writes at most @max bytes, and returns how many it wrote.
    pub unsafe fn write_in_place(&mut self, max: usize, f: impl FnOnce(*mut u8) -> usize) -> bool {
        if !self.make_room(max) {
            return false;
        }

        let pos = self.pos();
        let n = f(unsafe { self.buf.cast::<u8>().add(pos) });
        self.pos += n as u32;

        self.nul_terminate_reserved();
        true
    }

    /// @p, as the kernel's printf extension @fmt prints it - %pS, %ps, %pg:
    /// only snprintf() knows them.
    pub fn prt_ptr(&mut self, fmt: &CStr, p: *const c_void) {
        let len = loop {
            let size = if self.size != 0 { self.remaining() + 1 } else { 0 };
            let len = unsafe { c::snprintf(self.buf.wrapping_add(self.pos()), size as _, fmt.as_ptr().cast(), p) };
            let len = len.max(0) as usize;
            if len > self.remaining() && self.make_room(len) {
                continue;
            }
            break len;
        };

        let indent_pos = self.pos();
        self.pos += len.min(self.remaining()) as u32;
        self.do_indent(indent_pos);
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
            unsafe { crate::util::alloc::kvfree_unsized(self.buf.cast()) };
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
        let base: u64 = if matches!(self.si_units(), c::PrintbufSi::PRINTBUF_UNITS_10) { 1000 } else { 1024 };
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

/// krealloc(), or kvrealloc() if @vmalloc, as the task's memalloc scope says:
/// what C's kfree() or kvfree() takes back. Userspace's are both krealloc().
///
/// # Safety
/// @p is null, or this's, and not used after unless this fails.
unsafe fn realloc(p: *mut c_char, size: usize, vmalloc: bool) -> *mut c_char {
    #[cfg(kernel)]
    unsafe {
        use kernel::bindings::{krealloc_node_align, kvrealloc_node_align, GFP_KERNEL, NUMA_NO_NODE};

        let gfp = crate::util::alloc::current_gfp_context_raw(GFP_KERNEL);
        if vmalloc {
            kvrealloc_node_align(p.cast(), size, 1, gfp, NUMA_NO_NODE).cast()
        } else {
            krealloc_node_align(p.cast(), size, 1, gfp, NUMA_NO_NODE).cast()
        }
    }
    #[cfg(not(kernel))]
    {
        let _ = vmalloc;
        unsafe { bcachefs_shim::alloc::krealloc(p.cast(), size, c::GFP_KERNEL) }.cast()
    }
}

/// @bdev's name: os::blkdev::prt_name()'s. NULL is "(null)", as %pg prints
/// it: C names the filesystem's superblock's device, which has none.
pub fn prt_bdevname(out: &mut Printbuf, bdev: Option<&crate::os::blkdev::BlockDevice>) {
    match bdev {
        Some(bdev) => crate::os::blkdev::prt_name(out, bdev),
        None       => out.write_bytes(b"(null)"),
    }
}

impl fmt::Write for Printbuf {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.fmt_job {
            self.fmt_job = false;
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
                self.fmt_job = true;
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
        crate::btree::bkey::bpos_to_text(out, *self)
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

/// UTC, as the kernel's %ptT prints it: 2026-11-03T22:41:07. Ours, not
/// %ptT or ctime_r(), so the kernel and the tools print the same thing.
impl ToText for DateTime {
    fn to_text(&self, out: &mut Printbuf) {
        let days = self.0.div_euclid(86400);
        let secs = self.0.rem_euclid(86400);

        /* civil_from_days(), from Howard Hinnant's chrono-Compatible Low-Level Date Algorithms */
        let z = days + 719468;
        let era = z.div_euclid(146097);
        let doe = z.rem_euclid(146097);
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = yoe + era * 400 + i64::from(month <= 2);

        write!(out, "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}",
               secs / 3600, secs / 60 % 60, secs % 60);
    }
}

/// A number with an optional unit suffix - 4k, 1.5M, 2 GiB, 10MB: k M G ...
/// and KiB MiB ... are powers of 1024, kB MB ... powers of 1000. One trailing
/// newline, as sysfs stores have.
pub fn parse_human_u64(s: &str) -> Result<u64, BchError> {
    let einval = || BchError::from(c::EINVAL);
    let erange = || BchError::from(c::ERANGE);

    let digits = |s: &[u8]| -> Result<(u64, usize), BchError> {
        let n = s.iter().take_while(|c| c.is_ascii_digit()).count();
        if n == 0 {
            return Err(einval());
        }
        let v = s[..n].iter()
            .try_fold(0u64, |v, &c| v.checked_mul(10)?.checked_add(u64::from(c - b'0')))
            .ok_or_else(erange)?;
        Ok((v, n))
    };

    let pow = |n: u64, p: usize| u32::try_from(p).ok().and_then(|p| n.checked_pow(p)).ok_or_else(erange);

    /* the multiplier, and how much of @s it took - one space before it allowed */
    let unit = |s: &[u8]| -> Result<(u64, usize), BchError> {
        let adv = usize::from(s.first() == Some(&b' '));
        let t = &s[adv..];

        /* before the single letters, or the M of MiB and MB is all that's read */
        for (base, units) in [(1024, ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB", "ZiB", "YiB"]),
                              (1000, ["B", "kB",  "MB",  "GB",  "TB",  "PB",  "EB",  "ZB",  "YB"])] {
            if let Some((u, name)) = units.iter().enumerate().find(|(_, n)| t.starts_with(n.as_bytes())) {
                return Ok((pow(base, u)?, adv + name.len()));
            }
        }
        if let Some(u) = t.first().and_then(|c| b"kMGTPEZY".iter().position(|u| u == c)) {
            return Ok((pow(1024, u + 1)?, adv + 1));
        }
        Ok((1, 0))
    };

    let s = s.as_bytes();
    let (negative, s) = match s.first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _          => (false, s),
    };

    let (v, n) = digits(s)?;
    let mut s = &s[n..];

    let (mut f_n, mut f_d) = (0u64, 1);
    if let Some(rest) = s.strip_prefix(b".") {
        let n;
        (f_n, n) = digits(rest)?;
        f_d = pow(10, n)?;
        s = &rest[n..];
    }

    let (b, n) = unit(s)?;
    let s = &s[n..];

    let v = v.checked_mul(b).ok_or_else(erange)?
        .checked_add(f_n.checked_mul(b).ok_or_else(erange)? / f_d).ok_or_else(erange)?;

    if !s.strip_prefix(b"\n").unwrap_or(s).is_empty() {
        return Err(einval());
    }
    if negative && v != 0 {
        return Err(erange());
    }
    Ok(v)
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
    /// The next write is pass_through()'s job: Printbuf::fmt_job.
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
/// indent and tabstops as they are; else via a printbuf of its own.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datetime() {
        for (t, s) in [(0,          "1970-01-01T00:00:00"),
                       (-1,         "1969-12-31T23:59:59"),
                       (951782400,  "2000-02-29T00:00:00"),
                       (4107542399, "2100-02-28T23:59:59"),
                       (1762209667, "2025-11-03T22:41:07")] {
            let mut out = Printbuf::new();
            DateTime(t).to_text(&mut out);
            assert_eq!(out.as_str(), s);
        }
    }

    #[test]
    fn strtou64_h() {
        for (s, ret, v) in [(c"4k", 0, 4096), (c"1.5M", 0, 1572864), (c"2KiB", 0, 2048),
                            (c"2MiB", 0, 2097152), (c"10MB", 0, 10000000), (c"1kB", 0, 1000),
                            (c"1.5 GB", 0, 1500000000), (c"1kiB", -22, 0), (c"3B", 0, 3),
                            (c"2 G", 0, 2147483648), (c"1\n", 0, 1), (c"-0", 0, 0), (c"-1", -34, 0),
                            (c"+7", 0, 7), (c"", -22, 0), (c"1.", -22, 0), (c"1.5", 0, 1),
                            (c"16E", -34, 0), (c"15.9E", 0, 18331451923248866918), (c"1x", -22, 0),
                            (c"1 ", -22, 0), (c"18446744073709551615", 0, u64::MAX),
                            (c"18446744073709551616", -34, 0)] {
            let r = parse_human_u64(s.to_str().unwrap()).map_err(|e| -e.raw());
            assert_eq!(r, if ret == 0 { Ok(v) } else { Err(ret) }, "{s:?}");
        }
    }
}
