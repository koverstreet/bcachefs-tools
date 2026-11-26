// SPDX-License-Identifier: GPL-2.0
//! Userspace (cargo) build script: the build configuration's cfgs. The fs
//! crate's C interface - its types, and the C functions it calls - is Rust's
//! own (types/lib.rs), so nothing here reads the C.

include!("build_config.rs");

/// The (major, minor) of the rustc cargo builds with.
fn rustc_version() -> (u32, u32) {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let out = std::process::Command::new(&rustc).arg("--version").output()
        .unwrap_or_else(|e| panic!("{rustc} --version: {e}"));
    let version = String::from_utf8_lossy(&out.stdout);
    let mut n = version.split_whitespace().nth(1).unwrap_or("").split('.').map(|s| s.parse().unwrap_or(0));
    (n.next().unwrap_or(0), n.next().unwrap_or(0))
}

fn main() {
    // `kernel` cfg selects kernel vs. bcachefs-shim types in mod.rs; cargo never
    // sets it, so declare it to avoid the unexpected-cfg warning.
    println!("cargo::rustc-check-cfg=cfg(kernel)");
    // The kernel build's, as for C; types defined in Rust test it.
    println!("cargo::rustc-check-cfg=cfg(__KERNEL__)");
    // core::intrinsics::return_address(), for C's _RET_IP_: rustc 1.97+.
    println!("cargo::rustc-check-cfg=cfg(bch_return_address)");
    if rustc_version() >= (1, 97) {
        println!("cargo::rustc-cfg=bch_return_address");
    }
    // The module's version, which an external module build gives it.
    println!("cargo::rustc-check-cfg=cfg(bch_version)");
    println!("cargo:rerun-if-changed=build_config.rs");

    emit_userspace_config_cfgs();
}
