// SPDX-License-Identifier: GPL-2.0

//! rust_gc_link OUT WORKDIR OBJ...
//!
//! Link bcachefs's Rust objects into one, leaving out the code nothing reaches:
//! ld -r --gc-sections. kbuild links a module without section GC, and rustc
//! compiles every function of a crate that a dependent could call - all of
//! core's and the vendored crates' - so without this, all of it is in the
//! module, with whatever it references: float intrinsics the kernel doesn't
//! have, for one.
//!
//! Roots: what C can call - the defined globals that aren't v0-mangled (_R*),
//! which is the #[no_mangle] set - less kCFI's __cfi_<fn> preambles, one per
//! function and global with it: as roots they'd keep every function. Kept
//! whole: every allocated section that isn't code or data - metadata nothing
//! references, which the GC would drop silently: .modinfo, __param,
//! __bug_table, __jump_table, __ex_table, .altinstructions, and whatever else
//! turns up - and objtool's .discard.* annotations, which objtool reads from
//! the output. Never a SHF_LINK_ORDER section: the GC keeps each piece with
//! the section it describes, and kept whole, __patchable_function_entries
//! would be a root naming every function.
//!
//! ld.bfd keeps the undefined symbols of what it dropped, and modpost takes
//! those for symbols the module needs: they're stripped - only those no
//! relocation names, which objcopy enforces.
//!
//! Then defined symbols are renamed - an undefined one is vmlinux's name for
//! it - a function and its __cfi_ preamble together, as objtool pairs them:
//! - LLVM's hot/cold splitting outlines a cold path to foo.cold.N, a function
//!   entered by a call. objtool takes any foo.cold for a GCC cold subfunction,
//!   part of foo jumped into, so it calls all of it unreachable: never
//!   validated, no ORC to unwind it by. As foo.part.N - GCC's name for what it
//!   outlines to be called - it's the function it is.
//! - kallsyms returns -E2BIG for a name of KSYM_NAME_LEN bytes or more, which
//!   __sprint_symbol() takes as a length: every %pS of an address in it writes
//!   before its buffer. v0-mangled names run that long; they're cut short,
//!   with a hash of the whole added, unique and the same each build. DWARF
//!   keeps the long name, so what resolves by it (perf probe) misses them.
//!
//! Takes LD (with its flags), NM, OBJCOPY and READELF from the environment;
//! leaves gc.lds and the symbol lists objcopy took in WORKDIR.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::process::{exit, Command, Stdio};

const KSYM_NAME_LEN: usize = 512;
/// What's kept of a long name: with ".bch" and the hash, and __cfi_, under
/// KSYM_NAME_LEN.
const KEEP: usize = 480;
const CFI: &str = "__cfi_";

/// @cmd - a program and its flags, as one string, as kbuild's $(LD) - run
/// with @args: what it printed.
fn run(cmd: &str, args: &[&str]) -> Result<String, String> {
    let mut words = cmd.split_whitespace();
    let prog = words.next().ok_or_else(|| format!("no program in '{cmd}'"))?;
    let out = Command::new(prog).args(words).args(args).stderr(Stdio::inherit()).output()
        .map_err(|e| format!("running {prog}: {e}"))?;
    if !out.status.success() {
        return Err(format!("{cmd} {}: {}", args.join(" "), out.status));
    }
    String::from_utf8(out.stdout).map_err(|_| format!("{cmd} {}: output not UTF-8", args.join(" ")))
}

/// The last field of @out's lines of @nr fields: nm's names - "address type
/// name" defined, "type name" undefined.
fn names(out: &str, nr: usize) -> impl Iterator<Item = &str> + Clone {
    out.lines().filter_map(move |l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        (f.len() == nr).then(|| f[nr - 1])
    })
}

/// Of readelf -SW's section tables, the sections kept whole: allocated and not
/// code or data, or objtool's annotations - never a SHF_LINK_ORDER piece.
fn kept_whole(sections: &str) -> BTreeSet<&str> {
    sections.lines().filter_map(|l| {
        let (nr, row) = l.trim_start().strip_prefix('[')?.split_once(']')?;
        nr.trim().parse::<u32>().ok()?;
        // Name Type Address Off Size ES Flg Lk Inf Al: Flg is left out when empty
        let f: Vec<&str> = row.split_whitespace().collect();
        let (name, flags) = (*f.first()?, if f.len() == 10 { f[6] } else { "" });
        let code_or_data = ["text", "rodata", "data", "bss"].iter().any(|k| {
            name.strip_prefix('.').and_then(|n| n.strip_prefix(k)).is_some_and(|r| r.is_empty() || r.starts_with('.'))
        });
        (!flags.is_empty() && !flags.contains('L') &&
         ((flags.contains('A') && !code_or_data) || name.starts_with(".discard."))).then_some(name)
    }).collect()
}

/// Of readelf -rW's relocations, the names of the symbols they refer to.
fn relocated(relocs: &str) -> BTreeSet<&str> {
    relocs.lines().filter_map(|l| {
        // Offset Info Type Sym.Value Sym.Name [+ Addend]
        let f: Vec<&str> = l.split_whitespace().collect();
        (f.len() >= 5 && f[2].starts_with("R_")).then(|| f[4])
    }).collect()
}

fn linker_script(sections: &str, globals: &str) -> String {
    let roots: BTreeSet<&str> = names(globals, 3).filter(|n| !n.starts_with("_R") && !n.starts_with(CFI)).collect();

    let mut lds = String::from("SECTIONS {\n");
    for s in kept_whole(sections) {
        let _ = writeln!(lds, "\t{s} 0 : {{ KEEP(*({s})) }}");
    }
    lds.push_str("}\n");
    for r in roots {
        let _ = writeln!(lds, "EXTERN({r})");
    }
    lds
}

/// FNV-1a: stable across builds and hosts, as std's hasher isn't.
fn hash(s: &str) -> u64 {
    s.bytes().fold(0xcbf29ce484222325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3))
}

/// foo.cold.N as foo.part.N, if it's one.
fn uncold(f: &str) -> Option<String> {
    let (foo, n) = f.rsplit_once(".cold.")?;
    (!n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())).then(|| format!("{foo}.part.{n}"))
}

/// An objcopy --redefine-syms map, of the defined symbols @names.
fn renames<'a>(names: impl Iterator<Item = &'a str> + Clone) -> String {
    let unprefixed = |n: &'a str| n.strip_prefix(CFI).unwrap_or(n);
    let long: BTreeSet<&str> = names.clone().filter(|n| n.len() >= KSYM_NAME_LEN).map(unprefixed).collect();
    let cold = names.map(unprefixed).filter(|f| uncold(f).is_some());
    let fns: BTreeSet<&str> = long.iter().copied().chain(cold).collect();

    let mut map = String::new();
    for f in fns {
        let named = uncold(f).unwrap_or_else(|| f.to_owned());
        let new = match long.contains(f) {
            true => {
                let kept = named.char_indices().nth(KEEP).map_or(&*named, |(i, _)| &named[..i]);
                format!("{kept}.bch{:016x}", hash(f))
            }
            false => named,
        };
        let _ = writeln!(map, "{f} {new}\n{CFI}{f} {CFI}{new}");
    }
    map
}

fn write(path: &str, contents: &str) -> Result<(), String> {
    std::fs::write(path, contents).map_err(|e| format!("writing {path}: {e}"))
}

fn gc_link(out: &str, work: &str, objs: &[String]) -> Result<(), String> {
    let tool = |var: &str| std::env::var(var).map_err(|_| format!("{var} isn't set"));
    let (ld, nm, objcopy, readelf) = (tool("LD")?, tool("NM")?, tool("OBJCOPY")?, tool("READELF")?);
    let objs: Vec<&str> = objs.iter().map(String::as_str).collect();

    let sections = run(&readelf, &[&["-SW"], &objs[..]].concat())?;
    let globals = run(&nm, &[&["--defined-only", "--extern-only"], &objs[..]].concat())?;
    let lds = format!("{work}/gc.lds");
    write(&lds, &linker_script(&sections, &globals))?;
    run(&ld, &[&["-r", "--gc-sections", "-T", lds.as_str(), "-o", out], &objs[..]].concat())?;

    let (undefined, relocs) = (run(&nm, &["--undefined-only", out])?, run(&readelf, &["-rW", out])?);
    let relocated = relocated(&relocs);
    let unneeded: BTreeSet<&str> = names(&undefined, 2).filter(|n| !relocated.contains(n)).collect();
    if !unneeded.is_empty() {
        let list = format!("{work}/gc.unneeded");
        write(&list, &unneeded.iter().fold(String::new(), |l, n| l + n + "\n"))?;
        run(&objcopy, &[format!("--strip-symbols={list}").as_str(), out])?;
    }

    let defined = run(&nm, &["--defined-only", out])?;
    let map = renames(names(&defined, 3));
    if !map.is_empty() {
        let list = format!("{work}/gc.redefine");
        write(&list, &map)?;
        run(&objcopy, &[format!("--redefine-syms={list}").as_str(), out])?;
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [out, work, objs @ ..] = &args[..] else {
        eprintln!("usage: rust_gc_link OUT WORKDIR OBJ...");
        exit(2);
    };
    if let Err(e) = gc_link(out, work, objs) {
        eprintln!("rust_gc_link: {out}: {e}");
        exit(1);
    }
}
