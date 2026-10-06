fn link_bcachefs(link_search: &str, libbcachefs: &str, whole_archive: bool) {
    println!("cargo:rustc-link-search={}", link_search);
    println!("cargo:rerun-if-changed={}", libbcachefs);
    if whole_archive {
        println!("cargo:rustc-link-lib=static:+whole-archive=bcachefs");
    } else {
        println!("cargo:rustc-link-lib=static=bcachefs");
    }

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
}
