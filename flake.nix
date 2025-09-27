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
    in
    flake-parts.lib.mkFlake { inherit inputs; } {
      imports = [ inputs.treefmt-nix.flakeModule ];

      flake = {
        githubActions = nix-github-actions.lib.mkGithubMatrix {
          # github actions supports fewer architectures
          checks = nixpkgs.lib.getAttrs [ "aarch64-linux" "x86_64-linux" ] self.checks;
        };
      };

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
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ (import rust-overlay) ];
          };

          cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
          rustfmtToml = builtins.fromTOML (builtins.readFile ./rustfmt.toml);

          rev = self.shortRev or self.dirtyShortRev or (lib.substring 0 8 self.lastModifiedDate);
          version = "${cargoToml.package.version}+${rev}";

          # this is somewhat cursed
          mkosiFhs =
            let
              keyringDeb = pkgs.fetchurl {
                url = "https://deb.debian.org/debian/pool/main/d/debian-archive-keyring/debian-archive-keyring_2025.1_all.deb";
                hash = "sha256-nqd3jkQxRMpJBmhzeoqyLdPnSLuZ6AXiLsBVq+s8f6w=";
              };
              debian-archive-keyring =
                pkgs.runCommandNoCCLocal "debian-archive-keyring"
                  {
                    nativeBuildInputs = with pkgs; [ binutils ];
                  }
                  ''
                    ar vx ${keyringDeb}
                    mkdir "$out"
                    tar -xf data.tar.xz --transform "s|/usr||" -C "$out"
                  '';
            in
            pkgs.buildFHSEnv {
              name = "mkosi";
              runScript = "mkosi";
              targetPkgs =
                pkgs: with pkgs; [
                  (mkosi.overrideAttrs (
                    final: prev: {
                      patches = prev.patches ++ [ ./mkosi/0001-Use-apt-ftparchive-instead-of-reprepro.patch ];
                    }
                  ))
                  apt
                  debian-archive-keyring
                  dpkg
                  e2fsprogs
                  gnupg
                  jq
                  python3
                ];
            };
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
                    overlays = [ (import rust-overlay) ];
                  };
                  withCrossName =
                    set: lib.mapAttrs' (name: value: lib.nameValuePair "${name}-${crossSystem}" value) set;

                  craneBuild = pkgs'.callPackage ./crane-build.nix { inherit crane version; };
                  crossPackages = {
                    "bcachefs-tools" = craneBuild.package;
                    "bcachefs-tools-fuse" = craneBuild.packageFuse;
                  };
                in
                (withCrossName crossPackages) // lib.optionalAttrs (crossSystem == localSystem) crossPackages;
              packages = lib.mergeAttrsList (map packagesForSystem systems);
            in
            packages
            // {
              default = self'.packages.${cargoToml.package.name};
            };

          checks = {
            inherit (self'.packages)
              bcachefs-tools
              bcachefs-tools-aarch64-linux
              bcachefs-tools-fuse
              bcachefs-tools-fuse-i686-linux
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
              mkosiFhs
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
