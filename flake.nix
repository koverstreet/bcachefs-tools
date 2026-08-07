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

    nix-github-actions = {
      url = "github:nix-community/nix-github-actions";
      inputs.nixpkgs.follows = "nixpkgs";
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
      nix-github-actions,
    }:
    let
      # i686-linux dropped: no real consumers, and cross sqlite tcltest
      # tries to run on i686 and fails.
      systems = nixpkgs.lib.filter
        (s: nixpkgs.lib.hasSuffix "-linux" s && s != "i686-linux")
        nixpkgs.lib.systems.flakeExposed;

      cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
      rustfmtToml = builtins.fromTOML (builtins.readFile ./rustfmt.toml);

      rev = self.shortRev or self.dirtyShortRev or (nixpkgs.lib.substring 0 8 self.lastModifiedDate);
      version = "${cargoToml.package.version}+${rev}";
    in
    flake-parts.lib.mkFlake { inherit inputs; } {
      imports = [ inputs.treefmt-nix.flakeModule ];

      flake = {
        githubActions = nix-github-actions.lib.mkGithubMatrix {
          # github actions supports fewer architectures
          checks = nixpkgs.lib.getAttrs [ "aarch64-linux" "x86_64-linux" ] self.checks;
        };
        nixosModules = let
          bcachefsNixosModule = { pkgs, ... }: {
            boot.supportedFilesystems = [ "bcachefs" ];
            boot.bcachefs.package =
              (pkgs.extend self.overlays.default).bcachefsPackages.bcachefs-tools;
          };
        in {
          default = bcachefsNixosModule;
          bcachefs = bcachefsNixosModule;
        };
      };

      inherit systems;

      flake.overlays.default = nixpkgs.lib.composeManyExtensions [
        (import rust-overlay)
        (import ./overlay.nix { inherit inputs version; })
      ];

      perSystem =
        {
          self',
          config,
          lib,
          system,
          ...
        }:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ self.overlays.default ];
          };
          latexDerivation = (
            pkgs.texliveBasic.withPackages (
              ps: with ps; [
                imakeidx
                xkeyval
                upquote
                collection-fontsrecommended
              ]
            )
          );
        in
        {
          packages =
            let
              packagesForSystem =
                crossSystem:
                let
                  localSystem = system;
                  pkgs' = import nixpkgs {
                    inherit crossSystem localSystem;
                    overlays = [ self.overlays.default ];
                  };

                  withCrossName =
                    set: lib.mapAttrs' (name: value: lib.nameValuePair "${name}-${crossSystem}" value) set;
                in
                (withCrossName pkgs'.bcachefsPackages)
                // lib.optionalAttrs (crossSystem == localSystem) pkgs'.bcachefsPackages;
              packages = lib.mergeAttrsList (map packagesForSystem systems);
            in
            packages
            // {
              default = self'.packages.${cargoToml.package.name};
              # The whole repo is the source, not just ./doc: most of the
              # document is generated from it. bch-docgen extracts DOC_LATEX
              # blocks out of fs/**.{c,h,rs} and the option x-macros, and
              # `bcachefs _doc_gen` emits the CLI reference from the clap
              # definitions. Both locate the tree by walking up for fs/ and
              # write into doc/generated/, which is not checked in - so they
              # have to run here, from the source root, before pdflatex.
              doc = pkgs.stdenv.mkDerivation {
                pname = "bcachefs-tools-doc";
                inherit version;
                src = ./.;
                nativeBuildInputs = [
                  latexDerivation
                  pkgs.rustc
                  self'.packages.default
                ];
                buildPhase = ''
                  runHook preBuild

                  # bch-docgen is a dependency-free single file, so plain
                  # rustc builds it - no need to drag in the cargo workspace.
                  # _doc_gen runs first: it writes cli-reference.tex, and
                  # bch-docgen checks that every \bchdoc{} in the PoO resolves.
                  rustc -O doc/docgen/src/main.rs -o bch-docgen
                  bcachefs _doc_gen
                  ./bch-docgen
                  # As an argument, not a format string: printf would read the
                  # \r and \b in the LaTeX as carriage return and backspace.
                  printf '%s\n' '\renewcommand{\bchdocversion}{${version}}' \
                    > doc/generated/build-version.tex

                  # Twice, to resolve the table of contents and cross-references:
                  for i in 1 2; do
                    pdflatex -interaction=nonstopmode -halt-on-error \
                      doc/bcachefs-principles-of-operation.tex
                  done

                  runHook postBuild
                '';
                # share/doc, not doc: stdenv's move-docs hook relocates $out/doc
                # anyway, so name the real path rather than let it be moved.
                installPhase = ''
                  runHook preInstall
                  mkdir -p $out/share/doc
                  cp bcachefs-principles-of-operation.pdf $out/share/doc
                  runHook postInstall
                '';
              };
            };

          checks = {
            inherit (pkgs.bcachefsPackages)
              bcachefs-tools
              bcachefs-tools-fuse
              bcachefs-module-linux-latest
              bcachefs-module-linux-testing
              ;
            inherit (pkgs.callPackage ./crane-build.nix { inherit crane version; })
              # cargo-clippy
              cargo-test
              ;

            # cargo clippy with the current minimum supported rust version
            # according to Cargo.toml
            msrv =
              let
                rustVersion = cargoToml.package.rust-version;
                craneBuild = pkgs.callPackage ./crane-build.nix { inherit crane rustVersion version; };
              in
              craneBuild.cargo-test.overrideAttrs (
                final: prev: {
                  pname = "${prev.pname}-msrv";
                }
              );

            # The test derivation hardcodes "kvm" into requiredSystemFeatures
            # for any Linux test, which GitHub's hosted aarch64 runners don't
            # provide — so it won't schedule there. Strip it via
            # overrideTestDerivation (overrideAttrs for the test): qemu.forceAccel
            # defaults to false, so the driver falls back to TCG emulation when
            # /dev/kvm is absent (KVM used where available, emulated otherwise).
            nixos-test =
              (pkgs.testers.nixosTest (import ./nixos-test.nix self')).overrideTestDerivation
                (_: prev: {
                  requiredSystemFeatures = lib.remove "kvm" (prev.requiredSystemFeatures or [ ]);
                });
          };

          devShells.default = pkgs.mkShell {
            inputsFrom = [
              config.treefmt.build.devShell
              self'.packages.default
            ];

            # here go packages that aren't required for builds but are used for
            # development, and might need to be version matched with build
            # dependencies (e.g. clippy or rust-analyzer).
            packages = with pkgs; [
              bear
              rust-bindgen
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

          devShells.doc = pkgs.mkShell {
            packages = with pkgs; [
              latexDerivation
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
