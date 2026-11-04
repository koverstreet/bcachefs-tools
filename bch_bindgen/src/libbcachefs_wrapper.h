/* The non-fs/ userspace C that Rust calls into: tools-util, raid, and the
 * fuse and Rust shims. The fs/ wrapper is pulled in
 * for the bcachefs types these reference — the bindgen blocklists those so they
 * resolve to bcachefs-kernel's bindings rather than being redefined here.
 */

#include "bcachefs.h"

#include "tools-util.h"

#include "c_src/fuse_shims.h"
#include "c_src/rust_shims.h"
