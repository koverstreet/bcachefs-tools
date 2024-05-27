{
  description = "Userspace tools for bcachefs";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";

    flake-parts.url = "github:hercules-ci/flake-parts";

    treefmt-nix = {
      url = "github:numtide/treefmt-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    fenix = {
      url = "github:nix-community/fenix";
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
      fenix,
      flake-compat,
      ...
    }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      imports = [ inputs.treefmt-nix.flakeModule ];

      # can be extended, but these have proper binary cache support in nixpkgs
      # as of writing.
      systems = [
        "aarch64-linux"
        "x86_64-linux"
      ];

      perSystem =
        {
          self',
          config,
          pkgs,
          system,
          ...
        }:
        let
          rustfmtToml = builtins.fromTOML (builtins.readFile ./rustfmt.toml);
        in
        {
          packages.default = config.packages.bcachefs-tools;
          packages.bcachefs-tools = pkgs.callPackage ./build.nix { };

          packages.bcachefs-tools-fuse = config.packages.bcachefs-tools.override { fuseSupport = true; };

          devShells.default = pkgs.mkShell {
            inputsFrom = [
              config.packages.default
              config.treefmt.build.devShell
            ];

            LIBCLANG_PATH = "${pkgs.clang.cc.lib}/lib";

            # here go packages that aren't required for builds but are used for
            # development, and might need to be version matched with build
            # dependencies (e.g. clippy or rust-analyzer).
            packages = with pkgs; [
              cargo-audit
              cargo-outdated
              clang-tools
              clippy
              rust-analyzer
            ];
          };

          treefmt.config = {
            projectRootFile = "flake.nix";

            programs = {
              nixfmt-rfc-style.enable = true;
              rustfmt.edition = rustfmtToml.edition;
              rustfmt.enable = true;
              rustfmt.package = fenix.packages.${system}.default.rustfmt;
            };
          };
        };
    };
}
