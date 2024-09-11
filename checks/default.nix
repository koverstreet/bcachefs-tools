{ pkgs }:
let
  inherit (builtins) baseNameOf readDir;
  inherit (pkgs.lib)
    filterAttrs
    genAttrs
    hasSuffix
    mapAttrsToList
    removeSuffix
    ;

  scriptName = shFile: removeSuffix ".sh" (baseNameOf shFile);

  scriptNames = mapAttrsToList (n: v: scriptName n) (
    filterAttrs (n: v: v == "regular" && hasSuffix ".sh" n) (readDir ./.)
  );

  mkTest =
    name:
    pkgs.testers.runNixOSTest {
      inherit name;

      nodes.machine =
        { pkgs, ... }:
        {
          virtualisation.emptyDiskImages = [
            4096
            1024
          ];
          boot.supportedFilesystems = [ "bcachefs" ];
          boot.kernelPackages = pkgs.linuxPackages_latest;

          # Add any packages you need inside test scripts here
          environment.systemPackages = with pkgs; [
            f3
            genpass
            keyutils
          ];

          environment.variables = {
            BCACHEFS_LOG = "trace";
            RUST_BACKTRACE = "full";
          };
        };

      testScript = ''
        machine.succeed("modprobe bcachefs")
        machine.succeed("${./${name}.sh} 1>&2")
      '';
    };
in
genAttrs scriptNames mkTest
