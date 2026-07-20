// SPDX-License-Identifier: GPL-2.0
/*
 * Non-trivial C macros cannot be used in Rust. Similarly, inlined C functions
 * cannot be called either. This file explicitly creates functions ("helpers")
 * that wrap those so that they can be called from Rust.
 *
 * Sorted alphabetically.
 */

#include <linux/compiler_types.h>

#ifdef __BINDGEN__
// Omit `inline` for bindgen as it ignores inline functions.
#define __rust_helper
#else
// The helper functions are all inline functions.
//
// We use `__always_inline` here to bypass LLVM inlining checks, in case the
// helpers are inlined directly into Rust CGUs.
//
// The LLVM inlining checks are false positives:
// * LLVM doesn't want to inline functions compiled with
//   `-fno-delete-null-pointer-checks` with code compiled without.
//   The C CGUs all have this enabled and Rust CGUs don't. Inlining is okay
//   since this is one of the hardening features that does not change the ABI,
//   and we shouldn't have null pointer dereferences in these helpers.
// * LLVM doesn't want to inline functions with different list of builtins. C
//   side has `-fno-builtin-wcslen`; `wcslen` is not a Rust builtin, so they
//   should be compatible, but LLVM does not perform inlining due to attributes
//   mismatch.
// * clang and Rust doesn't have the exact target string. Clang generates
//   `+cmov,+cx8,+fxsr` but Rust doesn't enable them (in fact, Rust will
//   complain if `-Ctarget-feature=+cmov,+cx8,+fxsr` is used). x86-64 always
//   enable these features, so they are in fact the same target string, but
//   LLVM doesn't understand this and so inlining is inhibited. This can be
//   bypassed with `--ignore-tti-inline-compatible`, but this is a hidden
//   option.
#define __rust_helper __always_inline
#endif

// vendored-trim-618: #include "acpi.c" (acpi_of_match_device absent on 6.18)
#include "atomic.c"
#include "atomic_ext.c"
// vendored-trim-618 (driver model): #include "auxiliary.c"
#include "barrier.c"
// vendored-trim-618 (driver model): #include "binder.c"
#include "bitmap.c"
#include "bitops.c"
// vendored-trim-618 (driver model): #include "blk.c"
#include "bug.c"
#include "build_assert.c"
#include "build_bug.c"
// vendored-trim-618 (driver model): #include "clk.c"
#include "completion.c"
#include "cpu.c"
// vendored-trim-618 (driver model): #include "cpufreq.c"
#include "cpumask.c"
#include "cred.c"
// vendored-trim-618 (driver model): #include "device.c"
// vendored-trim-618 (driver model): #include "dma.c"
// vendored-trim-618 (driver model): #include "dma-resv.c"
// vendored-trim-618 (driver model): #include "drm.c"
// vendored-trim-618 (driver model): #include "drm_gpuvm.c"
#include "err.c"
// vendored-trim-618 (driver model): #include "irq.c"
#include "fs.c"
// vendored-trim-618: #include "gpu.c" (6.18 lacks gpu_buddy)
// vendored-trim-618 (driver model): #include "io.c"
#include "jump_label.c"
#include "kunit.c"
#include "list.c"
#include "maple_tree.c"
#include "mm.c"
#include "mutex.c"
// vendored-trim-618 (driver model): #include "of.c"
#include "page.c"
// vendored-trim-618 (driver model): #include "pci.c"
#include "pid_namespace.c"
// vendored-trim-618 (driver model): #include "platform.c"
#include "poll.c"
#include "processor.c"
// vendored-trim-618 (driver model): #include "property.c"
// vendored-trim-618 (driver model): #include "pwm.c"
#include "rbtree.c"
#include "rcu.c"
#include "refcount.c"
// vendored-trim-618 (driver model): #include "regulator.c"
// vendored-trim-618 (driver model): #include "scatterlist.c"
#include "security.c"
#include "signal.c"
#include "slab.c"
#include "spinlock.c"
#include "string.c"
#include "sync.c"
#include "task.c"
#include "time.c"
#include "uaccess.c"
// vendored-trim-618 (driver model): #include "usb.c"
#include "vmalloc.c"
#include "wait.c"
#include "workqueue.c"
#include "xarray.c"
