/* The non-fs/ userspace C that Rust calls into: tools-util. The fs/ wrapper is
 * pulled in for the bcachefs types it references — the bindgen blocklists those
 * so they resolve to bcachefs-kernel's bindings rather than being redefined
 * here.
 */

#include "bcachefs.h"

#include "tools-util.h"
