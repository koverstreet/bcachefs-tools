# Vendored kernel files

bcachefs source itself lives in `fs/` and is developed in this repository
directly — kernel-side `fs/bcachefs/` is downstream of us now.

A small number of *other* kernel files are vendored verbatim under
`include/linux/` and `linux/`, because they're general-purpose helpers
bcachefs uses (hash, ringbuffers, math) where it makes no sense to
maintain a separate userspace version.

## Files

| Tools path | Kernel source |
|---|---|
| `include/linux/xxhash.h` | `include/linux/xxhash.h` |
| `linux/xxhash.c` | `lib/xxhash.c` |
| `include/linux/list_nulls.h` | `include/linux/list_nulls.h` |
| `include/linux/poison.h` | `include/linux/poison.h` |
| `include/linux/generic-radix-tree.h` | `include/linux/generic-radix-tree.h` |
| `linux/generic-radix-tree.c` | `lib/generic-radix-tree.c` |
| `include/linux/kmemleak.h` | `include/linux/kmemleak.h` |
| `linux/int_sqrt.c` | `lib/math/int_sqrt.c` |
| `Makefile.compiler` | `scripts/Makefile.compiler` |

## When to refresh

These move rarely. Refresh when:

- bcachefs starts using a new symbol from one of these (build fails or
  silently behaves differently against the stale userspace copy)
- Upstream picks up a meaningful fix you want to ride along on

## How

```
make update-vendored-kernel-sources LINUX_DIR=~/linux
# review the staged changes
make update-commit-vendored-kernel-sources LINUX_DIR=~/linux  # or commit by hand
```

Both targets just `cp` from `LINUX_DIR` into the right tools paths and
`git add` them. The `-commit-` variant also writes a commit recording
the kernel commit at `LINUX_DIR`'s HEAD.

## Not in this list

Anything under `fs/bcachefs/` is **not** vendored — it's the same source
tree as the kernel's, but tools is now the canonical home. Changes go
in here and flow out to the kernel via the export-to-kernel tooling
(see TODO).
