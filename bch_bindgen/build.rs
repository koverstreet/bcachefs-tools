include!("../clang_target.rs");

fn main() {
    use std::path::PathBuf;

    println!("cargo:rerun-if-changed=src/keyutils_wrapper.h");
    println!("cargo:rerun-if-changed=../clang_target.rs");

    let out_dir: PathBuf = std::env::var_os("OUT_DIR")
        .expect("ENV Var 'OUT_DIR' Expected")
        .into();
    let top_dir: PathBuf = std::env::var_os("CARGO_MANIFEST_DIR")
        .expect("ENV Var 'CARGO_MANIFEST_DIR' Expected")
        .into();

    // Tell bindgen/clang the target triple so it computes correct type
    // layout (size, alignment) for the target architecture, not the host.
    let target = std::env::var("TARGET").unwrap();
    let clang_target = clang_target_for_rust_target(&target);

    let keyutils = pkg_config::probe_library("libkeyutils").expect("Failed to find keyutils lib");
    let bindings = bindgen::builder()
        .header(
            top_dir
                .join("src")
                .join("keyutils_wrapper.h")
                .display()
                .to_string(),
        )
        .clang_args(
            keyutils
                .include_paths
                .iter()
                .map(|p| format!("-I{}", p.display())),
        )
        .clang_arg(format!("--target={}", clang_target))
        .rust_edition(bindgen::RustEdition::Edition2024)
        .generate()
        .expect("BindGen Generation Failiure: [Keyutils]");
    bindings
        .write_to_file(out_dir.join("keyutils.rs"))
        .expect("Writing to output file failed for: `keyutils.rs`");
}
