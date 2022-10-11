fn main() {
	use std::path::PathBuf;
	// use std::process::Command;

	let out_dir: PathBuf = std::env::var_os("OUT_DIR").expect("ENV Var 'OUT_DIR' Expected").into();
	let top_dir: PathBuf = std::env::var_os("CARGO_MANIFEST_DIR")
		.expect("ENV Var 'CARGO_MANIFEST_DIR' Expected")
		.into();
	let libbcachefs_inc_dir =
		std::env::var("LIBBCACHEFS_INCLUDE").unwrap_or_else(|_| top_dir.join("libbcachefs").display().to_string());
	let libbcachefs_inc_dir = std::path::Path::new(&libbcachefs_inc_dir);
	println!("{}", libbcachefs_inc_dir.display());

	println!("cargo:rustc-link-lib=dylib=bcachefs");
	println!("cargo:rustc-link-search={}", env!("LIBBCACHEFS_LIB"));

	let _libbcachefs_dir = top_dir.join("libbcachefs").join("libbcachefs");
	let bindings = bindgen::builder()
		.header(top_dir.join("src").join("libbcachefs_wrapper.h").display().to_string())
		.clang_arg(format!("-I{}", libbcachefs_inc_dir.join("include").display()))
		.clang_arg(format!("-I{}", libbcachefs_inc_dir.display()))
		.clang_arg("-DZSTD_STATIC_LINKING_ONLY")
		.clang_arg("-DNO_BCACHEFS_FS")
		.clang_arg("-D_GNU_SOURCE")
		.derive_debug(true)
		.derive_default(true)
		.derive_eq(true)
		.layout_tests(true)
		.default_enum_style(bindgen::EnumVariation::Rust { non_exhaustive: true })
		.allowlist_function(".*bch2_.*")
		// .allowlist_function("bch2_read_super")
		// .allowlist_function("bch2_sb_field_.*")
		// .allowlist_function("bch2_super_write")
		// .allowlist_function("bch2_chacha_encrypt_key")
		// .allowlist_function("__bch2_super_read")
		.allowlist_function("bio_.*")
		.allowlist_function("bch2_super_write_fd")
		.allowlist_function("derive_passphrase")
		.allowlist_function("request_key")
		.allowlist_function("add_key")
		.allowlist_function("keyctl_search")
		.blocklist_type("bch_extent_ptr")
		.blocklist_type("btree_node")
		.blocklist_type("bch_extent_crc32")
		.blocklist_type("rhash_lock_head")
		.blocklist_type("srcu_struct")
		.allowlist_var("BCH_.*")
		.allowlist_var("KEY_SPEC_.*")
		.allowlist_type("bch_kdf_types")
		.allowlist_type("bch_sb_field_.*")
		.allowlist_type("bch_encrypted_key")
		.allowlist_type("nonce")
		.newtype_enum("bch_kdf_types")
		.opaque_type("gendisk")
		.opaque_type("bkey")
		// .opaque_type("bch_extent_ptr")
		// .opaque_type("bch_extent_crc32")
		.opaque_type("open_bucket.*")
		.generate()
		.expect("BindGen Generation Failure: [libbcachefs_wrapper]");
	bindings
		.write_to_file(out_dir.join("bcachefs.rs"))
		.expect("Writing to output file failed for: `bcachefs.rs`");

	let keyutils = pkg_config::probe_library("libkeyutils").expect("Failed to find keyutils lib");
	let bindings = bindgen::builder()
		.header(top_dir.join("src").join("keyutils_wrapper.h").display().to_string())
		.clang_args(keyutils.include_paths.iter().map(|p| format!("-I{}", p.display())))
		.generate()
		.expect("BindGen Generation Failure: [Keyutils]");
	bindings
		.write_to_file(out_dir.join("keyutils.rs"))
		.expect("Writing to output file failed for: `keyutils.rs`");
}
