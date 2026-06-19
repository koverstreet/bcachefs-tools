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
        let path = entry.path();
        if path.is_dir() {
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
}
