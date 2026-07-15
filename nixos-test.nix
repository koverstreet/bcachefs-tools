self': {
  name = "bcachefs-nixos";

  nodes.machine =
    { config, ... }:
    {
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
