fn watch_dir(dir: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        // file_type() reads the dirent directly - unlike Path::is_dir() it does
        // NOT follow symlinks. This crate only walks bounded source dirs so it
        // wasn't the one that stalled, but the shared shape follows the same
        // trap: a symlink (e.g. ktest-out/kernel) into a large tree would
        // stat-storm over virtiofs. Keep both watchers symlink-safe.
        let Ok(file_type) = entry.file_type() else { continue };
        let path = entry.path();
        if file_type.is_dir() {
            if matches!(entry.file_name().to_str(), Some("target" | "ktest-out" | ".git")) {
                continue;
            }
            watch_dir(&path.to_string_lossy());
        } else if path.extension().is_some_and(|ext| ext == "h" || ext == "c") {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}

include!("../clang_target.rs");
include!("../fs/build_config.rs");

fn main() {
    use std::path::PathBuf;

    println!("cargo:rerun-if-changed=src/libbcachefs_wrapper.h");
    println!("cargo:rerun-if-changed=../clang_target.rs");
    println!("cargo:rerun-if-changed=../fs/build_config.rs");
    // Watch all C/H files that the wrapper might include, so bindgen
    // reruns when any header changes — not just the handful we used
    // to list explicitly.
    for dir in ["../fs", "../c_src", "../include"] {
        watch_dir(dir);
    }

    let out_dir: PathBuf = std::env::var_os("OUT_DIR")
        .expect("ENV Var 'OUT_DIR' Expected")
        .into();
    let top_dir: PathBuf = std::env::var_os("CARGO_MANIFEST_DIR")
        .expect("ENV Var 'CARGO_MANIFEST_DIR' Expected")
        .into();
    let root = top_dir.parent().expect("bch_bindgen should have a parent dir");
    rerun_if_userspace_config_changed(root);

    let urcu = pkg_config::probe_library("liburcu").expect("Failed to find urcu lib");
    // Tell bindgen/clang the target triple so it computes correct type
    // layout (size, alignment) for the target architecture, not the host.
    let target = std::env::var("TARGET").unwrap();
    let clang_target = clang_target_for_rust_target(&target);

    let bindings = bindgen::builder()
        .formatter(bindgen::Formatter::Prettyplease)
        .header(
            top_dir
                .join("src")
                .join("libbcachefs_wrapper.h")
                .display()
                .to_string(),
        )
        .clang_arg(format!("--target={}", clang_target))
        .clang_args(
            urcu.include_paths
                .iter()
                .map(|p| format!("-I{}", p.display())),
        )
        .clang_arg("-I..")
        .clang_arg("-I../fs")
        .clang_arg("-I../c_src")
        .clang_arg("-I../include")
        // generated headers - rust_types_gen.h - as for the C compiles:
        .clang_arg("-I../build")
        .clang_args(userspace_config_args(root))
        .clang_arg("-DRUST_BINDGEN")
        .clang_arg("-fkeep-inline-functions")
        .derive_debug(true)
        .derive_default(true)
        .layout_tests(true)
        .default_enum_style(bindgen::EnumVariation::Rust {
            non_exhaustive: true,
        })
        .bitfield_enum("btree_iter_update_trigger_flags")
        .bitfield_enum("bch_trans_commit_flags")
        .bitfield_enum("bch_write_flags")
        // What c_src/rust_shims.h exports to Rust - see the note there for why
        // these are C constants rather than macros for bindgen to evaluate.
        //
        // These have to be narrow, not a blanket BCH_.*: this crate's bindings
        // and the fs bindings are both glob-imported into one module, and the
        // headers here also reach bcachefs's own BCH_SB_*, BCH_BY_INDEX,
        // BCH_FORCE_IF_* and friends. Allowing BCH_.* binds those a second time
        // and every use site becomes an ambiguous_glob_imports error, which is
        // deny-by-default. Measured: 5 symbols becomes 360, and the build fails
        // with 102 errors.
        //
        // The cost is that bindgen drops anything not matched here and says
        // nothing, so a new shim constant surfaces as "cannot find value in
        // module c" at its use site rather than pointing at this line. If that
        // keeps happening, give the shim exports a reserved prefix rather than
        // widening these.
        .allowlist_var("BCH_BLK.*")
        .allowlist_var("BCH_FS_IOC_.*")
        .allowlist_var("BCH_SIZEOF_.*")
        .allowlist_function("raid_init")
        .allowlist_function("linux_shrinkers_init")
        .allowlist_function("sysfs_.*")
        .allowlist_var("linux_page_size")
        .allowlist_function("cmd_.*")
        .allowlist_function(".*_cmds")
        .allowlist_function("bio_.*")
        .allowlist_function("derive_passphrase")
        .allowlist_function("request_key")
        .allowlist_function("add_key")
        .allowlist_function("keyctl_search")
        .allowlist_function("match_string")
        .allowlist_function("bch2_add_key")
        .allowlist_function("bch2_opt_strs_free")
        .allowlist_function("bch2_parse_opts")
        .allowlist_function("bch2_passphrase_check")
        .allowlist_function("bch2_sb_is_encrypted")
        // tools-util and libbcachefs types/functions for Rust command conversions
        .allowlist_type("format_opts")
        .allowlist_type("dev_opts")
        .allowlist_function("ask_yn")
        .allowlist_function("read_file_str")
        .allowlist_function("read_file_u64")
        .allowlist_function("bch2_install_fatal_signal_handlers")
        .allowlist_function("copy_fs")
        .allowlist_function("rust_.*")
        .allowlist_function("bch_sb_crypt_init")
        .allowlist_function("bch_crypt_kdf_init")
        .allowlist_function("read_passphrase")
        .blocklist_function("bch2_prt_vprintf")
        .blocklist_function("bch2_inode_opts_get_inode")
        .blocklist_function("linux_shrinkers_init")
        .blocklist_function("rust_read_submit")
        .blocklist_function("rust_write_submit")
        .blocklist_type("rhash_lock_head")
        .blocklist_type("rhash.*")
        .blocklist_type("srcu_struct")
        .blocklist_type("bch_ioctl_data_event")
        .blocklist_type("__.*")
        .blocklist_type("_bindgen_ty_.*")
        .blocklist_type("accounting_.*")
        .blocklist_type("bbpos")
        .blocklist_type("bio.*")
        .blocklist_type("bkey.*")
        .blocklist_type("bpos")
        .blocklist_type("btree.*")
        .blocklist_type("bucket")
        .blocklist_type("bucket_table")
        .blocklist_type("disk_accounting_type")
        .blocklist_type("darray.*")
        .blocklist_type("fsck_err_opts")
        .blocklist_type("genradix.*")
        .blocklist_type("jset.*")
        .blocklist_type("journal.*")
        .blocklist_type("nonce")
        .blocklist_type("opt_flags")
        .blocklist_type("opt_type")
        .blocklist_type("printbuf")
        .blocklist_type("sb_names")
        .blocklist_type("subvol_inum")
        .blocklist_type("bch_[a-n].*")
        .blocklist_type("bch_[p-z].*")
        .blocklist_type("bch_opt_fn")
        .blocklist_type("bch_opt_id")
        .blocklist_type("bch_option")
        .blocklist_type("bch_opts.*")
        .allowlist_var("KEY_SPEC_.*")
        .blocklist_item("bch2_bkey_ops")
        .allowlist_type("bch_.*")
        .allowlist_type("bkey_i_.*")
        .allowlist_type("bkey_s_c_.*")
        .allowlist_type("bkey_s_.*")
        .allowlist_type("btree_flags")
        .allowlist_type("disk_accounting_type")
        .allowlist_type("fsck_err_opts")
        .rustified_enum("fsck_err_opts")
        .allowlist_type("nonce")
        .no_debug("bch_replicas_padded")
        .no_debug("jset")
        .no_debug("bch_replicas_entry_cpu")
        .newtype_enum("bcachefs_metadata_version")
        .newtype_enum("bch_opt_id")
        .newtype_enum("bch_bkey_type")
        .newtype_enum("bch_data_type")
        .newtype_enum("bch_compression_type")
        .newtype_enum("bch_reconcile_accounting_type")
        .newtype_enum("disk_accounting_type")
        .newtype_enum("bch_jset_entry_type")
        .newtype_enum("bch_kdf_types")
        .newtype_enum("bch_sb_field_type")
        .rustified_enum("bch_key_types")
        .opaque_type("gendisk")
        .opaque_type("gc_stripe")
        .opaque_type("open_bucket.*")
        .opaque_type("replicas_delta_list")
        // bch_replicas_padded is a union of a flexible-array entry and a
        // struct_size_t()-sized byte array; bindgen can't represent it and
        // emits bogus empty anon structs (which then get arch-dependent
        // alignment, breaking cross builds). Rust never touches its internals,
        // so let clang compute the size and emit an opaque blob.
        .opaque_type("bch_replicas_padded")
        // bindgen still hoists the union's anonymous members to top-level empty
        // structs even with the parent opaque; they're now unreferenced, and
        // their layout tests get arch-dependent alignment. Drop them entirely.
        .blocklist_type("bch_replicas_padded__bindgen_ty_.*")
        .allowlist_type("sb_names")
        .no_copy("btree_trans")
        .no_copy("printbuf")
        .no_copy("bch_sb_handle")
        .no_partialeq("bkey")
        .no_partialeq("bpos")
        .generate_inline_functions(true)
        // Emit C wrappers for static inline functions (e.g.
        // bch2_btree_iter_set_pos, bpos_successor) so they're callable from
        // Rust; generate_inline_functions alone only binds external-linkage
        // inlines, not static inlines in headers.
        .wrap_static_fns(true)
        .wrap_static_fns_path(out_dir.join("extern.c"))
        .generate()
        .expect("BindGen Generation Failiure: [libbcachefs_wrapper]");

    std::fs::write(
        out_dir.join("non_fs.rs"),
        packed_and_align_fix(bindings.to_string()),
    )
    .expect("Writing to output file failed for: `non_fs.rs`");

    // Compile the static-inline wrappers bindgen just generated and link them
    // in, matching the clang args bindgen parsed the headers with.
    let mut wrappers = cc::Build::new();
    wrappers
        .file(out_dir.join("extern.c"))
        .include(top_dir.join(".."))
        .include(top_dir.join("../fs"))
        .include(top_dir.join("../c_src"))
        .include(top_dir.join("../include"))
        .include(top_dir.join("../build"))
        .define("RUST_BINDGEN", None)
        .flag("-fkeep-inline-functions")
        .warnings(false);
    for f in userspace_config_args(root) {
        wrappers.flag(f);
    }
    for p in &urcu.include_paths {
        wrappers.include(p);
    }
    wrappers.compile("bcachefs_static_wrappers");


    // dh-cargo Built-Using (Debian): point the path at the top-level crate (the
    // package root, == the dpkg build's $PWD), so dh-cargo-built-using sees this
    // lib as built from our own in-tree source and skips it ("top-level crate
    // being built, no need to add Built-Using") rather than running `dpkg -S` on
    // a build path no Debian package owns. top_dir is bch_bindgen/; the workspace
    // root ($PWD under dpkg-buildpackage) is its parent.
    println!(
        "dh-cargo:deb-built-using=bcachefs_static_wrappers=0={}",
        top_dir.parent().expect("bch_bindgen has a parent dir").display()
    );

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
        .generate()
        .expect("BindGen Generation Failiure: [Keyutils]");
    bindings
        .write_to_file(out_dir.join("keyutils.rs"))
        .expect("Writing to output file failed for: `keyutils.rs`");
}

// rustc has a limitation where it does not allow structs with a "packed" attribute to contain a
// member with an "align(N)" attribute. There are a few types in bcachefs with this problem. We can
// "fix" these types by stripping off "packed" from the outer type, or "align(N)" from the inner
// type. For all of the affected types, stripping "packed" from the outer type happens to preserve
// the same layout in Rust as in C.
//
// Some types are only affected on attributes on architectures where the natural alignment of u64
// is 4 instead of 8, for example i686 or ppc64: struct bch_csum and struct bch_sb_layout have
// "align(8)" added on such architecutres. These types are included by several "packed" types:
//   - bch_extent_crc128
//   - jset
//   - btree_node_entry
//   - bch_sb
//
// TODO: find a way to conditionally include arch-specific modifications when compiling for that
// target arch. Regular conditional compilation won't work here since build scripts are always
// compiled for the host arch, not the target arch, so that won't work when cross-compiling.
fn packed_and_align_fix(bindings: std::string::String) -> std::string::String {
    let bindings = bindings
        .replace(
            "#[repr(C, packed(8))]\npub struct btree_node {",
            "#[repr(C, align(8))]\npub struct btree_node {",
        )
        .replace(
            "#[repr(C, packed(8))]\n#[derive(Debug, Default, Copy, Clone)]\npub struct bch_extent_crc128 {",
            "#[repr(C, align(8))]\n#[derive(Debug, Default, Copy, Clone)]\npub struct bch_extent_crc128 {",
        )
        .replace(
            "#[repr(C, packed(8))]\npub struct jset {",
            "#[repr(C, align(8))]\npub struct jset {",
        )
        .replace(
            "#[repr(C, packed(8))]\npub struct btree_node_entry {",
            "#[repr(C, align(8))]\npub struct btree_node_entry {",
        )
        .replace(
            "#[repr(C, packed(8))]\npub struct bch_sb {",
            "#[repr(C, align(8))]\npub struct bch_sb {",
        );

    // On architectures where u64 has alignment 4 (i686, ppc32), Rust's repr(C)
    // doesn't propagate the explicit __aligned(8) from struct bkey to types
    // that contain it (bkey_i_*, btree_node/btree_node_entry anonymous unions,
    // bch_ioctl_query_accounting). Fix by adding align(8) to all such types.
    //
    // These types all contain bkey (which is __packed __aligned(8) in C),
    // so they inherit alignment 8 on all architectures. Rust's repr(C) doesn't
    // propagate this — it computes alignment from the fields' natural alignment,
    // which for u64 is 4 on 32-bit.
    let target_ptr_width = std::env::var("CARGO_CFG_TARGET_POINTER_WIDTH")
        .unwrap_or_default();
    let bindings = if target_ptr_width == "32" {
        let mut result = String::with_capacity(bindings.len());
        let mut lines = bindings.lines().peekable();
        while let Some(line) = lines.next() {
            if line == "#[repr(C)]" {
                if let Some(&next) = lines.peek() {
                    let needs_align8 = next.contains("pub struct bkey_i_")
                        || next.contains("pub struct btree_node__bindgen_ty_1")
                        || next.contains("pub struct btree_node_entry__bindgen_ty_1")
                        || next.contains("pub struct bch_ioctl_query_accounting");
                    if needs_align8 {
                        result.push_str("#[repr(C, align(8))]");
                    } else {
                        result.push_str(line);
                    }
                } else {
                    result.push_str(line);
                }
            } else {
                result.push_str(line);
            }
            result.push('\n');
        }
        result
    } else {
        bindings
    };

    bindings
}
