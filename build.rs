fn main() {
    println!("cargo:rustc-link-search=.");
    println!("cargo:rerun-if-changed=libbcachefs.a");
    println!("cargo:rustc-link-lib=static:+whole-archive=bcachefs");

    println!("cargo:rustc-link-lib=urcu");
    println!("cargo:rustc-link-lib=zstd");
    println!("cargo:rustc-link-lib=blkid");
    println!("cargo:rustc-link-lib=uuid");
    println!("cargo:rustc-link-lib=sodium");
    println!("cargo:rustc-link-lib=z");
    println!("cargo:rustc-link-lib=lz4");
    println!("cargo:rustc-link-lib=zstd");
    println!("cargo:rustc-link-lib=udev");
    println!("cargo:rustc-link-lib=keyutils");
    println!("cargo:rustc-link-lib=aio");
    println!("cargo:rustc-link-lib=unwind");

    // Export static symbols for dladdr() in tools-side prt_addr_symbol
    println!("cargo:rustc-link-arg-bins=-rdynamic");

    // fuser crate talks to /dev/fuse directly — no libfuse3 needed
}
