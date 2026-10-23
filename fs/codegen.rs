// SPDX-License-Identifier: GPL-2.0
// The kernel build's bindgen: what fs/'s Rust uses of the kernel that
// kernel::bindings doesn't bind - the kernel-side counterpart of bcachefs-shim.
// Never bcachefs's own headers: they include the *_gen.h the Rust build
// generates, so binding them would make it depend on its own output. What's
// bound is kernel headers, carved out of the include trees' blocklist one by
// one, and vendored library code (the closures).
//
// codegen_main.rs, which the kernel/DKMS Makefile builds and runs, include!s
// this file - so plain // comments. It drives the bindgen CLI: no crates to
// vendor, no target/ in the kernel build tree.

use std::process::Command;

/// The kernel headers bound: each must be in UNBLOCKLISTED too. The CRCs are
/// static inlines in 6.16, the oldest kernel the module builds against, so
/// they're wrapped (extern.c); from 6.17, exported functions.
const KERNEL_HEADERS: &[&str] = &[
    "linux/crc32.h",			// crc32c(), a string hash
    "linux/crc64.h",			// crc64_be(), a string hash
    "linux/generic-radix-tree.h",	// GenRadix<T>'s __genradix
    "linux/xattr.h",			// xattr_handler, which fs.h only declares
    "linux/posix_acl_xattr.h",		// nop_posix_acl_*, xattr.rs's ACL handlers
    "crypto/sha2.h",			// sha256(), siphash_old's string hash key
    "linux/anon_inodes.h",		// anon_inode_getfile(), util/file's NewFile
    "linux/mm.h",			// totalram_pages(), a backpointer scan's batch limit
];

/// Headers under fs/ bound, by path - which the clang args have as an -I:
/// vendored library code, and codegen.h's wrappers of kernel macros.
const VENDORED_HEADERS: &[&str] = &["vendor/closure.h", "codegen.h"];

/// Under the kernel include trees, what isn't blocklisted: uapi/ (see
/// blocklist_dir()), and KERNEL_HEADERS.
const UNBLOCKLISTED: &[&str] = &[
    "uapi/",
    "linux/crc32.h",
    "linux/crc64.h",
    "linux/generic-radix-tree.h",
    "linux/xattr.h",
    "linux/posix_acl_xattr.h",
    "crypto/sha2.h",
    "linux/anon_inodes.h",
    "linux/mm.h",
];

const ALLOWLIST_FUNCTION: &[&str] = &[
    "crc32c", "crc64_be", "sha256", "anon_inode_getfile",
    "__genradix_ptr", "__genradix_ptr_alloc", "__genradix_iter_peek", "__genradix_free",
    "closure_get_not_zero", "closure_put",
    "rust_page_folio", "rust_folio_page_idx", "rust_folio_size",
    "rust_page_highmem", "rust_page_address",
    "rust_refcount_inc_not_zero", "rust_poll_wait", "rust_fault_in_writeable",
    "rust_fault_in_readable", "totalram_pages",
];
const ALLOWLIST_TYPE: &[&str] = &["closure", "closure_waitlist", "__genradix", "genradix_iter",
                                  "xattr_handler"];
const ALLOWLIST_VAR: &[&str] = &["GENRADIX_NODE_SIZE", "nop_posix_acl_access", "nop_posix_acl_default",
                                 "RUST_CONST_HELPER_.*"];

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
    // Zeroable when every field is, as the kernel's own bindgen_parameters
    // and the shim's have it: a struct holding these - bch_fs, holding
    // closures - can start zeroed in a Rust initializer
    flag!("--with-derive-custom-struct", ".*=::pin_init::MaybeZeroable");
    flag!("--with-derive-custom-union", ".*=::pin_init::MaybeZeroable");
    a.push("--use-core".into());
    // C's char as the kernel compiles it, u8, as kernel::bindings has it -
    // not core's, i8 on x86: a fn pointer field taking char * has to take
    // what the Rust fns it points to do
    flag!("--ctypes-prefix", "crate::util::ffi");
    a.push("--generate-inline-functions".into());
    a.push("--wrap-static-fns".into());
    flag!("--wrap-static-fns-path", &extern_c);

    for x in ALLOWLIST_FUNCTION { flag!("--allowlist-function", *x); }
    for x in ALLOWLIST_TYPE     { flag!("--allowlist-type", *x); }
    for x in ALLOWLIST_VAR      { flag!("--allowlist-var", *x); }
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

    // codegen.h's consts, by the names of the macros they're of
    let mut bindings = String::from_utf8(result.stdout).expect("bindgen output utf8")
        .replace("pub const RUST_CONST_HELPER_", "pub const ");
    bindings += &opaque_arrays_zeroable(&bindings);
    std::fs::write(format!("{out}/bindings.rs"), bindings).expect("write bindings.rs");

    // None, or one with the wrapper's path baked into its #include: the bare
    // name, so the C compile finds it next to extern.c.
    let fixed = std::fs::read_to_string(&extern_c).unwrap_or_default()
        .replace(&format!("\"{wrapper}\""), "\"codegen-wrapper.h\"");
    std::fs::write(&extern_c, fixed).expect("write extern.c");
}

/// bindgen 0.73 stores an opaque blob - the storage of a union it can't make a
/// Rust union, the closure's - as __BindgenOpaqueArrayN<T>, aligned as the C,
/// where 0.72 had a [u64; N]. It's bindgen's own helper, so --with-derive-custom
/// doesn't reach it, and without Zeroable neither is a closure, nor bch_fs
/// holding one. The impls, for the ones this bindgen defined: 0.72 defines
/// none, and an impl of a type that isn't there doesn't compile. The union's
/// members are __BindgenUnionField, made Zeroable in mod.rs.
fn opaque_arrays_zeroable(bindings: &str) -> String {
    bindings.lines()
        .filter_map(|l| l.strip_prefix("pub struct __BindgenOpaqueArray"))
        .filter_map(|rest| rest.split('<').next())
        .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        .map(|n| format!("\n// SAFETY: repr(C, align({n})) over a T: zeroed bytes are a zeroed T\n\
                          unsafe impl<T: ::pin_init::Zeroable> ::pin_init::Zeroable \
                          for __BindgenOpaqueArray{n}<T> {{}}\n"))
        .collect()
}

/// Escape regex metacharacters in a path, for --blocklist-file: unescaped, the
/// + in linux-headers-7.0.13+deb14-common quantifies, the match silently fails,
/// and kernel types leak into the bindings - a second struct mutex.
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

/// A --blocklist-file regex for everything under kernel include tree @dir but
/// UNBLOCKLISTED. The trees hold the kernel's own structs, which
/// kernel::bindings binds: a second copy would be a different type. uapi/ stays
/// visible: bindgen can't see across a blocklist that a pruned scalar typedef
/// is Copy, and structs holding one would lose Copy/Clone/Debug.
///
/// Path regexes because bindgen has nothing structural, no notion of a system
/// header. And --allowlist-file can't replace them: it makes every item in a
/// file a root, so --wrap-static-fns wraps every static inline in the tree, and
/// those using nested structs don't compile in extern.c.
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
