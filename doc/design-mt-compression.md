# Multithreaded extent-level compression: implementation plan

**Date:** 2026-06-09
**Status:** RFC / design
**Branch:** `multithreaded_compression`
**Supersedes audit:** `.claude/notes/2026-06-09-mt-compression-audit.md` (kept
for context; identifies why a Rust-only approach was rejected).

## Goal

Parallelise compression of the multiple extents produced by a single
`bch_write_op` inside `bch2_write_extent` (`fs/data/write.c:1710`), so that
large writes (>1 chunk of `encoded_extent_max`) compress N chunks in parallel
across the available CPUs instead of one at a time.

This is a **C-side change** to bcachefs core under `fs/`. Per CLAUDE.md's
updated risk-calibration guidance and `doc/vendored-kernel-files.md`, `fs/` is
the canonical home for bcachefs after its removal from mainline at
v6.18-rc1. Same source feeds the userspace `bcachefs` binary and the DKMS
kernel module.

## Non-goals

- **Not** parallelising the move path (`fs/data/move.c`,
  `fs/data/update.c`). Move-path uses `BCH_WRITE_data_encoded` with a
  pre-existing CRC; the `op->nonce += src_len >> 9` advance at
  `fs/data/write.c:1817-1818` is on a shared `op->nonce` field. Racing
  threads can derive the same ChaCha20 (key, nonce) pair from different
  plaintexts — that's a confidentiality break. The move-path parallelism
  story is a separate design problem; the foreground write path is enough
  win on its own.
- **Not** changing the on-disk format. CRC entries, extent layout,
  encryption nonce derivation all stay byte-for-byte identical.
- **Not** changing the public write API (`bch2_write`,
  `bch2_write_op_init`).
- **Not** touching the Rust side. Compression is and stays pure C kernel
  code.

## Invariants the design must preserve

From `.claude/notes/2026-06-09-mt-compression-audit.md` and the follow-up
audits, the do-while loop body at `fs/data/write.c:1768` mutates *eight*
distinct pieces of shared state per iteration. The parallel design must
preserve every one of these:

1. **`op->nonce` advance.** Currently `op->nonce += src_len >> 9` per
   iteration. Path B (move/encoded path) racing on this field reuses
   ChaCha20 nonces → silent confidentiality break.
2. **`op->pos.offset` advance.** Running cursor into the file. Out-of-order
   advance corrupts file content layout.
3. **`op->insert_keys` keylist sortedness.** `bch2_verify_keylist_sorted`
   (`fs/data/keylist.c:43`) enforces strict ascending `k.p`.
4. **`op->version` assignment.** `atomic64_inc_return(&c->key_version)` —
   atomic, but version-to-position mapping must remain monotonic for
   recovery (`fs/btree/check.c:712`).
5. **`wp->sectors_free` decrement.** Held under `wp->lock` across the
   loop; the post-parallel design keeps the allocator interaction serial.
6. **`src` bio iterator non-destruction.** `bch2_bio_compress` currently
   swaps `src->bi_iter.bi_size` (`fs/data/compress.c:587-590`). Parallel
   workers cannot share one bio.
7. **Crash consistency.** Current COW invariant: "no window where a crash
   can leave partially-written data" (`fs/data/write.c:30-32`). The
   parallel design submits extents in submission order to the keylist; the
   journal's per-extent atomicity stays as today.
8. **Backpointer ordering.** Backpointers are inserted during
   `__bch2_write_index` consumption of the keylist — already serial in
   submission order if the keylist is correctly ordered.

The design preserves all eight by: doing **only** compress / encrypt /
checksum in parallel, with all identifier assignment (nonce, pos, version,
keylist slot) pre-computed in a *serial prefix* and consumed in submission
order in a *serial post-pass*.

## Two pivotal observations

These make the whole thing tractable:

### Observation 1: `uncompressed_size` is known upfront, not after compression

`op->pos.offset` advances by `crc.uncompressed_size`. That's the original
input size to that chunk, which is determined *before* compression by
clamping to `encoded_extent_max`. Same for `op->nonce` (advance by
`src_len >> 9` where `src_len` is the pre-compression input length). So
the per-chunk identifiers can all be assigned in a deterministic serial
prefix before any worker touches the data.

### Observation 2: bcachefs already does parallel fan-in via closures

`fs/btree/node_scan.c:299, 314` already does N kthread reads with a
closure-based fan-in (`closure_sync_unbounded`). Same pattern works for
compression workers. We don't need new synchronisation primitives.

## Architecture

### Worker pool: enhanced userspace workqueue + standard `alloc_workqueue` API

Two approaches were considered:

(a) Enhance `linux/workqueue.c` to be properly multi-worker, then create
    `c->compress.wq` via the standard `alloc_workqueue(...,
    WQ_UNBOUND | WQ_CPU_INTENSIVE | WQ_MEM_RECLAIM, 0)` call.

(b) Dedicated `bch2_compress_workers` struct with its own N kthreads and
    job queue, bypassing the workqueue API entirely.

**Decision: (a).** Reasons:

- Same code in kernel and userspace; no `#ifdef __KERNEL__`.
- Fixes the userspace shim's single-worker-per-WQ limitation for the 12
  other `alloc_workqueue` callers in the tree that currently get
  silently serialised (e.g. `btree_update_wq` at max_active=512, journal
  WQs at 512, btree-read-complete at 512). All transparently benefit.
- The kernel side already supports multi-worker properly; the shim is the
  gap.
- The compression dispatch (`queue_work` + closure fan-in) is the same
  shape as `fs/btree/node_scan.c:299-314`, which is already in tree.

The shim enhancement is **its own phase** with its own tests, landed before
the MT compression phase depends on it.

### Workspace storage: existing mempool, sized up

`c->compress.workspace[opt]` is an existing per-algorithm `mempool_t`
(`fs/data/compress.c:728`). The audit established that the mempool is
thread-safe but currently sized `min_nr=1`. The fix is to size it to
`num_online_cpus()` (capped at a configurable max) so each concurrent
worker gets a preallocated workspace without falling back to `kvmalloc`.

We do **not** pre-pin per-worker workspaces in `struct wq_worker`,
because:

- That wastes memory when compression is rare (workspaces stay allocated
  even when no compression is happening).
- The mempool already handles the concurrency correctly; per-worker
  pinning is a micro-optimisation, not a correctness requirement.

### Chunking: fixed-size at `encoded_extent_max`

Currently `bch2_bio_compress` consumes `min(src->bi_iter.bi_size,
encoded_extent_max)` per call (`fs/data/compress.c:584`). So today's
"variable" chunking is already fixed-size except for the tail chunk. The
parallel design formalises this: pre-partition the src bio into N
chunks of exactly `encoded_extent_max` (with one tail chunk) before
dispatching workers.

No compression-ratio loss vs the existing code.

### Per-chunk destination buffer

Each worker writes to its own per-chunk destination buffer (allocated from
`c->compress.bounce[WRITE]` mempool, sized up — see prerequisites). After
all workers complete, the serial post-pass copies/references each chunk's
compressed output into the outgoing wbio in submission order.

### The serial prefix → parallel body → serial post-pass shape

```
bch2_write_extent(op, wp, src, dst):
    ...
    if (!should_parallelise(op, src))
        goto serial_loop;          # existing do-while, unchanged

    # === SERIAL PREFIX ===
    chunks = partition_bio(src, encoded_extent_max)
    for ch in chunks:
        ch.write_pos = op->pos      # pre-assign position
        ch.version   = atomic64_inc_return(&c->key_version) if needed
        ch.nonce     = op->nonce
        op->pos.offset += ch.uncompressed_size
        op->nonce     += ch.uncompressed_size >> 9
    # all per-chunk identifiers assigned in submission order;
    # workers receive them as immutable input

    # === PARALLEL BODY ===
    closure_init_stack(&batch.cl)
    for ch in chunks:
        closure_get(&batch.cl)
        queue_work(c->compress.wq, &ch.work)
        # worker does: compress -> encrypt -> checksum -> closure_put
    closure_sync_unbounded(&batch.cl)

    # === SERIAL POST-PASS ===
    for ch in chunks_in_submission_order:
        if ch.err:
            handle_error(ch)
            continue
        # existing logic: bch2_alloc_sectors_append_ptrs_inlined,
        # init_append_extent (appends to op->insert_keys),
        # update op->written, etc.
    return

serial_loop:
    # original do-while body, verbatim
```

### Decision matrix: when serial vs parallel

The parallel path engages **only** when **all** of:

- `op->compression_opt != 0` (compression actually configured)
- `!op->incompressible` (caller hasn't already marked incompressible)
- `!(op->flags & BCH_WRITE_data_encoded)` (not the move path; path B
  nonce dependency is not safely parallelisable)
- `bio_sectors(src) > (encoded_extent_max >> 9)` (more than one chunk)
- `c->compress.wq != NULL` (pool initialised successfully)

Anything else falls through to the existing serial loop, behaviour-
preserving.

### Invariant preservation proofs

1. **`op->nonce` advance.** Assigned per-chunk in the serial prefix in
   submission order. Workers receive `ch.nonce` as immutable input and
   never touch `op->nonce`. Race eliminated.
2. **`op->pos.offset`.** Same: assigned in serial prefix from a single
   thread. Workers do not advance `op->pos`.
3. **Keylist sortedness.** The serial post-pass appends keys in the order
   of `chunks[]` (submission order). Because `op->pos.offset` was
   monotonically advanced in the serial prefix, `ch[i].write_pos <
   ch[i+1].write_pos`. The keylist is therefore strictly ascending.
   `bch2_verify_keylist_sorted` continues to hold in debug builds.
4. **`op->version` assignment.** `atomic64_inc_return` is unchanged.
   Versions are assigned monotonically in the serial prefix; the post-pass
   appends in the same order. Recovery's "version-as-rough-journal-seq"
   assumption (`fs/btree/check.c:712`) is preserved.
5. **`wp->sectors_free`.** `bch2_alloc_sectors_append_ptrs_inlined`
   continues to be called in the serial post-pass under `wp->lock`. No
   change.
6. **`src` bio iterator.** The parallel code refactors `bch2_bio_compress`
   to take a `bvec_iter` by value (not destructively swap
   `src->bi_iter`). The shared `src` bio's iterator is read-only to
   workers. The serial post-pass does the final `bio_advance(src,
   total_consumed)` after all workers complete.
7. **Crash consistency.** Per-extent journal atomicity unchanged; keylist
   is appended in submission order; submission-order matches expected
   on-disk extent ordering. The COW "no partial-write window" property
   still holds at the extent granularity (each extent is atomic in the
   journal as today).
8. **Backpointers.** Inserted in serial keylist-consumption order in
   `__bch2_write_index` — unchanged.

## Implementation phases

Each phase ends with verified-passing build and tests. Phases land as
small independently-mergeable commits.

### Phase 0: bootstrap (1 commit)

Fix the stale `CLAUDE.md:60-61` line that says "kernel code is synced
separately" — this is no longer true after the v6.18-rc1 removal. The
correct guidance is "go slow on `fs/`, but `fs/` is the canonical source".

### Phase 1: prerequisite patches (6 small commits)

Each patch is independently reviewable and useful on its own.

| # | Patch | Files | Test |
|---|---|---|---|
| P1 | `mempool_alloc_noprof` macro rename explicit | `linux/mempool.c` | Build + `nm` check |
| P2 | LZ4 userspace stub fix (link against liblz4) | `include/linux/lz4.h`, `linux/lz4.c` (new) | Roundtrip with `compression=lz4` |
| P3 | `zstd_workspace_size` write-once + `READ_ONCE`/`WRITE_ONCE` | `fs/data/compress.c`, `compress_types.h` | KCSAN run, no race report |
| P4 | Eager mempool init at mount (kill lazy `sb_lock` thundering herd, both compress + decompress paths) | `fs/data/compress.c`, `fs/init/fs.c` | First-write latency benchmark |
| P5 | `bch2_verify_compress`: move `mempool_free` after verify | `fs/data/compress.c` | Existing roundtrip tests still pass |
| P6 | Scale workspace + bounce mempools to `num_online_cpus()` (capped) | `fs/data/compress.c` | Verify pool sizing via debugfs |

### Phase 2: userspace workqueue shim — multi-worker (1 commit)

Rewrite `linux/workqueue.c` to support real `max_active`. Per-WQ worker
array, per-worker condvar wakeup, multi-worker `flush_work` /
`drain_workqueue` / `cancel_work_sync`. Lazy worker creation.

New tests: `tests/test_workqueue.c` (or wired into the existing test
harness):

- ordered WQ (max_active=1) preserves FIFO order
- max_active=4 actually parallelises (4 sleeping jobs finish in N/4 time)
- `flush_work` waits for in-flight job
- `drain_workqueue` waits for entire backlog
- `cancel_work_sync` correctly waits for running job
- `destroy_workqueue` while jobs pending → drains cleanly
- TSan / helgrind clean run

### Phase 3: real `num_online_cpus()` in userspace (1 commit)

`linux/percpu.c`: add `bch_nr_online_cpus` populated from
`sysconf(_SC_NPROCESSORS_ONLN)` in a constructor. Redirect
`num_online_cpus()` to it. Leave `num_possible_cpus()` /
`num_present_cpus()` alone (tied to per-CPU storage).

Test: print `num_online_cpus()` at fs mount and verify it matches
`nproc`.

### Phase 4: MT compression core (3 commits)

**Commit 4A: refactor `bch2_compress` API for explicit workspace.**

Extract `bch2_compress_one(ws, ...)` from `bch2_compress()` — takes an
explicit workspace pointer instead of pulling from the mempool. The
original `bch2_compress()` becomes a thin wrapper that does the
mempool alloc/free and calls `bch2_compress_one()`.

Also refactor `bch2_bio_compress` to take a `bvec_iter` by value so that
parallel workers can each consume their own iterator without mutating
the shared `src->bi_iter`.

No behaviour change for serial callers. Existing tests must still pass.

**Commit 4B: introduce `c->compress.wq` and the compression batch API.**

New files:

- `fs/data/compress_workers.h`: batch API (`bch_compress_batch`,
  `bch2_compress_batch_init/submit/wait`).
- `fs/data/compress_workers.c`: per-job `struct work_struct`, worker
  function (compress / encrypt / checksum / `closure_put`),
  `bch2_fs_compress_workers_init/exit` lifecycle.

The pool is created in `bch2_fs_compress_init` (after the mempool
setup) as `c->compress.wq = alloc_workqueue("bcachefs_compress",
WQ_UNBOUND | WQ_CPU_INTENSIVE | WQ_MEM_RECLAIM, 0)`. Destroyed in
`bch2_fs_compress_exit` *before* the mempool teardown.

If `alloc_workqueue` fails, `c->compress.wq` stays NULL and the
parallel path simply doesn't engage — callers fall through to the
existing serial loop. No mount failure.

Tests:

- New KUnit suite `fs/data/compress_test.c`: pool stress (N kthreads
  doing roundtrip compress/decompress for 30s), fault injection
  (mempool failure path), error paths.
- Standalone unit test for the batch API.

**Commit 4C: wire the parallel path into `bch2_write_extent`.**

The conditional branch shown in the architecture pseudocode above.
Behind a build-time guard initially (e.g. a module param defaulting to
on; see Configuration below).

Tests (shell):

- `debian/tests/compression-mt-roundtrip-{lz4,zstd,gzip}`: write a
  multi-chunk file, read it back, byte-compare. Run with parallel
  path enabled and disabled, both must succeed and produce identical
  on-disk content (modulo encryption nonces, which are pre-assigned
  identically).
- `debian/tests/compression-mt-with-encryption`: same but with
  ChaCha20 encryption enabled. Tests the nonce-pre-assignment
  invariant.
- `debian/tests/compression-mt-incompressible`: write random data;
  must end up as `BCH_COMPRESSION_TYPE_incompressible` extents
  identical to serial path.
- Tracepoint-based proof of concurrency: a shell test that enables a
  tracepoint, writes 8 chunks worth of data, and asserts
  `max_concurrent_compresses >= 2`.

### Phase 5: integration tests & verification (1 commit)

- `nixos-test.nix`: extend with MT compression scenarios (multi-writer,
  parallel-chunk single-writer, mixed compressible+incompressible).
- `Makefile`: add `make test`, `make test-quick`, `make test-mt`
  targets that drive the test suite locally.
- dm-flakey crash-consistency test: kill mid-multi-chunk-write, verify
  fsck recovers.
- Per-CPU compression-time counters in debugfs (`compress_ns_total`,
  `compress_count`, `max_concurrent`) so benchmarks can attribute
  speedup correctly.

## Configuration

- Module param `bch2_compress_workers` (default 0 = auto =
  `num_online_cpus()` capped at 32). Sets `max_active` on the
  compression WQ.
- Module param `bch2_compress_mt_min_chunks` (default 2). The parallel
  path engages only when the write would produce ≥ N chunks.
- Per-fs sysfs override: `/sys/fs/bcachefs/<UUID>/options/compress_workers`.
- Toggle off the parallel path entirely via
  `bch2_compress_workers = 1` (single-worker WQ; behaves like serial
  loop but exercises all the new code paths).

## Memory budget

| System | Workers (capped 32) | Workspace mempool reserve (zstd+gzip+lz4) | Bounce reserve (2× workers) | Total static |
|---|---|---|---|---|
| 8-core | 8 | 8 × ~2 MB = 16 MB | 16 × 256 KB = 4 MB | ~20 MB |
| 32-core | 32 | 32 × ~2 MB = 64 MB | 64 × 256 KB = 16 MB | ~80 MB |
| 64-core | 32 (capped) | 64 MB | 16 MB | ~80 MB |

zstd-only (most common config) cuts these roughly in half. Numbers are
mempool *reserves* — actual allocations come from `kvmalloc` first and
only hit the reserve under memory pressure.

## Risks and mitigations

| Risk | Mitigation |
|---|---|
| Workqueue shim rewrite breaks unrelated WQ callers | Phase 2 lands on its own with comprehensive WQ unit tests + smoke test of existing fs operations before Phase 4. |
| Parallel path produces different on-disk bytes (encryption nonce drift) | Phase 4C shell test diffs on-disk bytes between serial and parallel paths with same input + same key. |
| MT compression breaks crash consistency | Phase 5 dm-flakey test mid-write. |
| Memory pressure on 64+ core systems | Cap at 32; mempool reserve only used under pressure; rest comes from kvmalloc. |
| Move path silently engages parallel path | Decision matrix explicitly excludes `BCH_WRITE_data_encoded`; assertion in `bch2_write_extent`. |
| Verify-compress is broken under MT | Prerequisite P5 fixes verify ordering; KUnit tests in Phase 4B exercise the verify path under contention. |

## Test workflow

Per CLAUDE.md and `doc/testing.md`:

1. Local: `nix develop` → `make -j` (userspace binary build, every commit).
2. DKMS module: `sudo make dkms-reload` (every commit that touches `fs/`).
3. In-kernel unit tests (added in this series): run via `CONFIG_BCACHEFS_TESTS=1`.
4. Userspace unit tests: `make test`.
5. NixOS VM test: `nix flake check`.
6. ktest (out-of-tree): `btk run -IP ~/ktest/tests/fs/bcachefs/<test>.ktest`.
7. xfstests (via ktest): for any change that touches IO paths.

## Submission

This series will eventually be sent to `linux-bcachefs@vger.kernel.org`
per `Documentation/SubmittingPatches.rst:96-104`. WIP/RFC posts welcomed
per the same doc; design feedback on `#bcachefs-dev` IRC accelerates
review.

## Status

- [x] Audit of prior Rust-side proposal completed
  (`.claude/notes/2026-06-09-mt-compression-audit.md`).
- [x] Round-2 design synthesis (this document).
- [ ] Phase 0: bootstrap.
- [ ] Phase 1: prerequisite patches.
- [ ] Phase 2: workqueue shim multi-worker.
- [ ] Phase 3: `num_online_cpus()` in userspace.
- [ ] Phase 4: MT compression core.
- [ ] Phase 5: integration tests & verification.
