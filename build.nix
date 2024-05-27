{
  lib,
  stdenv,
  pkg-config,
  attr,
  libuuid,
  libsodium,
  keyutils,
  liburcu,
  zlib,
  libaio,
  udev,
  zstd,
  lz4,
  nix-gitignore,
  rustPlatform,
  rustc,
  cargo,
  fuse3,
  fuseSupport ? false,
}:
let
  src = nix-gitignore.gitignoreSource [ ] ./.;

  commit = lib.strings.substring 0 7 (builtins.readFile ./.bcachefs_revision);
  version = "git-${commit}";
in
stdenv.mkDerivation {
  inherit src version;

  pname = "bcachefs-tools";

  nativeBuildInputs = [
    pkg-config
    cargo
    rustc
    rustPlatform.cargoSetupHook
    rustPlatform.bindgenHook
  ];

  buildInputs = [
    libaio
    keyutils # libkeyutils
    lz4 # liblz4

    libsodium
    liburcu
    libuuid
    zstd # libzstd
    zlib # zlib1g
    attr
    udev
  ] ++ lib.optional fuseSupport fuse3;

  ${if fuseSupport then "BCACHEFS_FUSE" else null} = "1";

  cargoRoot = ".";
  # when git-based crates are updated, run:
  # nix run github:Mic92/nix-update -- --version=skip --flake default
  # to update the hashes
  cargoDeps = rustPlatform.importCargoLock { lockFile = "${src}/Cargo.lock"; };

  makeFlags = [
    "DESTDIR=${placeholder "out"}"
    "PREFIX="
    "VERSION=${commit}"
  ];

  dontStrip = true;
  checkPhase = "./target/release/bcachefs version";
  doCheck = true;

  meta = {
    mainProgram = "bcachefs";
    license = lib.licenses.gpl2Only;
  };
}
