self': {
  name = "bcachefs-nixos";

  nodes.machine = { config, pkgs, ... }: {
    boot.kernelPackages = pkgs.linuxPackages_latest;

    assertions = [{
      assertion =
        config.boot.bcachefs.modulePackage or null == self'.packages.bcachefs-module-linux-latest;
      message = "Local bcachefs module isn't being used; update nixpkgs?";
    }];

    virtualisation.emptyDiskImages = [{
      size = 4096;
      driveConfig.deviceExtraOpts.serial = "test-disk";
    }];

    boot.supportedFilesystems.bcachefs = true;
    boot.bcachefs.package = self'.packages.bcachefs-tools;
  };

  testScript = ''
    machine.wait_for_unit("multi-user.target")

    with subtest("basic roundtrip without compression"):
      machine.succeed(
        "mkfs.bcachefs /dev/disk/by-id/virtio-test-disk",
        "mkdir -p /mnt",
        "mount /dev/disk/by-id/virtio-test-disk /mnt",
        "echo hello > /mnt/test.txt",
        "cat /mnt/test.txt | grep hello",
      )
      machine.succeed("umount /mnt")

    with subtest("remount and verify"):
      machine.succeed(
        "mount -t bcachefs /dev/disk/by-id/virtio-test-disk /mnt",
        "cat /mnt/test.txt | grep hello",
        "umount /mnt",
      )

    with subtest("zstd compresses compressible data"):
      machine.succeed(
        "mkfs.bcachefs --force --compression=zstd /dev/disk/by-id/virtio-test-disk",
        "mount /dev/disk/by-id/virtio-test-disk /mnt",
      )
      compression = machine.succeed("cat /sys/fs/bcachefs/*/options/compression").strip()
      assert compression == "zstd", f"expected zstd, got {compression}"
      machine.succeed("dd if=/dev/zero bs=1M count=4 of=/mnt/compressible 2>&1")
      machine.succeed("sync")
      usage = machine.succeed("bcachefs fs usage -a /mnt")
      print(f"fs usage:\n{usage}")
      found_zstd = False
      for line in usage.splitlines():
        if "zstd" in line and "compressed" not in line:
          parts = line.split()
          compressed = int(parts[1])
          uncompressed = int(parts[2])
          ratio = compressed / uncompressed
          print(f"compressed={compressed} uncompressed={uncompressed} ratio={ratio:.4f}")
          assert compressed < uncompressed, f"compressible data not compressed: {compressed} >= {uncompressed}"
          assert ratio < 0.1, f"compression ratio too high: {ratio:.2f}"
          found_zstd = True
          break
      assert found_zstd, "no zstd compression line in fs usage output"
      machine.succeed("md5sum /mnt/compressible > /tmp/compressible.md5")
      machine.succeed("md5sum -c /tmp/compressible.md5")
      machine.succeed("umount /mnt")

    with subtest("zstd early abort skips incompressible data"):
      machine.succeed(
        "mkfs.bcachefs --force --compression=zstd /dev/disk/by-id/virtio-test-disk",
        "mount /dev/disk/by-id/virtio-test-disk /mnt",
      )
      machine.succeed("dd if=/dev/urandom bs=1M count=4 of=/mnt/incompressible 2>&1")
      machine.succeed("sync")
      usage = machine.succeed("bcachefs fs usage -a /mnt")
      print(f"fs usage:\n{usage}")
      found = False
      for line in usage.splitlines():
        if "zstd" in line and "compressed" not in line:
          found = True
          parts = line.split()
          compressed = int(parts[1])
          uncompressed = int(parts[2])
          assert compressed == uncompressed, f"random data should not compress: {compressed} != {uncompressed}"
          break
        if "incompressible" in line and "compressed" not in line:
          found = True
          parts = line.split()
          compressed = int(parts[1])
          uncompressed = int(parts[2])
          assert compressed == uncompressed, f"incompressible data should be 1:1: {compressed} != {uncompressed}"
          break
      assert found, "no compression accounting line in fs usage output"
      machine.succeed("md5sum /mnt/incompressible > /tmp/incompressible.md5")
      machine.succeed("md5sum -c /tmp/incompressible.md5")
      machine.succeed("umount /mnt")

    with subtest("zstd mixed data integrity"):
      machine.succeed(
        "mkfs.bcachefs --force /dev/disk/by-id/virtio-test-disk",
        "mount /dev/disk/by-id/virtio-test-disk /mnt",
        "echo zstd > /sys/fs/bcachefs/*/options/compression",
      )
      machine.succeed(
        "dd if=/dev/zero bs=1K count=4 > /mnt/small-compressible",
        "dd if=/dev/urandom bs=1K count=4 > /mnt/small-incompressible",
        "dd if=/dev/zero bs=1M count=4 > /mnt/large-compressible",
        "dd if=/dev/urandom bs=1M count=4 > /mnt/large-incompressible",
        "md5sum /mnt/small-compressible /mnt/small-incompressible /mnt/large-compressible /mnt/large-incompressible > /tmp/all.md5",
        "md5sum -c /tmp/all.md5",
      )
      machine.succeed("umount /mnt")
  '';
}
