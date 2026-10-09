// SPDX-License-Identifier: GPL-2.0
//! Userspace (cargo) build script: the build configuration's cfgs, and
//! linking libbcachefs.a. The fs crate's C interface - its types, and the C
//! functions it calls - is Rust's own (types/lib.rs), so nothing here reads
//! the C.

include!("build_config.rs");

include!("../build-link.rs");

fn main() {
    // `kernel` cfg selects kernel vs. bcachefs-shim types in mod.rs; cargo never
    // sets it, so declare it to avoid the unexpected-cfg warning.
    println!("cargo::rustc-check-cfg=cfg(kernel)");
    // The build whose object rust_types_gen reads - see types/lib.rs.
    println!("cargo::rustc-check-cfg=cfg(bch_cstruct_records)");
    // The kernel build's, as for C; types defined in Rust test it.
    println!("cargo::rustc-check-cfg=cfg(__KERNEL__)");
    println!("cargo:rerun-if-changed=build_config.rs");

    let src = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"); // = fs/
    let root = std::path::Path::new(&src).parent().expect("fs crate has a parent dir");
    rerun_if_userspace_config_changed(root);
    emit_userspace_config_cfgs(root);

    println!("cargo:rerun-if-changed=../build-link.rs");
    link_bcachefs(
        &root.display().to_string(),
        &root.join("libbcachefs.a").display().to_string(),
        false,
    );
}
