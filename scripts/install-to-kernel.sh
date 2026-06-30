#!/bin/sh
# install-to-kernel.sh — add bcachefs to a Linux kernel source tree so it builds
# in-tree (CONFIG_BCACHEFS_FS).
#
# bcachefs is currently maintained out of mainline, so people who want to build
# their own kernel with bcachefs support have to re-add it by hand. This does
# that: it copies this checkout's fs/ to <kernel>/fs/bcachefs/ and wires it into
# the kernel's fs/Kconfig and fs/Makefile.
#
# The copied fs/Makefile builds correctly in-tree as-is: its DKMS / out-of-tree
# machinery is all guarded by `ifdef BCACHEFS_DKMS`, which an in-tree build never
# sets, so CONFIG_BCACHEFS_FS comes from the kernel's own .config like any other
# filesystem. The userspace-tools scaffolding that also lives under fs/ (Rust
# codegen, vendored crates) is inert to kbuild — it only builds what's in the
# bcachefs-y object list.
#
# Idempotent: re-running refreshes the copied source and leaves the Kconfig /
# Makefile edits in place without duplicating them.
#
# Usage: scripts/install-to-kernel.sh /path/to/linux

set -e

die() { echo "install-to-kernel: $*" >&2; exit 1; }

ktree=${1:?usage: install-to-kernel.sh <kernel source tree>}
src=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)	# bcachefs-tools root

[ -d "$src/fs" ] ||
	die "no fs/ under $src — run this from a bcachefs-tools checkout"
[ -f "$ktree/Makefile" ] && [ -f "$ktree/fs/Kconfig" ] && [ -f "$ktree/fs/Makefile" ] ||
	die "$ktree doesn't look like a kernel source tree (no fs/Kconfig)"

dst=$ktree/fs/bcachefs
echo "install-to-kernel: copying fs/ -> $dst"
rm -rf "$dst"
cp -a "$src/fs" "$dst"

# fs/Kconfig: source bcachefs's Kconfig from inside the block-filesystem section
# (bcachefs depends on BLOCK). Anchor after btrfs, which is reliably present and
# block-dependent.
kconfig=$ktree/fs/Kconfig
if grep -q 'fs/bcachefs/Kconfig' "$kconfig"; then
	echo "install-to-kernel: fs/Kconfig already sources bcachefs"
else
	grep -q '^source "fs/btrfs/Kconfig"$' "$kconfig" ||
		die "no 'source \"fs/btrfs/Kconfig\"' anchor in $kconfig — patch it by hand"
	sed -i '/^source "fs\/btrfs\/Kconfig"$/a source "fs/bcachefs/Kconfig"' "$kconfig"
	echo "install-to-kernel: added source line to fs/Kconfig"
fi

# fs/Makefile: descend into bcachefs/ when CONFIG_BCACHEFS_FS is set.
kmakefile=$ktree/fs/Makefile
if grep -q 'CONFIG_BCACHEFS_FS' "$kmakefile"; then
	echo "install-to-kernel: fs/Makefile already builds bcachefs"
else
	grep -q '^obj-.*+= btrfs/$' "$kmakefile" ||
		die "no 'obj-\$(CONFIG_BTRFS_FS) += btrfs/' anchor in $kmakefile — patch it by hand"
	sed -i '/^obj-.*+= btrfs\/$/a obj-$(CONFIG_BCACHEFS_FS)\t+= bcachefs/' "$kmakefile"
	echo "install-to-kernel: added obj line to fs/Makefile"
fi

echo "install-to-kernel: done — now enable CONFIG_BCACHEFS_FS in your kernel config"
