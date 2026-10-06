include!("build-link.rs");

fn main() {
    println!("cargo:rerun-if-changed=build-link.rs");
    let root = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    link_bcachefs(&root, &format!("{root}/libbcachefs.a"), true);

    // Export static symbols for dladdr() in tools-side prt_addr_symbol
    println!("cargo:rustc-link-arg-bins=-rdynamic");

    // fuser crate talks to /dev/fuse directly — no libfuse3 needed
}
