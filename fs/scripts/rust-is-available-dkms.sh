#!/bin/sh
# SPDX-License-Identifier: GPL-2.0
#
# Return "y" when bcachefs' optional DKMS Rust objects can be built with the
# current kernel/toolchain, else "n". The kernel's own rust_is_available.sh owns
# the normal Rust-for-Linux availability rules; this script adds only the extra
# checks needed by bcachefs' out-of-tree Rust glue.

set -e

canonical_version()
{
	IFS=.
	set -- $1
	echo $((100000 * $1 + 100 * $2 + $3))
}

skip()
{
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
	skip
fi

if ! "$kernel_rust_check" >/dev/null 2>&1; then
	skip
fi

rustc_output=$(LC_ALL=C "$RUSTC" --version 2>/dev/null) || skip
rustc_version=$(echo "$rustc_output" |
	sed -nE '1s:.*rustc ([0-9]+\.[0-9]+\.[0-9]+).*:\1:p')

if [ -z "$rustc_version" ]; then
	skip
fi

if [ -n "$CONFIG_RUSTC_VERSION" ] &&
   [ "$(canonical_version "$rustc_version")" != "$CONFIG_RUSTC_VERSION" ]; then
	skip
fi

command -v "$HOSTRUSTC" >/dev/null 2>&1 || skip
command -v "$BINDGEN" >/dev/null 2>&1 || skip

if [ ! -r "$KERNEL_OBJ/include/generated/rustc_cfg" ]; then
	skip
fi

echo y
