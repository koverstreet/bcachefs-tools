// SPDX-License-Identifier: GPL-2.0

//! ksym_len: from `nm` output on stdin, an objcopy --redefine-syms map, on
//! stdout, renaming the symbols kallsyms can't take - or, with --check,
//! failing and naming any left.
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

fn main() {
    let check = std::env::args().nth(1).as_deref() == Some("--check");
    let names: Vec<String> = io::stdin().lock().lines()
        .map(|l| l.expect("reading nm's output"))
        .filter_map(|l| l.split_whitespace().last().map(str::to_owned))
        .collect();
    let long = names.iter().filter(|n| n.len() >= KSYM_NAME_LEN);

    if check {
        let left: Vec<_> = long.collect();
        if !left.is_empty() {
            for n in &left {
                eprintln!("{n}");
            }
            eprintln!("{} symbols of KSYM_NAME_LEN ({KSYM_NAME_LEN}) bytes or more left, above: \
                       kallsyms would write before its buffer", left.len());
            exit(1);
        }
        return;
    }

    let fns: BTreeSet<&str> = long.map(|n| n.strip_prefix(CFI).unwrap_or(n)).collect();
    let mut out = io::stdout().lock();
    for f in fns {
        let kept = f.char_indices().nth(KEEP).map_or(f, |(i, _)| &f[..i]);
        let new = format!("{kept}.bch{:016x}", hash(f));
        writeln!(out, "{f} {new}\n{CFI}{f} {CFI}{new}").expect("writing the map");
    }
}
