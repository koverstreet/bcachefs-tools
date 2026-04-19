{ inputs, version }:
let
  rustOverlay = import inputs.rust-overlay;
in
final: prev:
let
  pkgs = prev.extend rustOverlay;
  craneBuild = pkgs.callPackage ./crane-build.nix {
    inherit version;
    inherit (inputs) crane;
  };
in
{
  bcachefsPackages = {
    "bcachefs-tools" = craneBuild.package;
    "bcachefs-tools-fuse" = craneBuild.packageFuse;
    "bcachefs-module-linux-latest" =
      final.linuxPackages_latest.callPackage craneBuild.package.kernelModule
        { };
    "bcachefs-module-linux-testing" =
      final.linuxPackages_testing.callPackage craneBuild.package.kernelModule
        { };
  };
}
