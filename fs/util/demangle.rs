// SPDX-License-Identifier: GPL-2.0

//! Rust v0 symbol demangling, on the heap: rustc_demangle's v0 printer, as a
//! loop over a stack of steps in a KVVec instead of recursion.
//!
//! Backtraces are printed from wherever lock debugging fires - the bottom of
//! a deep write path, say - and rustc_demangle's recursive descent takes
//! kilobytes of stack for a name with nested generics and closures: enough
//! to overflow a 16K kernel stack. Here the depth is the step stack's, so a
//! name costs the stack one frame, and the heap a few hundred bytes.
//!
//! The output is rustc_demangle's {:#}: no crate disambiguators, no
//! constants' type suffixes. A step that can't be done - the step stack
//! can't grow, the name's malformed, or it uses what isn't handled here
//! (punycode identifiers, const values past integers, bools and chars,
//! pattern types) - and the name is printed as it came: what's demangled is
//! right, or it's the raw symbol.
//!
//! How v0 reads (the Rust symbol mangling RFC): a symbol is _R, a path, an
//! optional instantiating crate, and an optional .suffix. Paths, types and
//! consts nest; each step parses the construct at the cursor, prints its
//! prefix, and pushes what follows it - the nested construct, then whatever
//! prints after that - in reverse, so they pop in order. A backref (B<n>) is
//! the construct at offset n, printed by moving the cursor there, with a
//! step to move it back. An impl's own path is parsed and not printed: the
//! steps between Skip and Unskip print nothing, and don't follow backrefs.

use crate::util::alloc::flags::GFP_NOWAIT;
use crate::util::alloc::{AllocError, KVVec};
use crate::util::Printbuf;
use core::fmt::Write;

/// The most a name is demangled to: a backref can repeat what it points at,
/// so output can be exponential in the input.
const MAX_OUT: usize = 16 << 10;

/// The most steps pending: a page's worth, so the KVVec is kmalloc()'d -
/// kvmalloc() of a page or less never falls back to vmalloc - and freed by
/// kfree(), which a caller under a spinlock can do, where vfree() can't.
const MAX_STEPS: usize = 4096 / core::mem::size_of::<Step>();

/// Why a name isn't demangled: it's printed raw.
struct Raw;

impl From<AllocError> for Raw {
    fn from(_: AllocError) -> Self {
        Raw
    }
}

impl From<core::fmt::Error> for Raw {
    fn from(_: core::fmt::Error) -> Self {
        Raw
    }
}

#[derive(Clone, Copy)]
enum Step {
    Path { in_value: bool },
    /// A nested path's last segment: namespace @ns, after its parent
    PathNested { ns: Option<u8> },
    Type,
    GenericArg,
    Const { in_value: bool },
    /// The elements of a list up to its E, @sep between them: @tuple, a
    /// tuple's, gets a trailing comma if it has one element
    List { elem: Elem, sep: &'static str, n: u32, tuple: bool },
    Lit(&'static str),
    /// A fn type's return type, after its arguments
    FnRet,
    /// The end of a binder of @n lifetimes
    BinderEnd(u32),
    /// A dyn type's lifetime, after its traits
    DynLifetime,
    /// A trait of a dyn type, through backrefs: when the path's been
    /// printed, the cursor goes back to @ret
    DynTrait { ret: Option<usize> },
    /// A dyn trait's associated type bindings, inside its generics' <...> if
    /// @open
    DynAssoc { open: bool },
    /// Back from a backref
    Goto(usize),
    Skip,
    Unskip,
}

#[derive(Clone, Copy)]
enum Elem {
    Type,
    GenericArg,
    DynTrait,
}

struct Demangler<'s, 'o> {
    sym:   &'s [u8],
    at:    usize,
    out:   &'o mut Printbuf,
    start: usize,
    /// Printing suppressed: inside an impl's own path
    skip:  u32,
    /// Lifetimes bound by for<...> binders around what's being printed
    bound: u32,
    steps: KVVec<Step>,
}

impl<'s> Demangler<'s, '_> {
    fn push(&mut self, s: Step) -> Result<(), Raw> {
        if self.steps.len() == MAX_STEPS {
            return Err(Raw);
        }
        Ok(self.steps.push(s, GFP_NOWAIT)?)
    }

    fn print(&mut self, s: &str) -> Result<(), Raw> {
        if self.skip == 0 {
            self.out.write_str(s)?;
        }
        Ok(())
    }

    fn print_u64(&mut self, v: u64) -> Result<(), Raw> {
        if self.skip == 0 {
            write!(self.out, "{v}");
        }
        Ok(())
    }

    fn peek(&self) -> Option<u8> {
        self.sym.get(self.at).copied()
    }

    fn eat(&mut self, b: u8) -> bool {
        let yes = self.peek() == Some(b);
        self.at += usize::from(yes);
        yes
    }

    fn next(&mut self) -> Result<u8, Raw> {
        let b = self.peek().ok_or(Raw)?;
        self.at += 1;
        Ok(b)
    }

    fn digit_10(&mut self) -> Option<u8> {
        let d = self.peek().filter(u8::is_ascii_digit)? - b'0';
        self.at += 1;
        Some(d)
    }

    fn integer_62(&mut self) -> Result<u64, Raw> {
        if self.eat(b'_') {
            return Ok(0);
        }
        let mut x: u64 = 0;
        while !self.eat(b'_') {
            let d = match self.next()? {
                d @ b'0'..=b'9' => d - b'0',
                d @ b'a'..=b'z' => 10 + d - b'a',
                d @ b'A'..=b'Z' => 36 + d - b'A',
                _ => return Err(Raw),
            };
            x = x.checked_mul(62).and_then(|x| x.checked_add(d.into())).ok_or(Raw)?;
        }
        x.checked_add(1).ok_or(Raw)
    }

    fn opt_integer_62(&mut self, tag: u8) -> Result<u64, Raw> {
        if !self.eat(tag) {
            return Ok(0);
        }
        self.integer_62()?.checked_add(1).ok_or(Raw)
    }

    fn disambiguator(&mut self) -> Result<u64, Raw> {
        self.opt_integer_62(b's')
    }

    /// An identifier, ASCII: punycode's not handled.
    fn ident(&mut self) -> Result<&'s str, Raw> {
        if self.peek() == Some(b'u') {
            return Err(Raw);
        }
        let mut len = usize::from(self.digit_10().ok_or(Raw)?);
        if len != 0 {
            while let Some(d) = self.digit_10() {
                len = len.checked_mul(10).and_then(|l| l.checked_add(d.into())).ok_or(Raw)?;
            }
        }
        self.eat(b'_');

        let (sym, start) = (self.sym, self.at);
        self.at = start.checked_add(len).filter(|&end| end <= sym.len()).ok_or(Raw)?;
        core::str::from_utf8(&sym[start..self.at]).map_err(|_| Raw)
    }

    /// A backref's target, after its B: before the B, as backrefs point
    /// backwards.
    fn backref(&mut self) -> Result<usize, Raw> {
        let b = self.at - 1;
        let i = self.integer_62()?;
        usize::try_from(i).ok().filter(|&i| i < b).ok_or(Raw)
    }

    /// The construct at a backref, by @step there - or, skipping, nothing.
    fn follow(&mut self, step: Step) -> Result<(), Raw> {
        let target = self.backref()?;
        if self.skip == 0 {
            self.push(Step::Goto(self.at))?;
            self.push(step)?;
            self.at = target;
        }
        Ok(())
    }

    fn hex(&mut self) -> Result<&'s [u8], Raw> {
        let (sym, start) = (self.sym, self.at);
        loop {
            match self.next()? {
                b'0'..=b'9' | b'a'..=b'f' => {}
                b'_' => return Ok(&sym[start..self.at - 1]),
                _ => return Err(Raw),
            }
        }
    }

    fn hex_u64(nibbles: &[u8]) -> Option<u64> {
        let first = nibbles.iter().position(|&b| b != b'0').unwrap_or(nibbles.len());
        let nibbles = &nibbles[first..];
        (nibbles.len() <= 16).then(|| {
            nibbles.iter().fold(0, |v, &b| v << 4 | u64::from((b as char).to_digit(16).unwrap()))
        })
    }

    fn lifetime(&mut self, lt: u64) -> Result<(), Raw> {
        if self.skip != 0 {
            return Ok(());
        }
        self.print("'")?;
        if lt == 0 {
            return self.print("_");
        }
        let depth = u64::from(self.bound).checked_sub(lt).ok_or(Raw)?;
        if depth < 26 {
            let c = [b'a' + depth as u8];
            self.print(core::str::from_utf8(&c).unwrap())
        } else {
            self.print("_")?;
            self.print_u64(depth)
        }
    }

    /// A binder's lifetimes, for<'a, ...>, if it has any: how many.
    fn binder(&mut self) -> Result<u32, Raw> {
        let n = self.opt_integer_62(b'G')?;
        if self.skip != 0 {
            return Ok(0);
        }
        let n = u32::try_from(n).map_err(|_| Raw)?;
        if n > 0 {
            self.print("for<")?;
            for i in 0..n {
                if i > 0 {
                    self.print(", ")?;
                }
                self.bound += 1;
                self.lifetime(1)?;
            }
            self.print("> ")?;
        }
        Ok(n)
    }

    /// @s, and everything it pushes.
    fn run(&mut self, s: Step) -> Result<(), Raw> {
        self.push(s)?;
        while let Some(s) = self.steps.pop() {
            self.step(s)?;
            if self.out.as_bytes().len() - self.start > MAX_OUT {
                return Err(Raw);
            }
        }
        Ok(())
    }

    fn step(&mut self, s: Step) -> Result<(), Raw> {
        match s {
            Step::Path { in_value } => self.path(in_value)?,
            Step::PathNested { ns } => {
                let dis = self.disambiguator()?;
                let name = self.ident()?;
                let empty = name.is_empty();
                match ns {
                    Some(ns) => {
                        self.print("::{")?;
                        match ns {
                            b'C' => self.print("closure")?,
                            b'S' => self.print("shim")?,
                            _ => self.print(core::str::from_utf8(&[ns]).unwrap())?,
                        }
                        if !empty {
                            self.print(":")?;
                            self.print(name)?;
                        }
                        self.print("#")?;
                        self.print_u64(dis)?;
                        self.print("}")?;
                    }
                    None if !empty => {
                        self.print("::")?;
                        self.print(name)?;
                    }
                    None => {}
                }
            }
            Step::Type => self.ty()?,
            Step::GenericArg => {
                if self.eat(b'L') {
                    let lt = self.integer_62()?;
                    self.lifetime(lt)?;
                } else if self.eat(b'K') {
                    self.konst(false)?;
                } else {
                    self.ty()?;
                }
            }
            Step::Const { in_value } => self.konst(in_value)?,
            Step::List { elem, sep, n, tuple } => {
                if self.eat(b'E') {
                    if tuple && n == 1 {
                        self.print(",")?;
                    }
                } else {
                    if n > 0 {
                        self.print(sep)?;
                    }
                    self.push(Step::List { elem, sep, n: n + 1, tuple })?;
                    self.push(match elem {
                        Elem::Type       => Step::Type,
                        Elem::GenericArg => Step::GenericArg,
                        Elem::DynTrait   => Step::DynTrait { ret: None },
                    })?;
                }
            }
            Step::Lit(s) => self.print(s)?,
            Step::FnRet => {
                if !self.eat(b'u') {
                    self.print(" -> ")?;
                    self.push(Step::Type)?;
                }
            }
            Step::BinderEnd(n) => self.bound -= n,
            Step::DynLifetime => {
                if !self.eat(b'L') {
                    return Err(Raw);
                }
                let lt = self.integer_62()?;
                if lt != 0 {
                    self.print(" + ")?;
                    self.lifetime(lt)?;
                }
            }
            Step::DynTrait { ret } => {
                if self.eat(b'B') {
                    let target = self.backref()?;
                    if self.skip == 0 {
                        // printed there, and the bindings read here after
                        let ret = ret.unwrap_or(self.at);
                        self.push(Step::DynTrait { ret: Some(ret) })?;
                        self.at = target;
                    } else {
                        self.push(Step::DynAssoc { open: false })?;
                    }
                } else {
                    let open = self.eat(b'I');
                    self.push(Step::DynAssoc { open })?;
                    if let Some(ret) = ret {
                        self.push(Step::Goto(ret))?;
                    }
                    if open {
                        self.push(Step::List { elem: Elem::GenericArg, sep: ", ", n: 0, tuple: false })?;
                        self.push(Step::Lit("<"))?;
                    }
                    self.push(Step::Path { in_value: false })?;
                }
            }
            Step::DynAssoc { open } => {
                if self.eat(b'p') {
                    self.print(if open { ", " } else { "<" })?;
                    let name = self.ident()?;
                    self.print(name)?;
                    self.print(" = ")?;
                    self.push(Step::DynAssoc { open: true })?;
                    let konst = self.eat(b'K');
                    self.push(if konst { Step::Const { in_value: false } } else { Step::Type })?;
                } else if open {
                    self.print(">")?;
                }
            }
            Step::Goto(at) => self.at = at,
            Step::Skip => self.skip += 1,
            Step::Unskip => self.skip -= 1,
        }
        Ok(())
    }

    fn path(&mut self, in_value: bool) -> Result<(), Raw> {
        match self.next()? {
            b'C' => {
                self.disambiguator()?;
                let name = self.ident()?;
                self.print(name)?;
            }
            b'N' => {
                let ns = match self.next()? {
                    ns @ b'A'..=b'Z' => Some(ns),
                    b'a'..=b'z' => None,
                    _ => return Err(Raw),
                };
                self.push(Step::PathNested { ns })?;
                self.push(Step::Path { in_value })?;
            }
            tag @ (b'M' | b'X' | b'Y') => {
                self.push(Step::Lit(">"))?;
                if tag != b'M' {
                    self.push(Step::Path { in_value: false })?;
                    self.push(Step::Lit(" as "))?;
                }
                self.push(Step::Type)?;
                self.push(Step::Lit("<"))?;
                if tag != b'Y' {
                    // the impl's own path, not printed
                    self.disambiguator()?;
                    self.push(Step::Unskip)?;
                    self.push(Step::Path { in_value: false })?;
                    self.push(Step::Skip)?;
                }
            }
            b'I' => {
                self.push(Step::Lit(">"))?;
                self.push(Step::List { elem: Elem::GenericArg, sep: ", ", n: 0, tuple: false })?;
                self.push(Step::Lit(if in_value { "::<" } else { "<" }))?;
                self.push(Step::Path { in_value })?;
            }
            b'B' => self.follow(Step::Path { in_value })?,
            _ => return Err(Raw),
        }
        Ok(())
    }

    fn ty(&mut self) -> Result<(), Raw> {
        let tag = self.next()?;
        if let Some(ty) = basic_type(tag) {
            return self.print(ty);
        }
        match tag {
            b'R' | b'Q' => {
                self.print("&")?;
                if self.eat(b'L') {
                    let lt = self.integer_62()?;
                    if lt != 0 {
                        self.lifetime(lt)?;
                        self.print(" ")?;
                    }
                }
                if tag == b'Q' {
                    self.print("mut ")?;
                }
                self.push(Step::Type)?;
            }
            b'P' | b'O' => {
                self.print(if tag == b'P' { "*const " } else { "*mut " })?;
                self.push(Step::Type)?;
            }
            b'A' | b'S' => {
                self.print("[")?;
                self.push(Step::Lit("]"))?;
                if tag == b'A' {
                    self.push(Step::Const { in_value: true })?;
                    self.push(Step::Lit("; "))?;
                }
                self.push(Step::Type)?;
            }
            b'T' => {
                self.print("(")?;
                self.push(Step::Lit(")"))?;
                self.push(Step::List { elem: Elem::Type, sep: ", ", n: 0, tuple: true })?;
            }
            b'F' => {
                let n = self.binder()?;
                let unsafe_ = self.eat(b'U');
                let abi = if self.eat(b'K') {
                    if self.eat(b'C') {
                        Some("C")
                    } else {
                        let abi = self.ident()?;
                        if abi.is_empty() {
                            return Err(Raw);
                        }
                        Some(abi)
                    }
                } else {
                    None
                };
                if unsafe_ {
                    self.print("unsafe ")?;
                }
                if let Some(abi) = abi {
                    // a - in the ABI is mangled as _
                    self.print("extern \"")?;
                    for (i, part) in abi.split('_').enumerate() {
                        if i > 0 {
                            self.print("-")?;
                        }
                        self.print(part)?;
                    }
                    self.print("\" ")?;
                }
                self.print("fn(")?;
                self.push(Step::BinderEnd(n))?;
                self.push(Step::FnRet)?;
                self.push(Step::Lit(")"))?;
                self.push(Step::List { elem: Elem::Type, sep: ", ", n: 0, tuple: false })?;
            }
            b'D' => {
                self.print("dyn ")?;
                let n = self.binder()?;
                self.push(Step::DynLifetime)?;
                self.push(Step::BinderEnd(n))?;
                self.push(Step::List { elem: Elem::DynTrait, sep: " + ", n: 0, tuple: false })?;
            }
            b'B' => self.follow(Step::Type)?,
            _ => {
                // a path: from its tag
                self.at -= 1;
                self.path(false)?;
            }
        }
        Ok(())
    }

    /// A const: of integers, bools and chars - the rest aren't handled.
    fn konst(&mut self, in_value: bool) -> Result<(), Raw> {
        let tag = self.next()?;
        match tag {
            b'p' => self.print("_")?,
            b'h' | b't' | b'm' | b'y' | b'o' | b'j' | b'a' | b's' | b'l' | b'x' | b'n' | b'i' => {
                if matches!(tag, b'a' | b's' | b'l' | b'x' | b'n' | b'i') && self.eat(b'n') {
                    self.print("-")?;
                }
                let hex = self.hex()?;
                match Self::hex_u64(hex) {
                    Some(v) => self.print_u64(v)?,
                    None => {
                        self.print("0x")?;
                        self.print(core::str::from_utf8(hex).unwrap())?;
                    }
                }
            }
            b'b' => match Self::hex_u64(self.hex()?) {
                Some(0) => self.print("false")?,
                Some(1) => self.print("true")?,
                _ => return Err(Raw),
            },
            b'c' => {
                let c = Self::hex_u64(self.hex()?)
                    .and_then(|v| u32::try_from(v).ok())
                    .and_then(char::from_u32)
                    .ok_or(Raw)?;
                if self.skip == 0 {
                    self.out.write_char('\'')?;
                    if c == '"' {
                        self.out.write_char(c)?;
                    } else {
                        for e in c.escape_debug() {
                            self.out.write_char(e)?;
                        }
                    }
                    self.out.write_char('\'')?;
                }
            }
            b'B' => self.follow(Step::Const { in_value })?,
            _ => return Err(Raw),
        }
        Ok(())
    }
}

fn basic_type(tag: u8) -> Option<&'static str> {
    Some(match tag {
        b'b' => "bool",
        b'c' => "char",
        b'e' => "str",
        b'u' => "()",
        b'a' => "i8",
        b's' => "i16",
        b'l' => "i32",
        b'x' => "i64",
        b'n' => "i128",
        b'i' => "isize",
        b'h' => "u8",
        b't' => "u16",
        b'm' => "u32",
        b'y' => "u64",
        b'o' => "u128",
        b'j' => "usize",
        b'f' => "f32",
        b'd' => "f64",
        b'z' => "!",
        b'p' => "_",
        b'v' => "...",
        _ => return None,
    })
}

/// A suffix rustc_demangle keeps: LLVM's period-delimited words.
fn symbol_like(s: &[u8]) -> bool {
    s.iter().all(|&c| c.is_ascii_alphanumeric() || c.is_ascii_punctuation())
}

/// @sym, a v0 symbol - _R... - demangled to @out: Raw if it can't be, with
/// what's been written for the caller to take back.
fn v0(out: &mut Printbuf, sym: &str) -> Result<(), Raw> {
    let inner = sym.strip_prefix("_R").filter(|s| !s.is_empty()).ok_or(Raw)?.as_bytes();
    if !inner[0].is_ascii_uppercase() || !inner.is_ascii() {
        return Err(Raw);
    }

    let mut d = Demangler {
        sym:   inner,
        at:    0,
        start: out.as_bytes().len(),
        out,
        skip:  0,
        bound: 0,
        steps: KVVec::new(),
    };
    d.steps.reserve(32, GFP_NOWAIT)?;

    d.run(Step::Path { in_value: true })?;

    // the instantiating crate, not printed
    if d.peek().is_some_and(|b| b.is_ascii_uppercase()) {
        d.skip += 1;
        d.run(Step::Path { in_value: false })?;
        d.skip -= 1;
    }

    let suffix = &inner[d.at..];
    if !suffix.is_empty() {
        if suffix[0] != b'.' || !symbol_like(suffix) {
            return Err(Raw);
        }
        d.out.write_str(core::str::from_utf8(suffix).unwrap())?;
    }
    Ok(())
}

/// A legacy name, _ZN...: std's, prebuilt, in userspace - rustc_demangle's
/// legacy demangling doesn't recurse.
#[cfg(not(kernel))]
fn prt_other(out: &mut Printbuf, sym: &str) {
    write!(out, "{:#}", rustc_demangle::demangle(sym));
}

/// In the kernel every Rust name is v0: anything else is C's.
#[cfg(kernel)]
fn prt_other(out: &mut Printbuf, sym: &str) {
    let _ = out.write_str(sym);
}

/// @sym demangled to @out, as rustc_demangle's {:#}: a v0 name here, on the
/// heap - as it is, if it can't be - and anything else by prt_other().
pub fn prt_demangled(out: &mut Printbuf, sym: &str) {
    // ThinLTO's renaming of internal symbols, which rustc_demangle drops
    let sym = match sym.find(".llvm.") {
        Some(i) if sym[i + 6..].bytes().all(|c| matches!(c, b'A'..=b'F' | b'0'..=b'9' | b'@')) => &sym[..i],
        _ => sym,
    };

    if sym.starts_with("_R") {
        let saved = out.state_save();
        if v0(out, sym).is_err() {
            out.state_restore(saved);
            let _ = out.write_str(sym);
        }
    } else {
        prt_other(out, sym);
    }
}
