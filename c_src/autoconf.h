/* SPDX-License-Identifier: GPL-2.0 */
/*
 * The userspace build's configuration - what include/generated/autoconf.h is
 * to a kernel build. Force-included (-include) into every C compile, every
 * bindgen run and the static-inline wrappers bindgen emits, so the C and the
 * Rust bindings see the same headers: a define only one side had would give
 * them different struct layouts, or a Rust call into a static inline's
 * fallback where C has the real function.
 *
 * The CONFIG_* defines here are also Rust cfgs (fs/build_config.rs), as the
 * kernel's rustc_cfg makes them for a kernel build.
 *
 * Per-build additions - make debug's CONFIG_BCACHEFS_DEBUG - come from
 * EXTRA_CFLAGS, which fs/build_config.rs applies the same way.
 */
#ifndef _BCACHEFS_TOOLS_AUTOCONF_H
#define _BCACHEFS_TOOLS_AUTOCONF_H

#define _FILE_OFFSET_BITS		64
#define _GNU_SOURCE			1
#define _LGPL_SOURCE			1
#define RCU_MEMBARRIER			1
#define ZSTD_STATIC_LINKING_ONLY	1
#define FUSE_USE_VERSION		35
#define NO_BCACHEFS_CHARDEV		1
#define NO_BCACHEFS_FS			1
#define BCACHEFS_FUSE			1
#define __SANE_USERSPACE_TYPES__	1

#define CONFIG_DEBUG_FS			1
#define CONFIG_UNICODE			1
#define CONFIG_STACKTRACE		1

#endif /* _BCACHEFS_TOOLS_AUTOCONF_H */
