#!/bin/bash
# Build the DKMS module against a kernel configured with CONFIG_RUST=n, the way
# distro kernels without Rust are (e.g. CachyOS's ThinLTO kernel), so the module
# builds bcachefs's vendored Rust stack with the rustc on PATH.
#
# usage: build-on-kernel-without-rust.sh <kernel version> <rustc-at-config: none|present> [workdir]
#
# "none" configures the kernel with no rustc, leaving CONFIG_RUSTC_VERSION=0,
# as a distro build without a Rust toolchain does. "present" configures it with
# the rustc on PATH and Rust turned off.
set -euo pipefail

kver=$1
rustc_at_config=$2
work=${3:-$(mktemp -d)}
src=$(cd "$(dirname "$0")/.." && pwd)
jobs=$(nproc)

mkdir -p "$work"
cd "$work"

if [[ ! -d linux-$kver ]]; then
	[[ -f linux-$kver.tar.xz ]] ||
		curl -sSfLo linux-$kver.tar.xz "https://cdn.kernel.org/pub/linux/kernel/v${kver%%.*}.x/linux-$kver.tar.xz"
	tar xJf linux-$kver.tar.xz
fi
kdir=$work/linux-$kver

case $rustc_at_config in
none)		cfg_rustc=(RUSTC=/nonexistent/rustc) ;;
present)	cfg_rustc=() ;;
*)		echo "rustc-at-config must be none or present" >&2; exit 2 ;;
esac

make -C "$kdir" -s "${cfg_rustc[@]}" defconfig
"$kdir"/scripts/config --file "$kdir"/.config -d RUST -e MODULES
make -C "$kdir" -s "${cfg_rustc[@]}" olddefconfig
make -C "$kdir" -s -j"$jobs" "${cfg_rustc[@]}" modules_prepare
grep -E '^CONFIG_RUSTC_VERSION=|^CONFIG_RUST=' "$kdir"/.config || true

rm -rf dkms-root
make -C "$src" -s install_dkms DESTDIR="$work"/dkms-root
dkmsdir=$(echo "$work"/dkms-root/usr/src/bcachefs-*)

rustc --version
# modules_prepare builds no vmlinux, so there is no Module.symvers and every
# kernel symbol looks undefined to modpost.
make -C "$dkmsdir" -j"$jobs" KDIR="$kdir" KBUILD_MODPOST_WARN=1
ls -l "$dkmsdir"/src/fs/bcachefs/bcachefs.ko
