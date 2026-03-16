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
      systems = nixpkgs.lib.filter (s: nixpkgs.lib.hasSuffix "-linux" s) nixpkgs.lib.systems.flakeExposed;

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
              self.packages.${pkgs.stdenv.hostPlatform.system}.bcachefs-tools;
          };
        in {
          default = bcachefsNixosModule;
          bcachefs = bcachefsNixosModule;
        };
      };

      inherit systems;

      flake.overlays.default = import ./overlay.nix { inherit inputs version; };

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
            overlays = [ (import rust-overlay) ];
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
                    overlays = [
                      (import rust-overlay)
                      self.overlays.default
                    ];
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
              doc = pkgs.stdenv.mkDerivation {
                pname = "bcachefs-tools-doc";
                inherit version;
                src = ./doc;
                buildInputs = with pkgs; [
                  latexDerivation
                ];
                buildPhase = ''
                  pdflatex bcachefs-principles-of-operation.tex
                  pdflatex bcachefs-principles-of-operation.tex
                '';
                installPhase = ''
                  mkdir -p $out/doc
                  cp bcachefs-principles-of-operation.pdf $out/doc
                '';
              };
            };

          checks = {
            inherit (self'.packages)
              bcachefs-tools
              bcachefs-tools-aarch64-linux
              bcachefs-tools-fuse
              bcachefs-tools-fuse-i686-linux
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

            nixos-test = pkgs.testers.nixosTest (import ./nixos-test.nix self');
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
