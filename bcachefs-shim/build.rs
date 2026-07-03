// Bindgen for the userspace Linux kernel-compat layer (the in-tree `include/`
// shim). Parses the same fs/ wrapper header as the bcachefs-kernel crate, but
// emits only the symbols *defined under* include/ (plus the system primitives
// they transitively reference, e.g. __u64/__le16). The bcachefs-kernel crate
// blocklists this same set and imports it from here, so each C type is bound
// exactly once.
use std::path::PathBuf;

fn main() {
    let out_dir: PathBuf = std::env::var_os("OUT_DIR")
        .expect("ENV Var 'OUT_DIR' Expected")
        .into();
    let top_dir: PathBuf = std::env::var_os("CARGO_MANIFEST_DIR")
        .expect("ENV Var 'CARGO_MANIFEST_DIR' Expected")
        .into();
    let root = top_dir
        .parent()
        .expect("bcachefs-shim should have a parent dir")
        .to_path_buf();
    let fs_dir = root.join("fs");
    let include_dir = root.join("include");
    let wrapper = top_dir
        .join("src")
        .join("wrapper.h");

    println!("cargo:rerun-if-changed={}", wrapper.display());
    println!("cargo:rerun-if-changed={}", include_dir.display());

    let urcu = pkg_config::probe_library("liburcu").expect("Failed to find urcu lib");
    let target = std::env::var("TARGET").unwrap();

    let bindings = bindgen::builder()
        .formatter(bindgen::Formatter::Prettyplease)
        .header(wrapper.display().to_string())
        .clang_arg(format!("--target={}", target))
        .clang_args(
            urcu.include_paths
                .iter()
                .map(|p| format!("-I{}", p.display())),
        )
        .clang_arg(format!("-I{}", root.display()))
        .clang_arg(format!("-I{}", fs_dir.display()))
        .clang_arg(format!("-I{}", root.join("c_src").display()))
        .clang_arg(format!("-I{}", include_dir.display()))
        .clang_arg("-DZSTD_STATIC_LINKING_ONLY")
        .clang_arg("-DNO_BCACHEFS_FS")
        .clang_arg("-D_GNU_SOURCE")
        .clang_arg("-DRUST_BINDGEN")
        .clang_arg("-fkeep-inline-functions")
        .derive_debug(true)
        .derive_default(true)
        .layout_tests(true)
        // Emit `::core::ffi` rather than `::std::os::raw`, so the bindings are
        // valid in both std (userspace) and no_std.
        .use_core()
        .default_enum_style(bindgen::EnumVariation::Rust {
            non_exhaustive: true,
        })
        // Own only what's defined under include/ — the kernel-compat surface.
        // allowlist_recursively (default) pulls in the system primitives these
        // reference (__u64 etc.), so they're emitted here too.
        .allowlist_file(format!("{}/.*", include_dir.display()))
        // fs/ references a few system / urcu types directly that aren't
        // reachable from include/ alone; own them here too so the bcachefs-kernel
        // crate can import them rather than redefine them.
        .allowlist_type("uid_t")
        .allowlist_type("gid_t")
        .allowlist_type("cds_.*")
        // Never emit fs/ types: those are bcachefs-kernel's, and some (printbuf)
        // even carry Rust impls there. allowlist_recursively would otherwise
        // pull them in through an include/ reference.
        .blocklist_file(format!("{}/.*", fs_dir.display()))
        // The kernel's DEFINE_LOCK_GUARD / DEFINE_CLASS cleanup machinery
        // (class_*_constructor/destructor/lock_ptr) are never called from Rust,
        // and wrap_static_fns can't emit valid C wrappers for them.
        .blocklist_function("class_.*")
        .blocklist_function("__class_.*")
        // printbuf is an fs/ type owned by bcachefs-kernel; don't emit the
        // userspace helpers that take it (they reference the blocklisted type
        // and belong to bch_bindgen anyway).
        .blocklist_function("prt_.*")
        .blocklist_function("sysfs_.*")
        // Empty opaque structs — only ever used behind pointers. As zero-field
        // structs they make every extern fn that transitively embeds one (via
        // bch_fs's rhashtable) trip improper_ctypes. opaque_type gives them a
        // blob field so they're FFI-safe, without us inventing a layout.
        .opaque_type("rhash_lock_head")
        .opaque_type("srcu_struct")
        .opaque_type("bch_ioctl_data_event")
        .generate_inline_functions(true)
        .wrap_static_fns(true)
        .wrap_static_fns_path(out_dir.join("extern.c"))
        .generate()
        .expect("BindGen Generation Failure: [bcachefs-shim]");

    std::fs::write(out_dir.join("bindings.rs"), bindings.to_string())
        .expect("Writing to output file failed for: `bindings.rs`");

    // Compile the static-inline wrappers bindgen generated, matching the clang
    // args the headers were parsed with.
    let mut wrappers = cc::Build::new();
    wrappers
        .file(out_dir.join("extern.c"))
        .include(&root)
        .include(&fs_dir)
        .include(root.join("c_src"))
        .include(&include_dir)
        .define("ZSTD_STATIC_LINKING_ONLY", None)
        .define("NO_BCACHEFS_FS", None)
        .define("_GNU_SOURCE", None)
        .define("RUST_BINDGEN", None)
        .flag("-fkeep-inline-functions")
        .warnings(false);
    for p in &urcu.include_paths {
        wrappers.include(p);
    }
    wrappers.compile("bcachefs_shim_static_wrappers");

    // dh-cargo Built-Using (Debian): point at the workspace root (== the dpkg
    // build's $PWD) so dh-cargo-built-using sees this lib as built from our own
    // in-tree source and skips it, rather than aborting on a build path no
    // Debian package owns. Mirrors the same declaration in fs/build.rs and
    // bch_bindgen/build.rs. `root` is top_dir's parent, i.e. the workspace root.
    println!(
        "dh-cargo:deb-built-using=bcachefs_shim_static_wrappers=0={}",
        root.display()
    );
}
