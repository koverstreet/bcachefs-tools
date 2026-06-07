// SPDX-License-Identifier: GPL-2.0
//! Standalone codegen entry point — the kernel/DKMS Makefile compiles and runs
//! this with a plain `rustc -O codegen_main.rs -o codegen` (zero dependencies),
//! then invokes the binary to emit the bindings before building `mod.o`.
//!
//! Userspace drives the same logic from `build.rs`; both `include!` `codegen.rs`
//! so the two builds produce byte-identical bindings.
//!
//! Usage:
//!   codegen --src <fs-dir> --out <out-dir> [--cflags "<clang args>"] \
//!           [--blocklist "<dir>:<dir>"] [--target <triple>] [--ptr-width 64|32]
//!
//! Kernel: Kbuild passes --cflags and --blocklist. Run standalone for debugging
//! and it computes the userspace set (target + liburcu + -I + -D) instead.

include!("codegen.rs");

fn opt(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let src = opt(&args, "--src").expect("--src <fs-dir> required");
    let out = opt(&args, "--out").expect("--out <out-dir> required");
    let target = opt(&args, "--target").unwrap_or_else(host_target);
    let ptr_width = opt(&args, "--ptr-width").unwrap_or_else(|| "64".to_string());

    let clang_args: Vec<String> = match opt(&args, "--cflags") {
        Some(c) => c.split_whitespace().map(String::from).collect(),
        None => userspace_clang_args(&src, &target),
    };
    let blocklist: Vec<String> = match opt(&args, "--blocklist") {
        Some(v) => v.split(':').map(|s| format!("{s}/.*")).collect(),
        None => default_blocklist(&src),
    };

    run_bindgen(&src, &out, &clang_args, &blocklist, &ptr_width);
    gen_xmacros(&src, &out);
}
