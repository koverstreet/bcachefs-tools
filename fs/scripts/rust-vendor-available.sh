#!/bin/sh
# SPDX-License-Identifier: GPL-2.0
#
# Print "y" when bcachefs' vendored Rust stack can be built, else everything
# that's missing, "; "-separated. All of it, not the first thing found: whoever
# reads this is installing packages, and should only have to do that once.
#
# The vendored stack - fs/vendor/kernel-rust, Makefile.rust.vendor - is what
# the build falls back to when the kernel's own Rust can't be used (see
# rust-is-available-dkms.sh). It needs rustc >= $RUSTC_MIN with its rust-src,
# bindgen, and from the kernel tree include/generated/rustc_cfg (written by
# syncconfig whatever the config) and scripts/generate_rust_target.rs.
#
# The answer travels through make and the shell into a build error, so it's
# stripped of quotes, backslashes, $ and backticks: an unusual path should
# mangle the message, never the build.

RUSTC=${RUSTC:-rustc}
HOSTRUSTC=${HOSTRUSTC:-$RUSTC}
BINDGEN=${BINDGEN:-bindgen}
KERNEL_OBJ=${KERNEL_OBJ:-.}

missing=

miss()
{
	missing="${missing:+$missing; }$1"
}

if ! command -v "$RUSTC" >/dev/null 2>&1; then
	miss "rustc ($RUSTC) not found"
else
	version=$(LC_ALL=C "$RUSTC" --version 2>/dev/null |
		sed -nE '1s/^rustc ([0-9]+\.[0-9]+\.[0-9]+).*/\1/p')

	if [ -z "$version" ]; then
		miss "no version from $RUSTC --version"
	elif [ "$(printf '%s\n' "$RUSTC_MIN" "$version" | sort -V | head -1)" != "$RUSTC_MIN" ]; then
		miss "rustc $version is older than $RUSTC_MIN"
	fi

	core=$("$RUSTC" --print sysroot 2>/dev/null)/lib/rustlib/src/rust/library/core/src/lib.rs
	[ -r "$core" ] ||
		miss "rust-src not found (no $core - with rustup: rustup component add rust-src)"
fi

if [ "$HOSTRUSTC" != "$RUSTC" ] && ! command -v "$HOSTRUSTC" >/dev/null 2>&1; then
	miss "host rustc ($HOSTRUSTC) not found"
fi

command -v "$BINDGEN" >/dev/null 2>&1 ||
	miss "bindgen ($BINDGEN) not found"

[ -r "$KERNEL_OBJ/scripts/generate_rust_target.rs" ] ||
	miss "kernel tree has no scripts/generate_rust_target.rs"
[ -r "$KERNEL_OBJ/include/generated/rustc_cfg" ] ||
	miss "kernel tree has no include/generated/rustc_cfg"

printf '%s\n' "${missing:-y}" | tr -d '"\\$`'
