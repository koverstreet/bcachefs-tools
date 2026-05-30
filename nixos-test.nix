self': {
  name = "bcachefs-nixos";

  nodes.machine =
    { config, pkgs, ... }:
    {
      # modulePackage defaults to the module built for boot.kernelPackages;
      # pin to linuxPackages_latest so it matches bcachefs-module-linux-latest
      # (the kernel we ship that module for) and the assertion below compares
      # like-for-like. The default kernel lags linuxPackages_latest, which made
      # the assertion spuriously fail.
      boot.kernelPackages = pkgs.linuxPackages_latest;

      assertions = [
        {
          assertion =
            config.boot.bcachefs.modulePackage or null == self'.packages.bcachefs-module-linux-latest;
          message = "Local bcachefs module isn't being used; update nixpkgs?";
        }
      ];

      virtualisation.emptyDiskImages = [
        {
          size = 4096;
          driveConfig.deviceExtraOpts.serial = "test-disk";
        }
      ];

      boot.supportedFilesystems.bcachefs = true;
      boot.bcachefs.package = self'.packages.bcachefs-tools;
    };

  testScript = ''
    machine.succeed(
      "modinfo bcachefs | grep updates/src/fs/bcachefs > /dev/null",
      "mkfs.bcachefs /dev/disk/by-id/virtio-test-disk",
      "mkdir /mnt",
      "mount /dev/disk/by-id/virtio-test-disk /mnt",
    )
  '';
}
