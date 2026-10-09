#!/bin/sh
# SPDX-License-Identifier: GPL-2.0
#
# rust-available.sh kernel|vendored: print "y" if bcachefs's Rust can be built
# against the kernel's own Rust / our vendored copy (fs/Makefile.rust.vendor),
# else everything that's missing, "; "-separated - all of it, so whoever reads
# it installs packages once. It ends up in a build error, so it's stripped of
# quotes, backslashes, $ and backticks.
#
# Environment: RUSTC, HOSTRUSTC, BINDGEN, CC, KERNEL_SRC, KERNEL_OBJ; for kernel,
# CONFIG_RUSTC_VERSION; for vendored, RUSTC_MIN.

RUSTC=${RUSTC:-rustc}
HOSTRUSTC=${HOSTRUSTC:-$RUSTC}
BINDGEN=${BINDGEN:-bindgen}
KERNEL_SRC=${KERNEL_SRC:-.}
KERNEL_OBJ=${KERNEL_OBJ:-$KERNEL_SRC}
export RUSTC BINDGEN CC

missing=
miss()
{
	missing="${missing:+$missing; }$1"
}

report()
{
	missing=$(printf '%s' "$missing" | tr -d '"\\$`')
	[ -n "$missing" ] && [ "$1" = kernel ] &&
		echo "bcachefs: can't use the kernel's Rust, trying the vendored stack — $missing" >&2
	printf '%s\n' "${missing:-y}"
	exit 0
}

# Both: rustc, bindgen, and a host rustc that can compile - --version works on
# a half-upgraded install that fails at its first lazily bound symbol (#1082).
version=$(LC_ALL=C "$RUSTC" --version 2>/dev/null |
	sed -nE '1s/^rustc ([0-9]+\.[0-9]+\.[0-9]+).*/\1/p')
[ -n "$version" ] || miss "no rustc version from '$RUSTC --version'"
command -v "$BINDGEN" >/dev/null 2>&1 || miss "bindgen ($BINDGEN) not found"
if tmp=$(mktemp -d 2>/dev/null); then
	printf 'fn main() {}\n' | LC_ALL=C "$HOSTRUSTC" -o "$tmp/probe" - >/dev/null 2>"$tmp/err" ||
		miss "$HOSTRUSTC can't compile an empty program: $(grep -m1 . "$tmp/err")"
	rm -rf "$tmp"
fi

case $1 in
kernel)
	# The kernel's own availability check first: its reason is the first
	# "***" block of its output.
	check=$KERNEL_SRC/scripts/rust_is_available.sh
	if [ ! -x "$check" ]; then
		miss "no $check (kernel sources lack Rust support)"
		report kernel
	elif ! out=$("$check" 2>&1); then
		why=$(printf '%s\n' "$out" | sed -n 's/^\*\*\* *//p' |
			awk 'NF { w = w (w ? "; " : "") $0; next } w { print w; exit }')
		miss "the kernel's scripts/rust_is_available.sh: ${why:-failed}"
		report kernel
	fi

	if [ -n "$version" ] && [ -n "$CONFIG_RUSTC_VERSION" ]; then
		minor=${version#*.}
		[ $((100000 * ${version%%.*} + 100 * ${minor%.*} + ${version##*.})) = "$CONFIG_RUSTC_VERSION" ] ||
			miss "rustc $version does not match the kernel's CONFIG_RUSTC_VERSION ($CONFIG_RUSTC_VERSION)"
	fi

	[ -r "$KERNEL_OBJ/include/generated/rustc_cfg" ] ||
		miss "missing $KERNEL_OBJ/include/generated/rustc_cfg (kernel not configured for Rust)"

	# The prebuilt crates a module's Rust links against: kbuild passes
	# --extern kernel and pin_init (and from 7.2 zerocopy), and the kernel
	# crate needs the macros proc-macro. Packagers trim rust/.
	r=$KERNEL_OBJ/rust
	zc=$(grep -q -- '--extern zerocopy' "$KERNEL_SRC/scripts/Makefile.build" 2>/dev/null && echo zerocopy)
	for crate in core kernel pin_init $zc; do
		[ -r "$r/lib$crate.rmeta" ] || miss "missing the kernel's prebuilt Rust crate $r/lib$crate.rmeta"
	done
	for crate in macros ${zc:+zerocopy_derive}; do
		ls "$r/lib$crate".* >/dev/null 2>&1 || miss "missing the kernel's prebuilt Rust proc-macro $r/lib$crate.*"
	done

	# ...built by this rustc, or E0514. CONFIG_RUSTC_VERSION is the rustc at
	# configure time, not necessarily the one that built rust/; the rmeta
	# header names it.
	built=$(head -c 4096 "$r/libcore.rmeta" 2>/dev/null | tr -c '[:print:]' '\n' |
		grep -oE 'rustc [0-9]+\.[0-9]+\.[0-9]+' | head -1 | sed 's/^rustc //')
	[ -z "$built" ] || [ -z "$version" ] || [ "$built" = "$version" ] ||
		miss "rustc $version can't use the kernel's Rust, built by rustc $built"
	;;
vendored)
	if [ -n "$version" ] &&
	   [ "$(printf '%s\n' "$RUSTC_MIN" "$version" | sort -V | head -1)" != "$RUSTC_MIN" ]; then
		miss "rustc $version is older than $RUSTC_MIN"
	fi
	core=$("$RUSTC" --print sysroot 2>/dev/null)/lib/rustlib/src/rust/library/core/src/lib.rs
	[ -r "$core" ] || miss "rust-src not found (no $core - with rustup: rustup component add rust-src)"
	for f in scripts/generate_rust_target.rs include/generated/rustc_cfg; do
		[ -r "$KERNEL_OBJ/$f" ] || miss "kernel tree has no $f"
	done
	;;
esac

report "$1"
