// SPDX-License-Identifier: GPL-2.0
//! The codegen tool: the kernel/DKMS Makefile compiles and runs this with a
//! plain `rustc -O codegen_main.rs -o codegen` (zero dependencies), for the
//! bindings mod.o includes - see codegen.rs.
//!
//! Usage:
//!   codegen --out <out-dir> --blocklist "<dir>:<dir>" -- <clang args...>
//!
//! Kbuild passes the C build's flags after `--`, verbatim as separate args -
//! never wrapped in a quoted string, so embedded quotes like arm64's
//! -DARM64_ASM_ARCH='"..."' survive intact.

include!("codegen.rs");

fn opt(args: &[String], name: &str) -> String {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
        .unwrap_or_else(|| panic!("{name} required"))
}

fn main() {
    let all: Vec<String> = std::env::args().collect();

    // Codegen's own options come before the `--`, so a clang flag can't be
    // mistaken for one.
    let sep = all.iter().position(|a| a == "--").expect("-- <clang args> required");
    let args = &all[..sep];

    let out = opt(args, "--out");
    let blocklist: Vec<String> = opt(args, "--blocklist").split(':').map(blocklist_dir).collect();

    run_bindgen(&out, &all[sep + 1..], &blocklist);
}
