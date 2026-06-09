self': {
  name = "bcachefs-nixos";

  nodes.machine = { config, pkgs, ... }: {
    boot.kernelPackages = pkgs.linuxPackages_latest;

    # Build the DKMS module with BCACHEFS_TESTS=1 so the in-kernel
    # compression test hooks (test_zstd_*, test_mt_concurrency,
    # test_mt_compress_*, test_mt_levels) are compiled in and reachable
    # through /sys/fs/bcachefs/*/compress_test.
    boot.bcachefs.modulePackage =
      self'.packages.bcachefs-module-linux-latest.overrideAttrs (old: {
        makeFlags = (old.makeFlags or []) ++ [ "BCACHEFS_TESTS=1" ];
      });

    assertions = [{
      assertion =
        config.boot.bcachefs.modulePackage or null != null;
      message = "bcachefs module not set";
    }];

    # MT compression path requires more than one CPU.  nixos test VMs
    # default to 1 vCPU, in which case bch2_compress_nr_workers() returns
    # 1 and bch2_write_should_mt_compress() never fires.  Pin to 4 vCPUs
    # so the parallel dispatch branch is actually exercisable in the
    # kernel module.
    virtualisation.cores = 4;

    virtualisation.emptyDiskImages = [{
      size = 4096;
      driveConfig.deviceExtraOpts.serial = "test-disk";
    }];

    boot.supportedFilesystems.bcachefs = true;
    boot.bcachefs.package = self'.packages.bcachefs-tools;
  };

  testScript = ''
    machine.wait_for_unit("multi-user.target")

    with subtest("module built from local sources"):
      machine.succeed(
        "modinfo bcachefs | grep updates/src/fs/bcachefs > /dev/null",
        "mkfs.bcachefs /dev/disk/by-id/virtio-test-disk",
        "mkdir -p /mnt",
        "mount /dev/disk/by-id/virtio-test-disk /mnt",
        "umount /mnt",
      )

    with subtest("mt compress workers initialized"):
      # bch2_compress_workers module param defaults to 0 (= auto =
      # min(num_online_cpus(), 32); see fs/data/compress.c:55).  The
      # effective worker count is computed in bch2_compress_nr_workers()
      # at WQ init time, so the param value on its own doesn't prove the
      # pool was sized; we still need nproc >= 2 for MT to engage.
      param = machine.succeed("cat /sys/module/bcachefs/parameters/compress_workers").strip()
      print(f"compress_workers module param: {param}")
      nproc = int(machine.succeed("nproc").strip())
      print(f"nproc: {nproc}")
      assert nproc >= 2, f"need >= 2 vCPUs for MT path, got {nproc}"

      machine.succeed(
        "mkfs.bcachefs --force --compression=zstd /dev/disk/by-id/virtio-test-disk",
        "mount /dev/disk/by-id/virtio-test-disk /mnt",
      )

      # bch2_compress_wq_init() is called from bch2_fs_compress_init()
      # at mount time.  The only init-time log emitted is the failure
      # path's pr_notice(); success is silent.  Assert the failure
      # message is absent, which proves the WQ was allocated and the
      # per-worker workspace + dst + verify buffers were kmalloc'd.
      dmesg = machine.succeed("dmesg")
      print(f"dmesg tail:\n{dmesg[-2000:]}")
      assert "MT compression workqueue init failed" not in dmesg, (
        "MT compression WQ init failed; check dmesg for details")

      machine.succeed("umount /mnt")

    with subtest("mt compression roundtrip"):
      # 128 MiB of /dev/zero.  encoded_extent_max is 256 KiB (see
      # fs/opts.h:172), so this write produces 512 chunks and the MT
      # dispatch branch (bch2_write_should_mt_compress) returns true.
      machine.succeed(
        "mkfs.bcachefs --force --compression=zstd /dev/disk/by-id/virtio-test-disk",
        "mount /dev/disk/by-id/virtio-test-disk /mnt",
      )
      machine.succeed(
        "dd if=/dev/zero of=/tmp/src-mt bs=1M count=128 2>&1",
        "cp /tmp/src-mt /mnt/mt-zeros",
        "sync",
      )
      machine.succeed("cmp /tmp/src-mt /mnt/mt-zeros")
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
      machine.succeed("umount /mnt")

      machine.succeed("mount /dev/disk/by-id/virtio-test-disk /mnt")
      machine.succeed("cmp /tmp/src-mt /mnt/mt-zeros")
      machine.succeed("umount /mnt")

    with subtest("mt compression workers concurrent"):
      # test_mt_concurrency (fs/debug/compress_test.c:229) submits N
      # trivial work items that each sleep 10 ms, then walks the
      # recorded start/end timestamps and counts pairs whose intervals
      # overlap.  With truly parallel workers, overlap_count > 0; with
      # a serial fallback, overlap_count = 0 and the test returns
      # -EIO.  The test emits two pr_info()s: a "mt concurrency test:
      # ..." header and a "mt concurrency test passed: %u overlapping
      # pairs out of %u" footer; the framework adds a "compress test
      # test_mt_concurrency passed" line on success.
      machine.succeed(
        "mkfs.bcachefs --force --compression=zstd /dev/disk/by-id/virtio-test-disk",
        "mount /dev/disk/by-id/virtio-test-disk /mnt",
      )
      dmesg_before = int(machine.succeed("dmesg | wc -l").strip())
      machine.succeed(
        "echo 'test_mt_concurrency 1' > /sys/fs/bcachefs/*/compress_test",
      )
      # The kernel test returns synchronously after drain_workqueue;
      # let the console buffer flush before reading dmesg.
      machine.wait_for_unit("multi-user.target")
      dmesg_after = machine.succeed("dmesg").splitlines()[dmesg_before:]
      dmesg_new = "\n".join(dmesg_after)
      print(f"dmesg after test_mt_concurrency:\n{dmesg_new}")
      assert "mt concurrency test" in dmesg_new, (
        f"no 'mt concurrency test' marker in dmesg:\n{dmesg_new}")
      assert "overlapping pairs" in dmesg_new, (
        f"no 'overlapping pairs' report in dmesg:\n{dmesg_new}")
      assert "FAILED" not in dmesg_new, (
        f"test_mt_concurrency reported FAILED:\n{dmesg_new}")
      machine.succeed("umount /mnt")

    with subtest("mt compression ratio"):
      # 256 MiB of zeros.  Same dispatch path as the roundtrip subtest,
      # but the larger volume exercises the per-worker workspace more
      # aggressively and makes the zstd dictionary convergence visible
      # in fs usage.  Uses 256 MiB instead of 1 GiB to fit within the
      # NixOS test VM's root filesystem.
      machine.succeed(
        "mkfs.bcachefs --force --compression=zstd /dev/disk/by-id/virtio-test-disk",
        "mount /dev/disk/by-id/virtio-test-disk /mnt",
      )
      machine.succeed(
        "dd if=/dev/zero of=/tmp/src-mt-ratio bs=1M count=256 2>&1",
        "cp /tmp/src-mt-ratio /mnt/mt-ratio",
        "sync",
      )
      usage = machine.succeed("bcachefs fs usage -a /mnt")
      print(f"fs usage:\n{usage}")
      found = False
      for line in usage.splitlines():
        if "zstd" in line and "compressed" not in line:
          parts = line.split()
          compressed = int(parts[1])
          uncompressed = int(parts[2])
          ratio = compressed / uncompressed
          print(f"compressed={compressed} uncompressed={uncompressed} ratio={ratio:.4f}")
          # All-zeros streams compress to a handful of KiB regardless of
          # worker count; the ratio may be slightly higher than with 1 GiB
          # due to fixed metadata overhead.
          assert ratio < 0.02, f"zeros compression ratio too high: {ratio:.4f}"
          found = True
          break
      assert found, "no zstd compression line in fs usage output"
      machine.succeed("cmp /tmp/src-mt-ratio /mnt/mt-ratio")
      machine.succeed("umount /mnt")

      machine.succeed("mount /dev/disk/by-id/virtio-test-disk /mnt")
      machine.succeed("cmp /tmp/src-mt-ratio /mnt/mt-ratio")
      machine.succeed("umount /mnt")

    with subtest("mt stress test"):
      # Format + write 5 times in a row, mixing compressible and
      # incompressible extents so the MT dispatch path has to interleave
      # the two completion queues.  fsck after each cycle so a silent
      # metadata corruption from the new code paths would fail the test.
      machine.succeed(
        "dd if=/dev/zero of=/tmp/src-mt-zero bs=1M count=128 2>&1",
        "dd if=/dev/urandom of=/tmp/src-mt-rand bs=1M count=128 2>&1",
      )
      for i in range(5):
        print(f"mt stress cycle {i+1}/5")
        machine.succeed(
          "mkfs.bcachefs --force --compression=zstd /dev/disk/by-id/virtio-test-disk",
          "mount /dev/disk/by-id/virtio-test-disk /mnt",
          "cp /tmp/src-mt-zero /mnt/mt-zero",
          "cp /tmp/src-mt-rand /mnt/mt-rand",
          "sync",
        )
        machine.succeed(
          "cmp /tmp/src-mt-zero /mnt/mt-zero",
          "cmp /tmp/src-mt-rand /mnt/mt-rand",
          "umount /mnt",
          "bcachefs fsck /dev/disk/by-id/virtio-test-disk",
        )

    with subtest("mt small write fallback"):
      # Writes below encoded_extent_max (256 KiB) take the serial path;
      # verify the serial fallback round-trips correctly and produces a
      # compressed extent.  The 4 KiB write is 64x below the MT threshold.
      machine.succeed(
        "mkfs.bcachefs --force --compression=zstd /dev/disk/by-id/virtio-test-disk",
        "mount /dev/disk/by-id/virtio-test-disk /mnt",
      )
      machine.succeed(
        "dd if=/dev/zero of=/tmp/src-mt-small bs=4K count=1 2>&1",
        "cp /tmp/src-mt-small /mnt/mt-small",
        "sync",
      )
      machine.succeed("cmp /tmp/src-mt-small /mnt/mt-small")
      machine.succeed("umount /mnt")

      machine.succeed("mount /dev/disk/by-id/virtio-test-disk /mnt")
      machine.succeed("cmp /tmp/src-mt-small /mnt/mt-small")
      machine.succeed("umount /mnt")
  '';
}
