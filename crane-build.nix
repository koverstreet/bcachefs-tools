{
  lib,
  pkgs,
  stdenvNoCC,

  # build time
  pkg-config,
  rustPlatform,
  versionCheckHook,

  # run time
  fuse3,
  keyutils,
  libaio,
  libsodium,
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
  craneLib = (crane.mkLib pkgs).overrideToolchain (
    p: p.rust-bin.stable."${rustVersion}".minimal.override { extensions = [ "clippy" ]; }
  );

  args = {
    inherit version;
    src = lib.fileset.toSource {
      root = ./.;
      fileset = lib.fileset.fileFilter ({ hasExt, ... }: !hasExt "nix") ./.;
    };
    strictDeps = true;

    env = {
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
      pkg-config
      rustPlatform.bindgenHook
    ];

    buildInputs = [
      keyutils
      libaio
      libsodium
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
