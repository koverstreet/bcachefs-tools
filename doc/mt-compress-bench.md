# MT compression micro-benchmark

The MT compression workqueue in `fs/data/compress_workers.c` is supposed
to dispatch N independent compressions to a pool of `WQ_UNBOUND` workers
in parallel.  Before this benchmark existed, the only verification was a
correctness smoke test (`debian/tests/kernel-smoke-test.05.single-mt-
compression`) that wrote multi-extent data and checked md5s.  That
proved the MT path produced the right bytes; it did not prove the
dispatch was actually parallel.

This benchmark closes that gap: it runs the same N compressions twice
on the same source data, once serially on the calling thread and once
through the WQ, and asserts the WQ path is meaningfully faster.

## Where the code lives

| File | Role |
|------|------|
| `fs/debug/mt_compress_bench.c` | The benchmark itself.  Allocates N buffers, runs the serial and parallel paths back-to-back, emits results to dmesg with a stable `MT_COMPRESS_BENCH:` prefix. |
| `fs/debug/sysfs.c` | Wires `sysfs_mt_compress_bench` (a write-only sysfs attribute under `/sys/fs/bcachefs/<uuid>/`) to `bch2_compress_bench()`.  Payload is the compression opt (`"lz4"`, `"zstd:3"`, `"gzip:6"`, ...). |
| `fs/debug/tests.h` | Declares `bch2_compress_bench()`. |
| `fs/Makefile` | Compiles the bench into the module under `BCACHEFS_TESTS=1`. |
| `debian/tests/kernel-smoke-test.06.single-mt-compress-bench` | The CI driver: sets up a loop device, mounts, runs the bench for lz4/zstd:3/gzip:6, asserts speedup. |
| `tests/mt_compress_bench.sh` | Standalone driver: runnable by hand outside the debian/tests framework. |

## What it actually measures

For each requested codec the bench does the following:

1. Allocates `N = min(8, 2 * nr_workers)` source buffers of
   `min(256 KiB, encoded_extent_max)` bytes, filled with a repeating
   text pattern (highly compressible; a pure-zero buffer would hit a
   fast path that doesn't parallelize the same way).

2. **Warm-up**: one serial run + one parallel run, untimed.  This
   faults in the workers' zstd workspaces and the page cache so the
   timed runs are representative.

3. **Timed runs**: `BENCH_NR_ITERS` (4) iterations of:
   - `bench_one_serial()`: N calls to `bch2_compress_locked()` on the
     calling thread, each borrowing a worker workspace / verify_buf so
     serial and parallel exercise the same per-codec state.
   - `bench_one_parallel()`: N `bch2_compress_wq_submit()` calls
     against a parent closure, blocked on `closure_sync()`.

4. The wall time for each mode is summed across iterations and
   reported to dmesg.  The kernel-side check passes if
   `parallel_ns * 2 < serial_ns` on a system with `nr_workers > 1`.

5. On a system where the WQ came up with `< 2` workers (operator set
   `compress_workers=1`, or only 1 CPU online) the bench emits SKIP
   rather than FAIL: a strictly-serial WQ cannot parallelize, and
   failing on such a host would be testing the host, not the code.

## Running it by hand

```sh
# Build the module with the in-kernel tests enabled.
make BCACHEFS_TESTS=1
make install_dkms BCACHEFS_TESTS=1

# Mount any bcachefs filesystem and trigger the bench directly:
mount /dev/sdXn /mnt/bcachefs
SYSFS=$(ls -d /sys/fs/bcachefs/*/mt_compress_bench | head -1)
echo "zstd:3" | tee "$SYSFS"     # args: compression opt
dmesg | grep '^MT_COMPRESS_BENCH:'
# or use the wrapper:
tests/mt_compress_bench.sh lz4 zstd:9
```

The wrapper is also wired into the existing smoke-test framework
(`debian/tests/kernel-smoke-test.06.single-mt-compress-bench`), so a
normal autopkgtest run picks it up automatically.

## Expected numbers

On a recent multi-core x86 host with a single bcachefs filesystem,
zstd level 3 over 8 × 256 KiB chunks typically shows:

```
MT_COMPRESS_BENCH: opt=0x33 nr_workers=8 chunks=8 chunk_bytes=262144 iters=4
MT_COMPRESS_BENCH: serial_ns=38000000
MT_COMPRESS_BENCH: parallel_ns=6000000
MT_COMPRESS_BENCH: speedup=6.33x
MT_COMPRESS_BENCH: PASS - parallel >= 2x faster than serial
```

i.e. close to the theoretical 8x speedup once wall time is dominated
by codec work and the WQ is on `WQ_HIGHPRI` so it isn't preempted by
normal work.  On a single-CPU VM, the bench reports SKIP.

## Why a separate file (not folded into `compress_test.c`)

`fs/debug/compress_test.c` is the correctness suite: roundtrip
compress-decompress of zeros and random data, plus a synthetic
msleep-based concurrency test.  That last one proves the WQ *can* run
multiple works in parallel, but doesn't measure whether the actual
compression path goes through it.

The perf bench is small enough to live in its own file
(`fs/debug/mt_compress_bench.c`) and self-contained - it only depends
on the exported `bch2_compress_locked()` and `bch2_compress_wq_submit()`
APIs, not on `buf_uncompress()` which is currently `static` in
`fs/data/compress.c`.  Keeping the bench independent of the
correctness suite means the two can be added to the kernel tree at
different paces.
