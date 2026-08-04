/* The non-fs/ userspace C that Rust calls into: tools-util, the libbcachefs
 * userspace API, crypto, raid, and the fuse shims. The fs/ wrapper is pulled in
 * for the bcachefs types these reference — the bindgen blocklists those so they
 * resolve to bcachefs-kernel's bindings rather than being redefined here.
 */

/*
 * First, before anything else can touch it: clang's stddef.h is guarded with
 * the __need_* protocol, so a header that pulls in a partial stddef (for NULL,
 * or ptrdiff_t) leaves __STDDEF_H defined without defining size_t. A later
 * plain include is then a no-op and size_t never appears.
 *
 * That matters here because the ioctl numbers below encode sizeof() of their
 * argument type, and bindgen evaluates them by compiling a probe against this
 * header. If size_t is missing the probe fails, and bindgen drops or
 * miscomputes the constant without saying anything.
 */
#include <stddef.h>

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
 * whoever includes it. See the stddef.h note at the top.
 */
#include <linux/fs.h>
