fn main() {
    // Export static symbols for dladdr() in tools-side prt_addr_symbol
    println!("cargo:rustc-link-arg-bins=-rdynamic");

    // fuser crate talks to /dev/fuse directly — no libfuse3 needed
}
