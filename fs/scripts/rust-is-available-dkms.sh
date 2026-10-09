#!/bin/sh
# SPDX-License-Identifier: GPL-2.0
#
# Return "y" when bcachefs' DKMS Rust objects can be built against the kernel's
# own Rust, else the reason they can't. The kernel's own rust_is_available.sh
# owns the normal Rust-for-Linux availability rules; this script adds only the
# extra checks needed by bcachefs' out-of-tree Rust glue.
#
# When a check fails the Makefile falls back to bcachefs's vendored Rust stack,
# and fails the build if that can't be used either — but log exactly which
# prerequisite is missing (to stderr, so it lands in the DKMS build log). The
# point is that a kernel which is *almost* Rust-capable (config + scripts present
# but, say, the prebuilt stdlib not installed) otherwise dies deep in rustc with
# a cryptic "E0463: can't find crate for `core`", when the vendored stack would
# have built.

set -e

canonical_version()
{
	IFS=.
	set -- $1
	echo $((100000 * $1 + 100 * $2 + $3))
}

# The kernel's Rust can't be used: report exactly what's missing. The reason
# IS the verdict: stdout is "y", or else the reason we couldn't. If the vendored
# stack can't be used either, the Makefile's build error quotes it; the stderr
# copy lands in the DKMS build log either way.

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

#
# Stripped of quotes, backslashes, $ and backticks: the reason travels through
# make and the shell, and an unusual path should mangle the message, never the
# build.
skip()
{
	reason=$(printf '%s' "$1" | tr -d '"\\$`')
	if [ -n "$reason" ]; then
		echo "bcachefs: can't use the kernel's Rust, trying the vendored stack — $reason" >&2
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

# Its reason is the first "***" block of what it prints - "Rust compiler
# 'rustc' is too old.; Your version: ...; Minimum version: ..." - the rest is
# boilerplate pointing at the kernel docs.
if ! kernel_rust_out=$("$kernel_rust_check" 2>&1); then
	kernel_rust_why=$(printf '%s\n' "$kernel_rust_out" |
		sed -n 's/^\*\*\* *//p' |
		awk 'NF { why = why (why ? "; " : "") $0; next } why { print why; exit }')
	skip "the kernel's scripts/rust_is_available.sh: ${kernel_rust_why:-failed, without saying why}"
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

# rustc --version can work where compiling doesn't: a half-upgraded install,
# librustc_driver out of step with libLLVM, only fails at the first lazily
# bound symbol - "symbol lookup error", deep in the build (tools#1082). So
# compile something, with the host rustc: it builds the first Rust in the build.
if tmp=$(mktemp -d 2>/dev/null); then
	if ! printf 'fn main() {}\n' |
	     LC_ALL=C "$HOSTRUSTC" -o "$tmp/probe" - >/dev/null 2>"$tmp/err"; then
		err=$(grep -m1 . "$tmp/err")
		rm -rf "$tmp"
		skip "$HOSTRUSTC can't compile an empty program: $err"
	fi
	rm -rf "$tmp"
fi

if [ ! -r "$KERNEL_OBJ/include/generated/rustc_cfg" ]; then
	skip "missing $KERNEL_OBJ/include/generated/rustc_cfg (kernel not configured for Rust)"
fi

# The prebuilt Rust stdlib (libcore.rmeta etc.) must be present for an
# out-of-tree module to link against `core`. A kernel that ships the Rust config
# + scripts but not the compiled rust/ artifacts — a locally built kernel, or a
# kernel-devel/headers package without the Rust build output — otherwise dies
# with E0463 "can't find crate for `core`" instead of falling back to vendored.
libcore=$KERNEL_OBJ/rust/libcore.rmeta

if [ ! -r "$libcore" ]; then
	skip "missing the kernel's prebuilt Rust stdlib ($libcore); the kernel was built/installed without its rust/ artifacts"
fi

# core isn't all of it: kbuild compiles a module's Rust with --extern kernel and
# --extern pin_init (scripts/Makefile.build), from 7.2 also zerocopy and
# zerocopy_derive, and the kernel crate needs the macros proc-macro. A packager
# trimming rust/ can keep one and drop another.
for crate in kernel pin_init $(grep -q -- '--extern zerocopy' "$KERNEL_SRC/scripts/Makefile.build" 2>/dev/null && echo zerocopy); do
	[ -r "$KERNEL_OBJ/rust/lib$crate.rmeta" ] ||
		skip "missing the kernel's prebuilt Rust crate $crate ($KERNEL_OBJ/rust/lib$crate.rmeta)"
done

for procmacro in macros $(grep -q -- '--extern zerocopy_derive' "$KERNEL_SRC/scripts/Makefile.build" 2>/dev/null && echo zerocopy_derive); do
	ls "$KERNEL_OBJ/rust/lib$procmacro".* >/dev/null 2>&1 ||
		skip "missing the kernel's prebuilt Rust proc-macro $procmacro ($KERNEL_OBJ/rust/lib$procmacro.*)"
done

# ...and it has to have been built by *this* rustc, or rustc refuses to load it:
# E0514, "found crate `core` compiled by an incompatible version of rustc".
#
# The CONFIG_RUSTC_VERSION check above does not cover this. That records the
# rustc which ran at kernel *configure* time, which is not necessarily the one
# that compiled the .rmeta files now sitting in rust/: reconfigure after a
# toolchain change and auto.conf agrees with the installed rustc while every
# artifact on disk disagrees. Reported by debaba, on a locally built 7.2-rc6
# whose rust/ came from rustc 1.96 and which was then built against a 1.95
# host — straight to E0514 rather than to the vendored stack this script exists
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