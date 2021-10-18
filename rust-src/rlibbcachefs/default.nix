{ lib

, stdenv
, glibc
, llvmPackages
, rustPlatform

, bcachefs

, ...
}: rustPlatform.buildRustPackage ( let 
	cargo = lib.trivial.importTOML ./Cargo.toml;
in {
	pname = cargo.package.name;
	version = cargo.package.version;
	
	src = builtins.path { path = ../.; name = "rust-src"; };
	sourceRoot = "rust-src/rlibbcachefs";

	cargoLock = { lockFile = ./Cargo.lock; };

	nativeBuildInputs = bcachefs.bch_bindgen.nativeBuildInputs;
	buildInputs = bcachefs.bch_bindgen.buildInputs;
	inherit (bcachefs.bch_bindgen)
		LIBBCACHEFS_INCLUDE
		LIBCLANG_PATH
		BINDGEN_EXTRA_CLANG_ARGS;

	# -isystem ${llvmPackages.libclang.lib}/lib/clang/${lib.getVersion llvmPackages.libclang}/include";
	# CFLAGS = "-I${llvmPackages.libclang.lib}/include";
	# LDFLAGS = "-L${libcdev}";

	doCheck = false;
	
	# NIX_DEBUG = 4;
})