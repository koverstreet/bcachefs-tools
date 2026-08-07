// SPDX-License-Identifier: GPL-2.0
//! Userspace (cargo) build script.
//!
//! Generates the bcachefs bindings and compiles the static-inline C wrappers
//! bindgen emits. The codegen itself is shared with the kernel/DKMS build — see
//! `codegen.rs`, which both this and `codegen_main.rs` `include!`. Here we just
//! supply the userspace clang args, run the codegen, and cc + link the wrappers;
//! the kernel build supplies Kbuild's clang args and compiles `extern.c` as a
//! normal C object instead.

include!("codegen.rs");

fn watch_dir(dir: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        // file_type() reads the dirent directly - unlike Path::is_dir() it does
        // NOT follow symlinks. Following them descends ktest-out/kernel into a
        // full kernel tree and ktest-out/vm into /tmp; over virtiofs in the test
        // VM that stat-storm makes the build crawl. We walk "..", so this is
        // reachable whenever a build has been run in-tree.
        let Ok(file_type) = entry.file_type() else { continue };
        let path = entry.path();
        if file_type.is_dir() {
            // Build output and VCS dirs hold no wrapper-included C/H, only huge
            // file counts - don't recurse into them.
            if matches!(entry.file_name().to_str(), Some("target" | "ktest-out" | ".git")) {
                continue;
            }
            watch_dir(&path.to_string_lossy());
        } else if path.extension().is_some_and(|e| e == "h" || e == "c") {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}

fn main() {
    // `kernel` cfg selects kernel vs. bcachefs-shim types in mod.rs; cargo never
    // sets it, so declare it to avoid the unexpected-cfg warning.
    println!("cargo::rustc-check-cfg=cfg(kernel)");
    // The shared codegen logic is include!d, not a tracked source file — tell
    // cargo to rerun us when it changes.
    println!("cargo:rerun-if-changed=codegen.rs");
    // Rerun when any C/H file the wrapper might include changes.
    for dir in ["..", "../include"] {
        watch_dir(dir);
    }

    let out = std::env::var("OUT_DIR").expect("OUT_DIR");
    let src = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"); // = fs/
    let target = std::env::var("TARGET").expect("TARGET");
    let ptr_width = std::env::var("CARGO_CFG_TARGET_POINTER_WIDTH").unwrap_or_default();

    let clang_args = userspace_clang_args(&src, &target);
    let blocklist = default_blocklist(&src);

    run_bindgen(&out, &clang_args, &blocklist, &ptr_width);
    gen_xmacros(&src, &out);

    // Compile the static-inline wrappers bindgen just emitted and link them in,
    // with the same -I/-D set the headers were parsed with. cc-rs targets the
    // host compiler itself, so drop clang's `--target=` (gcc rejects it).
    let mut w = cc::Build::new();
    w.file(format!("{out}/extern.c")).warnings(false);
    for f in clang_args.iter().filter(|f| !f.starts_with("--target")) {
        w.flag(f);
    }
    w.compile("bcachefs_static_wrappers");

    // dh-cargo Built-Using (Debian): point the path at the package root (the
    // workspace root, == the dpkg build's $PWD) so dh-cargo-built-using sees
    // this lib as built from our own in-tree source and skips it, rather than
    // aborting on a build path no Debian package owns. Mirrors the same
    // declaration in bch_bindgen/build.rs; `src` is fs/, its parent is the root.
    println!(
        "dh-cargo:deb-built-using=bcachefs_static_wrappers=0={}",
        std::path::Path::new(&src)
            .parent()
            .expect("fs crate has a parent dir")
            .display()
    );
}
