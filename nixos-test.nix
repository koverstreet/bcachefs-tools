self': {
  name = "bcachefs-nixos";

  nodes.machine = { config, pkgs, ... }: {
    boot.kernelPackages = pkgs.linuxPackages_latest;

    boot.bcachefs.modulePackage =
      self'.packages.bcachefs-module-linux-latest.overrideAttrs (old: {
        makeFlags = (old.makeFlags or []) ++ [ "BCACHEFS_TESTS=1" ];
      });

    assertions = [{
      assertion =
        config.boot.bcachefs.modulePackage or null != null;
      message = "bcachefs module not set";
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
      machine.succeed("dd if=/dev/zero bs=1M count=4 of=/tmp/src-compressible 2>&1")
      machine.succeed("cp /tmp/src-compressible /mnt/compressible")
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
      machine.succeed("cmp /tmp/src-compressible /mnt/compressible")
      machine.succeed("umount /mnt")

      machine.succeed("mount /dev/disk/by-id/virtio-test-disk /mnt")
      machine.succeed("cmp /tmp/src-compressible /mnt/compressible")
      machine.succeed("umount /mnt")

    with subtest("zstd early abort skips incompressible data"):
      machine.succeed(
        "mkfs.bcachefs --force --compression=zstd /dev/disk/by-id/virtio-test-disk",
        "mount /dev/disk/by-id/virtio-test-disk /mnt",
      )
      machine.succeed("dd if=/dev/urandom bs=1M count=4 of=/tmp/src-incompressible 2>&1")
      machine.succeed("cp /tmp/src-incompressible /mnt/incompressible")
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
      machine.succeed("cmp /tmp/src-incompressible /mnt/incompressible")
      machine.succeed("umount /mnt")

      machine.succeed("mount /dev/disk/by-id/virtio-test-disk /mnt")
      machine.succeed("cmp /tmp/src-incompressible /mnt/incompressible")
      machine.succeed("umount /mnt")

    with subtest("in-kernel zstd compress tests"):
      machine.succeed(
        "mkfs.bcachefs --force --compression=zstd /dev/disk/by-id/virtio-test-disk",
        "mount /dev/disk/by-id/virtio-test-disk /mnt",
      )
      for test in ["test_zstd_compress_decompress", "test_zstd_early_abort_incompressible", "test_zstd_levels"]:
        machine.succeed(f"echo '{test} 10' > /sys/fs/bcachefs/*/compress_test")
      machine.succeed("umount /mnt")

    with subtest("zstd mixed data integrity"):
      machine.succeed(
        "mkfs.bcachefs --force /dev/disk/by-id/virtio-test-disk",
        "mount /dev/disk/by-id/virtio-test-disk /mnt",
        "echo zstd > /sys/fs/bcachefs/*/options/compression",
      )
      machine.succeed(
        "dd if=/dev/zero bs=1K count=4 of=/tmp/src-small-compressible 2>&1",
        "dd if=/dev/urandom bs=1K count=4 of=/tmp/src-small-incompressible 2>&1",
        "dd if=/dev/zero bs=1M count=4 of=/tmp/src-large-compressible 2>&1",
        "dd if=/dev/urandom bs=1M count=4 of=/tmp/src-large-incompressible 2>&1",
      )
      machine.succeed(
        "cp /tmp/src-small-compressible /mnt/small-compressible",
        "cp /tmp/src-small-incompressible /mnt/small-incompressible",
        "cp /tmp/src-large-compressible /mnt/large-compressible",
        "cp /tmp/src-large-incompressible /mnt/large-incompressible",
      )
      machine.succeed("sync")
      machine.succeed(
        "cmp /tmp/src-small-compressible /mnt/small-compressible",
        "cmp /tmp/src-small-incompressible /mnt/small-incompressible",
        "cmp /tmp/src-large-compressible /mnt/large-compressible",
        "cmp /tmp/src-large-incompressible /mnt/large-incompressible",
      )
      machine.succeed("umount /mnt")

      machine.succeed("mount /dev/disk/by-id/virtio-test-disk /mnt")
      machine.succeed(
        "cmp /tmp/src-small-compressible /mnt/small-compressible",
        "cmp /tmp/src-small-incompressible /mnt/small-incompressible",
        "cmp /tmp/src-large-compressible /mnt/large-compressible",
        "cmp /tmp/src-large-incompressible /mnt/large-incompressible",
      )
      machine.succeed("umount /mnt")
  '';
}
