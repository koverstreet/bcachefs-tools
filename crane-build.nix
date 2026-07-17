{
  lib,
  pkgs,
  stdenvNoCC,

  # build time
  jq,
  pkg-config,
  rustPlatform,
  rust-bindgen,
  versionCheckHook,

  # run time
  fuse3,
  keyutils,
  libaio,
  libsodium,
  libunwind,
  liburcu,
  libuuid,
  lz4,
  udev,
  zlib,
  zstd,

  crane,
  rustVersion ? "latest",
  version,
}:
let
  # Use the build-platform toolchain with the target added, so cross builds
  # cross-compile with the binary x86_64 rustc + the target's std rather than
  # building a cross rustc from source (slow, and trips rustc bootstrap bugs).
  # For native builds pkgsBuildHost == pkgs and the target is native, so this is
  # a no-op.
  craneLib = (crane.mkLib pkgs).overrideToolchain (
    _: pkgs.pkgsBuildHost.rust-bin.stable."${rustVersion}".minimal.override {
      extensions = [ "clippy" ];
      # outer (cross) pkgs: the host platform is the cross target, so we add its
      # rust-std to the build-platform toolchain. crane calls this with the
      # build-platform pkgs, so taking the target from there would be a no-op.
      targets = [ pkgs.stdenv.hostPlatform.rust.rustcTarget ];
    }
  );

  args = {
    inherit version;
    src = lib.fileset.toSource {
      root = ./.;
      fileset = lib.fileset.fileFilter ({ hasExt, ... }: !hasExt "nix") ./.;
    };
    strictDeps = true;

    env = {
      # bindgen runs at build time, so it must be the build-platform binary;
      # referencing the (cross) host package here drags in a from-source cross
      # rust toolchain (rustc/cargo/rustfmt-nightly).
      BINDGEN = "${pkgs.buildPackages.rust-bindgen}/bin/bindgen";
      PKG_CONFIG_SYSTEMD_SYSTEMDSYSTEMUNITDIR = "${placeholder "out"}/lib/systemd/system";
      PKG_CONFIG_UDEV_UDEVDIR = "${placeholder "out"}/lib/udev";
    };

    makeFlags = [
      "INITRAMFS_DIR=${placeholder "out"}/etc/initramfs-tools"
      "PREFIX=${placeholder "out"}"
      "VERSION=${version}"
    ];

    dontStrip = true;

    nativeBuildInputs = [
      jq
      pkg-config
      rustPlatform.bindgenHook
      rust-bindgen
    ];

    buildInputs = [
      keyutils
      libaio
      libsodium
      libunwind
      liburcu
      libuuid
      lz4
      udev
      zlib
      zstd
    ];

    checkFlags = lib.optionals (stdenvNoCC.hostPlatform.isAarch64) [
      "--skip=bcachefs::bindgen_test_layout_bch_replicas_padded__bindgen_ty_1"
      "--skip=bcachefs::bindgen_test_layout_bch_replicas_padded__bindgen_ty_2"
      "--skip=bcachefs::bindgen_test_layout_bch_replicas_padded__bindgen_ty_3"
      "--skip=bcachefs::bindgen_test_layout_bch_replicas_padded__bindgen_ty_4"
    ];

    checkPhaseCargoCommand = ''
      cargo test --profile release -- --nocapture $checkFlags
    '';
  };

  cargoArtifacts = craneLib.buildDepsOnly args;

  package = craneLib.buildPackage (
    args
    // {
      inherit cargoArtifacts;

      outputs = [
        "out"
        "dkms"
      ];

      makeFlags = args.makeFlags ++ [
        "DKMSDIR=${placeholder "dkms"}"
      ];

      enableParallelBuilding = true;
      buildPhaseCargoCommand = ''
        make ''${enableParallelBuilding:+-j''${NIX_BUILD_CORES}} $makeFlags
      '';
      doNotPostBuildInstallCargoBinaries = true;
      enableParallelInstalling = true;
      installPhaseCommand = ''
        make ''${enableParallelInstalling:+-j''${NIX_BUILD_CORES}} $makeFlags install install_dkms
      '';

      doInstallCheck = true;
      nativeInstallCheckInputs = [ versionCheckHook ];
      versionCheckProgramArg = "version";

      passthru.kernelModule = import ./module-build.nix package;

      meta = {
        description = "Userspace tools for bcachefs";
        license = lib.licenses.gpl2Only;
        mainProgram = "bcachefs";
      };
    }
  );

  packageFuse = package.overrideAttrs (
    final: prev: {
      makeFlags = prev.makeFlags ++ [ "BCACHEFS_FUSE=1" ];
      buildInputs = prev.buildInputs ++ [ fuse3 ];
    }
  );

  cargo-clippy = craneLib.cargoClippy (
    args
    // {
      inherit cargoArtifacts;
      cargoClippyExtraArgs = "--all-targets --all-features -- --deny warnings";
    }
  );

  # we have to build our own `craneLib.cargoTest`
  cargo-test = craneLib.mkCargoDerivation (
    args
    // {
      inherit cargoArtifacts;
      doCheck = true;

      enableParallelChecking = true;

      pnameSuffix = "-test";
      buildPhaseCargoCommand = "";

      checkPhaseCargoCommand = ''
        make ''${enableParallelChecking:+-j''${NIX_BUILD_CORES}} $makeFlags libbcachefs.a
        cargo test --profile release -- --nocapture $checkFlags
      '';
    }
  );
in
{
  inherit
    cargo-clippy
    cargo-test
    package
    packageFuse
    ;
}
