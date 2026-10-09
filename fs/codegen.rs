// SPDX-License-Identifier: GPL-2.0
// The kernel build's bindgen: what fs/'s Rust uses of the kernel that
// kernel::bindings doesn't bind - the kernel-side counterpart of
// bcachefs-shim, which binds the same for userspace from include/.
//
// Never bcachefs's own code. Its C interface is Rust's (types/lib.rs), and its
// headers include the ones the Rust build generates (*_gen.h), so binding
// them would make the Rust build depend on its own output. What's bound is
// kernel headers, carved out of the kernel include trees' blocklist one by
// one, and vendored library code: the closures.
//
// codegen_main.rs is the tool the kernel/DKMS Makefile builds and runs; it
// include!s this file. It drives the bindgen CLI rather than the library - no
// crates to vendor, no target/ litter in the kernel build tree. Plain //
// comments: include!d, never a crate root.

use std::process::Command;

/// The kernel headers bound: each must be in UNBLOCKLISTED too. The CRCs are
/// static inlines in 6.16, the oldest kernel the module builds against, so
/// they're wrapped (extern.c); from 6.17, exported functions.
const KERNEL_HEADERS: &[&str] = &[
    "linux/crc32.h",			// crc32c(), a string hash
    "linux/crc64.h",			// crc64_be(), a string hash
    "linux/generic-radix-tree.h",	// GenRadix<T>'s __genradix
];

/// Vendored library code bound, by path under fs/ - which the clang args have
/// as an -I.
const VENDORED_HEADERS: &[&str] = &["vendor/closure.h"];

/// Under the kernel include trees, what isn't blocklisted: uapi/ (see
/// blocklist_dir()), and KERNEL_HEADERS.
const UNBLOCKLISTED: &[&str] = &[
    "uapi/",
    "linux/crc32.h",
    "linux/crc64.h",
    "linux/generic-radix-tree.h",
];

const ALLOWLIST_FUNCTION: &[&str] = &["crc32c", "crc64_be"];
const ALLOWLIST_TYPE: &[&str] = &["closure", "closure_waitlist", "__genradix"];

/// Run the bindgen CLI and write `bindings.rs`, and `extern.c` - C wrappers for
/// whatever of it is a static inline - into `out`. Kbuild supplies
/// `clang_args`, the C build's own, and `blocklist_dirs`, the kernel include
/// trees.
pub fn run_bindgen(out: &str, clang_args: &[String], blocklist_dirs: &[String]) {
    std::fs::create_dir_all(out).expect("create out dir");

    // bindgen CLI takes one header: a wrapper that #includes the set.
    let wrapper = format!("{out}/codegen-wrapper.h");
    let body: String = KERNEL_HEADERS.iter().map(|h| format!("#include <{h}>\n"))
        .chain(VENDORED_HEADERS.iter().map(|h| format!("#include \"{h}\"\n")))
        .collect();
    std::fs::write(&wrapper, body).expect("write wrapper header");

    // bindgen writes extern.c only when there's a static inline to wrap - which
    // depends on the kernel - so a previous build's goes first, or it's what
    // gets compiled.
    let extern_c = format!("{out}/extern.c");
    if let Err(e) = std::fs::remove_file(&extern_c) {
        if e.kind() != std::io::ErrorKind::NotFound {
            panic!("could not remove the previous build's {extern_c}: {e}");
        }
    }

    let mut a: Vec<String> = vec![wrapper.clone()];
    macro_rules! flag { ($f:expr, $v:expr) => {{ a.push($f.into()); a.push(String::from($v)); }}; }

    a.push("--formatter".into()); a.push("prettyplease".into());
    a.push("--with-derive-default".into());
    a.push("--use-core".into());
    a.push("--generate-inline-functions".into());
    a.push("--wrap-static-fns".into());
    flag!("--wrap-static-fns-path", &extern_c);

    for x in ALLOWLIST_FUNCTION { flag!("--allowlist-function", *x); }
    for x in ALLOWLIST_TYPE     { flag!("--allowlist-type", *x); }
    for d in blocklist_dirs     { flag!("--blocklist-file", d.as_str()); }

    a.push("--".into());
    a.extend(clang_args.iter().cloned());

    let bindgen = std::env::var_os("BINDGEN").unwrap_or_else(|| "bindgen".into());
    let result = Command::new(&bindgen).args(&a).output().unwrap_or_else(|e| {
        panic!("could not run {bindgen:?}: {e}\n\
                bindgen generates the Rust bindings to the kernel APIs bcachefs uses \
                that the kernel's own don't cover: install it with `cargo install \
                bindgen-cli`, or set $BINDGEN to its path")
    });
    if !result.status.success() {
        eprintln!("{}", String::from_utf8_lossy(&result.stderr));
        std::process::exit(1);
    }

    let bindings = String::from_utf8(result.stdout).expect("bindgen output utf8");
    std::fs::write(format!("{out}/bindings.rs"), bindings).expect("write bindings.rs");

    // None, or one with the wrapper's path baked into its #include: the bare
    // name, so the C compile finds it next to extern.c.
    let fixed = std::fs::read_to_string(&extern_c).unwrap_or_default()
        .replace(&format!("\"{wrapper}\""), "\"codegen-wrapper.h\"");
    std::fs::write(&extern_c, fixed).expect("write extern.c");
}

/// Escape regex metacharacters so a literal path can be used in a bindgen
/// `--blocklist-file` regex. Without this, a path like
/// `linux-headers-7.0.13+deb14-common` is interpreted as a regex - the `+`
/// quantifies the preceding `3` - and silently fails to match, so the kernel
/// headers under it never get blocklisted and their types leak into the
/// generated bindings (e.g. a second `struct mutex` distinct from the kernel's).
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\.+*?()|[]{}^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A `--blocklist-file` regex for everything under kernel include tree `dir`,
/// except what's in UNBLOCKLISTED.
///
/// The trees are blocklisted because they hold the kernel's own structs -
/// `inode`, `super_block`, `bio` - which `kernel::bindings` already binds: a
/// second copy would be a different type, and nothing would link. A blocklisted
/// type the bound ones use resolves to `kernel::bindings`'s instead, the
/// generated code being included where that's glob-imported.
///
/// `uapi/` stays visible: its scalar typedefs are transparent, and bindgen
/// can't see across a blocklist that a pruned type is Copy - a struct holding
/// one would lose its Copy/Clone/Debug.
///
/// Why path regexes and not something structural: bindgen has no notion of
/// "system header" (nothing keyed off -isystem), only file-path patterns. And
/// --allowlist-file can't replace these: allowlisting makes every item in a
/// matching file a *root*, so --wrap-static-fns then wraps every static inline
/// in the tree - which breaks on functions whose signatures use
/// structs-defined-within-structs (bindgen's mangled names for those aren't
/// real C types, so the generated extern.c doesn't compile). The allowlists
/// pick the roots; this prunes their dependencies back to kernel::bindings.
fn blocklist_dir(dir: &str) -> String {
    format!("{}/{}", regex_escape(dir), not_prefixed(UNBLOCKLISTED))
}

/// A regex for any string that doesn't start with one of @prefixes. bindgen
/// matches with the `regex` crate, which has no lookahead, so it's spelled out
/// a character at a time: a character no prefix continues with matches,
/// whatever follows; one that some do continues down those - unless it
/// completes one.
fn not_prefixed(prefixes: &[&str]) -> String {
    let mut next: Vec<char> = prefixes.iter().filter_map(|p| p.chars().next()).collect();
    next.sort();
    next.dedup();

    // '-' last in the class, where it's literal rather than a range
    let class: String = next.iter().filter(|&&c| c != '-').map(|c| regex_escape(&c.to_string()))
        .chain(next.contains(&'-').then(|| "-".to_string()))
        .collect();
    let mut alts = vec![format!("[^{class}].*")];
    for c in next {
        let rest: Vec<&str> = prefixes.iter().filter_map(|p| p.strip_prefix(c)).collect();
        if rest.iter().any(|r| r.is_empty()) {
            continue;
        }
        alts.push(format!("{}{}", regex_escape(&c.to_string()), not_prefixed(&rest)));
    }
    format!("(?:{})", alts.join("|"))
}
