fn main() {
    println!("cargo:rustc-link-search=.");
    println!("cargo:rerun-if-changed=libbcachefs.a");
    println!("cargo:rustc-link-lib=static:+whole-archive=bcachefs");

    // libaio is missing a pkg-config file
    println!("cargo:rustc-link-lib=aio");

    system_deps::Config::new().probe().unwrap();
}
