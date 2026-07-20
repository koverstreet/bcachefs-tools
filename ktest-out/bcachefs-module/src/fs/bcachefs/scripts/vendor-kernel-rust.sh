#!/bin/sh
# SPDX-License-Identifier: GPL-2.0
#
# vendor-kernel-rust.sh — let a CONFIG_RUST=n kernel build bcachefs's Rust module.
#
# bcachefs's kernel module has a Rust component that normally links against the
# *kernel's* own Rust support stack — the `core`, `kernel`, `bindings`, ... crates
# the kernel compiles under its rust/ dir when CONFIG_RUST=y. On a kernel built
# WITHOUT CONFIG_RUST there is no such stack to link against, so bcachefs falls
# back to its C-only module. That loses the Rust code on every kernel shipped
# with Rust off — including the 6.18 LTS that distros (NixOS and others) camp on
# for years, and which nobody ships with CONFIG_RUST=y.
#
# This script removes that limitation. It vendors bcachefs's own copy of the
# kernel Rust support stack (kernel 7.1's rust/, patched to build against an
# older kernel's C ABI — see fs/vendor/kernel-rust/) into the target kernel's
# build tree and builds it there. The result is a tree that, to the rest of the
# build, is indistinguishable from a CONFIG_RUST=y kernel that shipped its rmeta:
# the existing CONFIG_RUST=y path in fs/Makefile then builds bcachefs's Rust
# module normally, and the arch's KBUILD_RUSTFLAGS (-Ccode-model=kernel,
# -Cno-redzone, cf-protection, the SSE-disable, the target.json) ride in for
# free. The vendored stack is self-contained: the module imports only the
# kernel's C ABI, so the .ko loads on a kernel that has never heard of Rust.
#
# GATE: only vendor when the kernel is genuinely CONFIG_RUST=n. When the kernel
# ships its own Rust, use it — vendoring would bake a second copy of the Rust
# stdlib into the module's ring-0 footprint for no reason.
#
# TOOLCHAIN: the vendored stack is built with one pinned toolchain (rustc + its
# matching rust-src, plus bindgen), independent of the target kernel's version —
# the rmeta is self-contained, so there is no per-kernel rustc matching as on the
# CONFIG_RUST=y path. rustc and its rust-src MUST come from the same install (a
# mismatch miscompiles core — E0232 'append_const_msg'); RUST_LIB_SRC therefore
# defaults to the active rustc's own sysroot source to enforce that. The caller
# is responsible for providing the C build prerequisites `make rust/` needs
# (elfutils/libelf for objtool, openssl, zlib, zstd, bc, pkg-config); on NixOS,
# wrap the invocation in the matching nix-shell.
#
# Idempotent: re-running against an already-vendored tree (or resuming after a
# failed run) does the right thing — see the marker / rust.kernel-orig logic.

set -e

# --best-effort (passed by the auto build hooks): never fail the caller's build.
# When we can't vendor — no Rust toolchain, an unconfigured tree, a broken rust/
# build — warn and exit 0 so bcachefs builds C-only; the absence is already
# visible at runtime ("built without Rust support") and via the Rust-gated unit
# tests. Without the flag (explicit/standalone use) those conditions are errors.
best_effort=0
if [ "$1" = --best-effort ]; then best_effort=1; shift; fi

fail() {
	echo "$0: $1" >&2
	if [ "$best_effort" = 1 ]; then
		echo "$0: continuing without vendored Rust (C-only module)" >&2
		exit 0
	fi
	exit 1
}

# Target kernel build tree: an explicit arg wins (the interactive build passes
# $(KDIR)); else DKMS's $kernelver (it runs PRE_BUILD with that set, no arg);
# else the running kernel.
if [ -n "$1" ]; then
	KDIR=$1
elif [ -n "$kernelver" ]; then
	KDIR=/lib/modules/$kernelver/build
else
	KDIR=/lib/modules/$(uname -r)/build
fi

scriptdir=$(cd "$(dirname "$0")" && pwd)
vendor=$scriptdir/../vendor/kernel-rust

[ -r "$vendor/kernel/lib.rs" ] ||
	fail "vendored rust sources not found at $vendor"
[ -r "$KDIR/.config" ] ||
	fail "$KDIR/.config not found — not a configured kernel build tree?"

# We move the kernel's own rust/ aside to rust.kernel-orig before swapping ours
# in, so the tree is reversible AND so its presence marks "bcachefs owns this
# tree's rust/ now". The marker file (written last) means "fully vendored".
orig=$KDIR/rust.kernel-orig
marker=$KDIR/rust/.bcachefs-vendored

# Fully vendored already: nothing to do.
if [ -e "$marker" ]; then
	echo "$0: $KDIR already vendored; nothing to do"
	exit 0
fi

# Untouched tree that ships its own rust: use it, don't vendor. (We skip this
# early-exit once we've started vendoring — $orig present — so a run that failed
# after flipping CONFIG_RUST=y but before the marker is resumed, not mistaken
# for a kernel that natively has rust.)
if [ ! -e "$orig" ] && grep -q '^CONFIG_RUST=y' "$KDIR/.config"; then
	echo "$0: $KDIR has CONFIG_RUST=y; using the kernel's own rust"
	exit 0
fi

RUSTC=${RUSTC:-rustc}
BINDGEN=${BINDGEN:-bindgen}
case "$(uname -m)" in
	x86_64)  ARCH=${ARCH:-x86_64} ;;
	aarch64) ARCH=${ARCH:-arm64} ;;
	*)       ARCH=${ARCH:-$(uname -m)} ;;
esac

# rustc and its rust-src MUST match — a mismatch miscompiles core (E0232,
# 'append_const_msg'). So DERIVE RUST_LIB_SRC from rustc's own sysroot whenever
# that sysroot bundles the source; deliberately do NOT honor a pre-existing
# RUST_LIB_SRC in that case, because a stale value left in the environment is
# exactly the trap that silently builds core from the wrong source. Only fall
# back to an explicit RUST_LIB_SRC when rustc ships without rust-src (a split
# install) — and warn, since the version match can't then be guaranteed.
rustc_src=$("$RUSTC" --print sysroot)/lib/rustlib/src/rust/library
if [ -r "$rustc_src/core/src/lib.rs" ]; then
	RUST_LIB_SRC=$rustc_src
elif [ -n "$RUST_LIB_SRC" ] && [ -r "$RUST_LIB_SRC/core/src/lib.rs" ]; then
	echo "$0: warning: $RUSTC bundles no rust-src; using RUST_LIB_SRC=$RUST_LIB_SRC" >&2
	echo "    — ensure it matches $RUSTC's version, or core miscompiles (E0232)" >&2
else
	fail "rust-src not found in $RUSTC's sysroot ($rustc_src); install the rust-src component for $RUSTC, or set RUST_LIB_SRC to a matching source tree"
fi

echo "$0: vendoring bcachefs rust into $KDIR (CONFIG_RUST=n -> vendored stack)"
echo "    rustc:    $("$RUSTC" --version 2>/dev/null) [$RUSTC]"
echo "    bindgen:  $("$BINDGEN" --version 2>/dev/null) [$BINDGEN]"
echo "    rust-src: $RUST_LIB_SRC"
echo "    arch:     $ARCH"

# 1. Swap the vendored rust/ source into the target tree, preserving the
#    kernel's own rust/ (if any) so the change is reversible. Copy (not symlink):
#    the build writes generated files + artifacts into rust/, which must not
#    pollute the committed vendor dir.
if [ -d "$KDIR/rust" ] && [ ! -e "$orig" ]; then
	mv "$KDIR/rust" "$orig"
fi
rm -rf "$KDIR/rust"
mkdir -p "$KDIR/rust"
cp -a "$vendor/." "$KDIR/rust/"

# 2. Flip CONFIG_RUST on. olddefconfig settles the cascade (4 rust-internal
#    symbols, no C-ABI drift) and computes CONFIG_RUSTC_VERSION from our rustc —
#    so pass RUSTC/BINDGEN here too, or Kconfig's rust-availability check runs
#    against the wrong (or no) toolchain and silently turns CONFIG_RUST back off.
"$KDIR/scripts/config" --file "$KDIR/.config" --enable RUST
make -C "$KDIR" ARCH="$ARCH" RUSTC="$RUSTC" BINDGEN="$BINDGEN" olddefconfig

grep -q '^CONFIG_RUST=y' "$KDIR/.config" ||
	fail "CONFIG_RUST did not stick after olddefconfig — Kconfig judged the rust toolchain unavailable; check that $RUSTC and $BINDGEN satisfy $KDIR/scripts/rust_is_available.sh"

# 3. Build the vendored rust stack into $KDIR/rust/*.rmeta.
#    -Zunstable-options: 6.18's rust/Makefile omits it, but rustc >= 1.95 needs
#    it to load a custom target.json.
#    -Awarnings: the older-kernel trims (fs/vendor/kernel-rust) leave dead-code /
#    unused-import warnings that the kernel's -Dwarnings would otherwise make
#    fatal.
make -C "$KDIR" ARCH="$ARCH" \
	RUSTC="$RUSTC" BINDGEN="$BINDGEN" RUST_LIB_SRC="$RUST_LIB_SRC" \
	KRUSTFLAGS='-Zunstable-options -Awarnings' \
	-j"$(nproc)" rust/ ||
	fail "'make rust/' failed building the vendored stack"

touch "$marker"
echo "$0: done — $KDIR/rust has the vendored rmeta and CONFIG_RUST=y;"
echo "    build bcachefs against $KDIR as usual (the CONFIG_RUST=y path)."
