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

get_elf_arch()
{
	_file=$1
	[ -f "$_file" ] || return 1

	# Read 20 bytes (magic + class + endian + ... + e_machine) in a single od call
	_raw=$(od -v -An -N 20 -t x1 "$_file" 2>/dev/null) || return 1
	set -f
	set -- $_raw
	set +f

	[ $# -eq 20 ] || return 1

	# Verify ELF magic header (\x7f E L F) using $1..$4
	[ "$1$2$3$4" = "7f454c46" ] || return 1

	_class=$5   # offset 4 (1 = 32-bit, 2 = 64-bit)
	_endian=$6  # offset 5 (1 = Little-Endian, 2 = Big-Endian)
	_m1=${19}   # offset 18
	_m2=${20}   # offset 19

	if [ "$_endian" = "02" ]; then
		_m="$_m2 $_m1"
	else
		_m="$_m1 $_m2"
	fi

	case "$_m" in
		"3e 00") echo "x86_64" ;;
		"b7 00") echo "aarch64" ;;
		"28 00") echo "arm" ;;
		"f3 00") [ "$_class" = "01" ] && echo "riscv32" || echo "riscv64" ;;
		"03 00") echo "x86" ;;
		"15 00") echo "ppc64" ;;
		"16 00") echo "s390x" ;;
		"02 01") echo "loongarch64" ;; # EM_LOONGARCH (258 = 0x0102)
		*)       echo "unknown (0x$_m)" ;;
	esac
}

# Reuses get_elf_arch on native host binaries to eliminate duplicate arch tables
get_host_arch()
{
	for _bin in "${HOSTRUSTC:-$RUSTC}" "$RUSTC" "${CC:-cc}" /bin/sh; do
		_path=$(command -v "$_bin" 2>/dev/null) || continue
		_arch=$(get_elf_arch "$_path") && [ -n "$_arch" ] && { echo "$_arch"; return 0; }
	done
	uname -m 2>/dev/null || echo "unknown"
}

# Fall back to the C-only module, reporting exactly what's missing. The reason
# IS the verdict: stdout is "y", or else the reason we couldn't. The Makefile
# bakes that into the module, so the mount-time "built without Rust support"
# message can say why - months later, on a machine whose build log is long gone.
# The stderr copy still lands in the DKMS build log for whoever is watching the
# build itself.
#
# Stripped of the characters that would otherwise terminate the C string literal
# it ends up inside: the reason travels through make and the shell into a -D. An
# unusual path should mangle the message, never break the build. The apostrophe
# comes out in the Makefile, which also covers the reasons it composes itself.
skip()
{
	reason=$(printf '%s' "$1" | tr -d '"\\$`')
	if [ -n "$reason" ]; then
		echo "bcachefs: building without Rust — $reason" >&2
	fi
	printf '%s\n' "${reason:-reason not recorded}"
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
libcore=$KERNEL_OBJ/rust/libcore.rmeta

if [ ! -r "$libcore" ]; then
	skip "missing the kernel's prebuilt Rust stdlib ($libcore); the kernel was built/installed without its rust/ artifacts"
fi

# ...and it has to have been built by *this* rustc, or rustc refuses to load it:
# E0514, "found crate `core` compiled by an incompatible version of rustc".
#
# The CONFIG_RUSTC_VERSION check above does not cover this. That records the
# rustc which ran at kernel *configure* time, which is not necessarily the one
# that compiled the .rmeta files now sitting in rust/: reconfigure after a
# toolchain change and auto.conf agrees with the installed rustc while every
# artifact on disk disagrees. Reported by debaba, on a locally built 7.2-rc6
# whose rust/ came from rustc 1.96 and which was then built against a 1.95
# host — straight to E0514 rather than to the C-only module this script exists
# to fall back to.
#
# So ask the artifact we actually link against. An rmeta opens with a
# length-prefixed version string at offset 17, right after the "rust" magic and
# the format version — the same string rustc reads to decide E0514. Extracted
# with head/tr/grep rather than strings(1), which is binutils and not something
# a DKMS build environment is guaranteed to have.
libcore_version=$(head -c 4096 "$libcore" 2>/dev/null | tr -c '[:print:]' '\n' |
	grep -oE 'rustc [0-9]+\.[0-9]+\.[0-9]+' | head -1 | sed 's/^rustc //')

if [ -n "$libcore_version" ] && [ "$libcore_version" != "$rustc_version" ]; then
	skip "rustc $rustc_version cannot use the kernel's Rust stdlib, which was built by rustc $libcore_version ($libcore)"
fi

# Proc-macro shared libraries (rust/libmacros.so) are dynamically loaded into
# rustc at build time, so their architecture must match the host running rustc.
libmacros=$KERNEL_OBJ/rust/libmacros.so

if [ -r "$libmacros" ] && command -v od >/dev/null 2>&1; then
	host_arch=$(get_host_arch)
	macro_arch=$(get_elf_arch "$libmacros")

	if [ -n "$host_arch" ] && [ -n "$macro_arch" ] && [ "$host_arch" != "$macro_arch" ]; then
		skip "host architecture ($host_arch) does not match rust/libmacros.so ($macro_arch); native kernel headers required"
	fi
fi

echo y