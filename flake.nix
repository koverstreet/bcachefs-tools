{
  description = "Userspace tools for bcachefs";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";

    flake-parts.url = "github:hercules-ci/flake-parts";

    treefmt-nix = {
      url = "github:numtide/treefmt-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    crane.url = "github:ipetkov/crane";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    flake-compat = {
      url = "github:edolstra/flake-compat";
      flake = false;
    };
  };

  outputs =
    inputs@{
      self,
      nixpkgs,
      flake-parts,
      treefmt-nix,
      crane,
      rust-overlay,
      flake-compat,
    }:
    let
      systems = nixpkgs.lib.filter (s: nixpkgs.lib.hasSuffix "-linux" s) nixpkgs.lib.systems.flakeExposed;
    in
    flake-parts.lib.mkFlake { inherit inputs; } {
      imports = [ inputs.treefmt-nix.flakeModule ];

      inherit systems;

      perSystem =
        {
          self',
          config,
          lib,
          system,
          ...
        }:
        let
          inherit (builtins) readFile split;
          inherit (lib.lists) findFirst;
          inherit (lib.strings) hasPrefix removePrefix substring;

          myOverlays = [
            (import rust-overlay)
            (_: prev: {
              util-linux = prev.util-linux.override {
                ncursesSupport = false;
                nlsSupport = false;
                pamSupport = false;
                systemdSupport = false;
              };
            })
          ];
          pkgs = import nixpkgs {
            inherit system;
            overlays = myOverlays;
          };

          cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
          rustfmtToml = builtins.fromTOML (builtins.readFile ./rustfmt.toml);

          rev = self.shortRev or self.dirtyShortRev or (substring 0 8 self.lastModifiedDate);
          makefileVersion = removePrefix "VERSION=" (
            findFirst (line: hasPrefix "VERSION=" line) "VERSION=0.0.0" (split "\n" (readFile ./Makefile))
          );
          version = "${makefileVersion}+${rev}";

          mkCommon =
            {
              crane,
              pkgs,
              rustVersion ? "latest",

              # build time
              buildPackages,
              pkg-config,
              rustPlatform,
              stdenv,

              # run time
              keyutils,
              libaio,
              libsodium,
              liburcu,
              libuuid,
              lz4,
              udev,
              util-linux,
              zlib,
              zstd,
            }:
            let
              inherit (stdenv) cc hostPlatform;

              craneLib = (crane.mkLib pkgs).overrideToolchain (
                p: p.rust-bin.stable."${rustVersion}".minimal.override { extensions = [ "clippy" ]; }
              );

              args = {
                inherit version;
                src = self;
                strictDeps = true;

                env = {
                  PKG_CONFIG_SYSTEMD_SYSTEMDSYSTEMUNITDIR = "${placeholder "out"}/lib/systemd/system";
                  PKG_CONFIG_UDEV_UDEVDIR = "${placeholder "out"}/lib/udev";

                  CARGO_BUILD_TARGET = hostPlatform.rust.rustcTargetSpec;
                  "CARGO_TARGET_${hostPlatform.rust.cargoEnvVarTarget}_LINKER" = "${cc.targetPrefix}cc";
                  HOST_CC = "${cc.nativePrefix}cc";
                  TARGET_CC = "${cc.targetPrefix}cc";
                };

                makeFlags = [
                  "INITRAMFS_DIR=${placeholder "out"}/etc/initramfs-tools"
                  "PREFIX=${placeholder "out"}"
                  "VERSION=${version}"
                ];

                dontStrip = true;

                depsBuildBuild = [
                  buildPackages.stdenv.cc
                ];

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
                  util-linux
                  zlib
                  zstd
                ];

                meta = {
                  description = "Userspace tools for bcachefs";
                  license = lib.licenses.gpl2Only;
                  mainProgram = "bcachefs";
                };
              };

              cargoArtifacts = craneLib.buildDepsOnly args;
            in
            {
              inherit args cargoArtifacts craneLib;
            };
          common = pkgs.callPackage mkCommon { inherit crane; };

          mkPackage =
            { common, name }:
            common.craneLib.buildPackage (
              common.args
              // {
                inherit (common) cargoArtifacts;
                pname = name;

                enableParallelBuilding = true;
                buildPhaseCargoCommand = ''
                  make ''${enableParallelBuilding:+-j''${NIX_BUILD_CORES}} $makeFlags
                '';
                doNotPostBuildInstallCargoBinaries = true;
                installPhaseCommand = ''
                  make ''${enableParallelBuilding:+-j''${NIX_BUILD_CORES}} $makeFlags install
                '';

                doInstallCheck = true;
                installCheckPhase = ''
                  runHook preInstallCheck

                  test "$($out/bin/bcachefs version)" = "${version}"

                  runHook postInstallCheck
                '';
              }
            );

          mkPackages =
            name: systems:
            let
              packagesForSystem =
                crossSystem:
                let
                  localSystem = system;
                  pkgs' = import nixpkgs {
                    inherit crossSystem localSystem;
                    overlays = myOverlays;
                  };

                  common = pkgs'.callPackage mkCommon { inherit crane; };
                  package = pkgs'.callPackage mkPackage { inherit common name; };
                  packageFuse = package.overrideAttrs (
                    final: prev: {
                      makeFlags = prev.makeFlags ++ [ "BCACHEFS_FUSE=1" ];
                      buildInputs = prev.buildInputs ++ [ pkgs'.fuse3 ];
                    }
                  );
                in
                [
                  (lib.nameValuePair "${name}-${crossSystem}" package)
                  (lib.nameValuePair "${name}-fuse-${crossSystem}" packageFuse)
                ];
            in
            lib.listToAttrs (lib.flatten (map packagesForSystem systems));
        in
        {
          packages =
            let
              inherit (cargoToml.package) name;
            in
            (mkPackages name systems)
            // {
              ${name} = config.packages."${name}-${system}";
              "${name}-fuse" = config.packages."${name}-fuse-${system}";
              default = config.packages.${name};
            };

          checks.cargo-clippy = common.craneLib.cargoClippy (
            common.args
            // {
              inherit (common) cargoArtifacts;
              cargoClippyExtraArgs = "--all-targets -- --deny warnings";
            }
          );

          # we have to build our own `craneLib.cargoTest`
          checks.cargo-test = common.craneLib.mkCargoDerivation (
            common.args
            // {
              inherit (common) cargoArtifacts;
              doCheck = true;

              enableParallelChecking = true;

              pnameSuffix = "-test";
              buildPhaseCargoCommand = "";
              checkPhaseCargoCommand = ''
                make ''${enableParallelChecking:+-j''${NIX_BUILD_CORES}} $makeFlags libbcachefs.a
                cargo test --profile release -- --nocapture
              '';
            }
          );

          # cargo clippy with the current minimum supported rust version
          # according to Cargo.toml
          checks.msrv =
            let
              rustVersion = cargoToml.package.rust-version;
              common = pkgs.callPackage mkCommon { inherit crane rustVersion; };
            in
            common.craneLib.cargoClippy (
              common.args
              // {
                pname = "msrv";
                inherit (common) cargoArtifacts;
                cargoClippyExtraArgs = "--all-targets -- --deny warnings";
              }
            );

          checks."32-bit" = config.packages."${cargoToml.package.name}-i686-linux";

          devShells.default = pkgs.mkShell {
            inputsFrom = [
              config.packages.default
              config.treefmt.build.devShell
            ];

            # here go packages that aren't required for builds but are used for
            # development, and might need to be version matched with build
            # dependencies (e.g. clippy or rust-analyzer).
            packages = with pkgs; [
              bear
              cargo-audit
              cargo-outdated
              clang-tools
              (rust-bin.stable.latest.minimal.override {
                extensions = [
                  "rust-analyzer"
                  "rust-src"
                ];
              })
            ];
          };

          treefmt.config = {
            projectRootFile = "flake.nix";
            flakeCheck = false;

            programs = {
              nixfmt.enable = true;
              rustfmt.edition = rustfmtToml.edition;
              rustfmt.enable = true;
              rustfmt.package = pkgs.rust-bin.selectLatestNightlyWith (toolchain: toolchain.rustfmt);
            };
          };
        };
    };
}
