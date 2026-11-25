// SPDX-License-Identifier: GPL-2.0

//! ksym_names: from `nm` output on stdin, an objcopy --redefine-syms map, on
//! stdout, renaming the symbols kallsyms and objtool can't take - or, with
//! --check, failing and naming any left.
//!
//! LLVM's hot/cold splitting outlines a function's cold path into a function
//! of its own, foo.cold.N, entered by a call. objtool takes any foo.cold for a
//! GCC cold subfunction - a part of foo, jumped into, in foo's frame - so it
//! finds nothing that jumps there, and calls all of it unreachable: never
//! validated, no ORC to unwind it by, on the panic paths. Or, foo inlined
//! everywhere and collected, it refuses foo.cold.N for lacking it. As
//! foo.part.N - GCC's name for what it outlines to be called - it's the
//! function it is.
//!
//! kallsyms' module_address_lookup() returns strscpy()'s -E2BIG for a name of
//! KSYM_NAME_LEN bytes or more, and __sprint_symbol() takes that as a length:
//! every %pS of an address in it - an oops', a WARN's - writes before its
//! stack buffer. v0-mangled Rust names run that long, spelling out every
//! generic argument. The path, the function's own name with it, comes first,
//! so the name is cut short - still v0's [A-Za-z0-9_] - and a hash of the
//! whole added, keeping it unique and the same from build to build.
//!
//! A function and its kCFI preamble, __cfi_<fn>, are renamed together,
//! whichever of the two is long: objtool pairs them by the prefix.
//!
//! Defined symbols only - an undefined one is another object's name for it,
//! vmlinux's for the kernel crate's. Section names stay long: kallsyms never
//! sees them. DWARF keeps the original name, so tools that resolve by it and
//! then look the symbol up (perf probe) miss a renamed one.

use std::collections::BTreeSet;
use std::io::{self, BufRead, Write};
use std::process::exit;

const KSYM_NAME_LEN: usize = 512;
/// What's kept of a long name: with ".bch" and the hash, and __cfi_, under
/// KSYM_NAME_LEN.
const KEEP: usize = 480;
const CFI: &str = "__cfi_";

/// FNV-1a: stable across builds and hosts, as std's hasher isn't.
fn hash(s: &str) -> u64 {
    s.bytes().fold(0xcbf29ce484222325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3))
}

/// foo.cold.N as foo.part.N, if it's one.
fn uncold(f: &str) -> Option<String> {
    let (foo, n) = f.rsplit_once(".cold.")?;
    (!n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())).then(|| format!("{foo}.part.{n}"))
}

/// Left in, both: an error, naming them.
fn check(names: &[String]) {
    let long: Vec<_> = names.iter().filter(|n| n.len() >= KSYM_NAME_LEN).collect();
    let cold: Vec<_> = names.iter().filter(|n| uncold(n).is_some()).collect();
    for n in long.iter().chain(&cold) {
        eprintln!("{n}");
    }
    if !long.is_empty() {
        eprintln!("{} symbols of KSYM_NAME_LEN ({KSYM_NAME_LEN}) bytes or more left, above: \
                   kallsyms would write before its buffer", long.len());
    }
    if !cold.is_empty() {
        eprintln!("{} of LLVM's .cold.N functions left, above: objtool would take them for GCC's \
                   cold subfunctions", cold.len());
    }
    if !long.is_empty() || !cold.is_empty() {
        exit(1);
    }
}

fn main() {
    let names: Vec<String> = io::stdin().lock().lines()
        .map(|l| l.expect("reading nm's output"))
        .filter_map(|l| l.split_whitespace().last().map(str::to_owned))
        .collect();

    if std::env::args().nth(1).as_deref() == Some("--check") {
        return check(&names);
    }

    let unprefixed = |n: &str| n.strip_prefix(CFI).unwrap_or(n).to_owned();
    let long: BTreeSet<String> = names.iter().filter(|n| n.len() >= KSYM_NAME_LEN).map(|n| unprefixed(n)).collect();
    let cold = names.iter().map(|n| unprefixed(n)).filter(|f| uncold(f).is_some());
    let fns: BTreeSet<String> = long.iter().cloned().chain(cold).collect();

    let mut out = io::stdout().lock();
    for f in &fns {
        let named = uncold(f).unwrap_or_else(|| f.clone());
        let new = match long.contains(f) {
            true => {
                let kept = named.char_indices().nth(KEEP).map_or(&*named, |(i, _)| &named[..i]);
                format!("{kept}.bch{:016x}", hash(f))
            }
            false => named,
        };
        writeln!(out, "{f} {new}\n{CFI}{f} {CFI}{new}").expect("writing the map");
    }
}
