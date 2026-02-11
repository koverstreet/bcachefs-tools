#[derive(Debug)]
pub struct Fix753 {}
impl bindgen::callbacks::ParseCallbacks for Fix753 {
    fn item_name(&self, original_item_name: &str) -> Option<String> {
        Some(original_item_name.trim_start_matches("Fix753_").to_owned())
    }
}

fn main() {
    use std::path::PathBuf;

    println!("cargo:rerun-if-changed=src/libbcachefs_wrapper.h");

    let out_dir: PathBuf = std::env::var_os("OUT_DIR")
        .expect("ENV Var 'OUT_DIR' Expected")
        .into();
    let top_dir: PathBuf = std::env::var_os("CARGO_MANIFEST_DIR")
        .expect("ENV Var 'CARGO_MANIFEST_DIR' Expected")
        .into();

    let urcu = pkg_config::probe_library("liburcu").expect("Failed to find urcu lib");
    let bindings = bindgen::builder()
        .formatter(bindgen::Formatter::Prettyplease)
        .header(
            top_dir
                .join("src")
                .join("libbcachefs_wrapper.h")
                .display()
                .to_string(),
        )
        .clang_args(
            urcu.include_paths
                .iter()
                .map(|p| format!("-I{}", p.display())),
        )
        .clang_arg("-I..")
        .clang_arg("-I../libbcachefs")
        .clang_arg("-I../c_src")
        .clang_arg("-I../include")
        .clang_arg("-DZSTD_STATIC_LINKING_ONLY")
        .clang_arg("-DNO_BCACHEFS_FS")
        .clang_arg("-D_GNU_SOURCE")
        .clang_arg("-DRUST_BINDGEN")
        .clang_arg("-fkeep-inline-functions")
        .derive_debug(true)
        .derive_default(true)
        .layout_tests(true)
        .default_enum_style(bindgen::EnumVariation::Rust {
            non_exhaustive: true,
        })
        .bitfield_enum("btree_iter_update_trigger_flags")
        .allowlist_function("bcachefs_usage")
        .allowlist_function("raid_init")
        .allowlist_function("linux_shrinkers_init")
        .allowlist_function("sysfs_.*")
        .allowlist_var("linux_page_size")
        .allowlist_function("cmd_.*")
        .allowlist_function(".*_cmds")
        .allowlist_function(".*bch2_.*")
        .allowlist_function("bcache_fs_open_fallible")
        .allowlist_function("bcache_fs_close")
        .allowlist_function("bio_.*")
        .allowlist_function("derive_passphrase")
        .allowlist_function("request_key")
        .allowlist_function("add_key")
        .allowlist_function("keyctl_search")
        .allowlist_function("match_string")
        .allowlist_function("printbuf.*")
        .allowlist_function("_bch2_err_matches")
        // tools-util functions for Rust command conversions
        .allowlist_function("open_for_format")
        .allowlist_function("ask_yn")
        .allowlist_function("get_or_split_cmdline_devs")
        .allowlist_function("dev_mounted")
        .allowlist_function("dev_to_mount")
        .allowlist_function("read_string_list_or_die")
        .allowlist_function("read_flag_list_or_die")
        .allowlist_function("write_file_str")
        .allowlist_function("read_file_str")
        .allowlist_function("read_file_u64")
        .allowlist_function("get_size")
        .allowlist_function("fd_to_dev_model")
        .allowlist_function("version_parse")
        .allowlist_function("copy_fs")
        .blocklist_function("bch2_prt_vprintf")
        .blocklist_type("rhash_lock_head")
        .blocklist_type("srcu_struct")
        .blocklist_type("bch_ioctl_data_event")
        .allowlist_var("BCH_.*")
        .allowlist_var("KEY_SPEC_.*")
        .allowlist_var("Fix753_.*")
        .allowlist_var("bch.*")
        .allowlist_var("__bch2.*")
        .allowlist_var("__BTREE_ITER.*")
        .allowlist_var("BTREE_ITER.*")
        .blocklist_item("bch2_bkey_ops")
        .allowlist_type("bch_.*")
        .allowlist_type("disk_accounting_type")
        .allowlist_type("fsck_err_opts")
        .rustified_enum("fsck_err_opts")
        .allowlist_type("nonce")
        .no_debug("bch_replicas_padded")
        .no_debug("jset")
        .no_debug("bch_replicas_entry_cpu")
        .newtype_enum("bch_kdf_types")
        .rustified_enum("bch_key_types")
        .opaque_type("gendisk")
        .opaque_type("gc_stripe")
        .opaque_type("open_bucket.*")
        .opaque_type("replicas_delta_list")
        .allowlist_type("sb_names")
        .no_copy("btree_trans")
        .no_copy("printbuf")
        .no_copy("bch_sb_handle")
        .no_partialeq("bkey")
        .no_partialeq("bpos")
        .generate_inline_functions(true)
        .parse_callbacks(Box::new(Fix753 {}))
        .generate()
        .expect("BindGen Generation Failiure: [libbcachefs_wrapper]");

    std::fs::write(
        out_dir.join("bcachefs.rs"),
        packed_and_align_fix(bindings.to_string()),
    )
    .expect("Writing to output file failed for: `bcachefs.rs`");

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
    bindings
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
        )
}
