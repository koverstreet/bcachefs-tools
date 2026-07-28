/* The non-fs/ userspace C that Rust calls into: tools-util, the libbcachefs
 * userspace API, crypto, raid, and the fuse shims. The fs/ wrapper is pulled in
 * for the bcachefs types these reference — the bindgen blocklists those so they
 * resolve to bcachefs-kernel's bindings rather than being redefined here.
 */

#include "bcachefs.h"

#include "tools-util.h"
#include "crypto.h"
#include "libbcachefs.h"
#include "raid/raid.h"

#include "c_src/fuse_shims.h"
#include "c_src/rust_shims.h"

/*
 * Block device ioctls: the numbers encode sizeof() of the argument type, so
 * they vary by arch and word size and must not be hardcoded. bindgen computes
 * them for the target (via clang_macro_fallback), but only if the argument
 * types resolve - the uapi header uses bare size_t and leaves defining it to
 * whoever includes it. Without stddef.h first, bindgen's probe fails to
 * compile and it drops the constant silently.
 */
#include <stddef.h>
#include <linux/fs.h>
