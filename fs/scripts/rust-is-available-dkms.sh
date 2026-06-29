#!/bin/sh
# SPDX-License-Identifier: GPL-2.0
#
# Return "y" when bcachefs' optional DKMS Rust objects can be built with the
# current kernel/toolchain, else "n". The kernel's own rust_is_available.sh owns
# the normal Rust-for-Linux availability rules; this script adds only the extra
# checks needed by bcachefs' out-of-tree Rust glue.
#
# When a check fails we fall back to the C-only module — but log exactly which
# prerequisite is missing (to stderr, so it lands in the DKMS build log). The
# point is that a kernel which is *almost* Rust-capable (config + scripts present
# but, say, the prebuilt stdlib not installed) otherwise builds C fine yet dies
# deep in rustc with a cryptic "E0463: can't find crate for `core`" — instead of
# this script catching it and saying what to install.

set -e

canonical_version()
{
	IFS=.
	set -- $1
	echo $((100000 * $1 + 100 * $2 + $3))
}

# Fall back to the C-only module, reporting exactly what's missing. The reason
# goes to stderr; only the y/n verdict goes to stdout (the Makefile captures it
# via $(shell ...), so stderr is free for diagnostics).
skip()
{
	if [ -n "$1" ]; then
		echo "bcachefs: building without Rust — $1" >&2
	fi
	echo n
	exit 0
}

KERNEL_SRC=${KERNEL_SRC:-.}
KERNEL_OBJ=${KERNEL_OBJ:-$KERNEL_SRC}
RUSTC=${RUSTC:-rustc}
HOSTRUSTC=${HOSTRUSTC:-$RUSTC}
BINDGEN=${BINDGEN:-bindgen}
CC=${CC:-cc}
export RUSTC BINDGEN CC

kernel_rust_check=$KERNEL_SRC/scripts/rust_is_available.sh

if [ ! -x "$kernel_rust_check" ]; then
	skip "no $kernel_rust_check (kernel sources lack Rust support)"
fi

if ! "$kernel_rust_check" >/dev/null 2>&1; then
	skip "$kernel_rust_check reports the kernel's Rust toolchain unavailable"
fi

rustc_output=$(LC_ALL=C "$RUSTC" --version 2>/dev/null) ||
	skip "rustc ($RUSTC) not found or failed to run"
rustc_version=$(echo "$rustc_output" |
	sed -nE '1s:.*rustc ([0-9]+\.[0-9]+\.[0-9]+).*:\1:p')

if [ -z "$rustc_version" ]; then
	skip "could not parse a rustc version from '$rustc_output'"
fi

if [ -n "$CONFIG_RUSTC_VERSION" ] &&
   [ "$(canonical_version "$rustc_version")" != "$CONFIG_RUSTC_VERSION" ]; then
	skip "rustc $rustc_version does not match the kernel's CONFIG_RUSTC_VERSION ($CONFIG_RUSTC_VERSION)"
fi

command -v "$HOSTRUSTC" >/dev/null 2>&1 || skip "host rustc ($HOSTRUSTC) not found"
command -v "$BINDGEN" >/dev/null 2>&1 || skip "bindgen ($BINDGEN) not found"

if [ ! -r "$KERNEL_OBJ/include/generated/rustc_cfg" ]; then
	skip "missing $KERNEL_OBJ/include/generated/rustc_cfg (kernel not configured for Rust)"
fi

# The prebuilt Rust stdlib (libcore.rmeta etc.) must be present for an
# out-of-tree module to link against `core`. A kernel that ships the Rust config
# + scripts but not the compiled rust/ artifacts — a locally built kernel, or a
# kernel-devel/headers package without the Rust build output — otherwise dies
# with E0463 "can't find crate for `core`" instead of falling back to C-only.
if [ ! -r "$KERNEL_OBJ/rust/libcore.rmeta" ]; then
	skip "missing the kernel's prebuilt Rust stdlib ($KERNEL_OBJ/rust/libcore.rmeta); the kernel was built/installed without its rust/ artifacts"
fi

echo y
