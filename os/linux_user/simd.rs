// SPDX-License-Identifier: GPL-2.0

//! The vector registers, which userspace code may always use.

/// @f, which may use the vector registers.
pub fn with<T>(f: impl FnOnce() -> T) -> T {
    f()
}
