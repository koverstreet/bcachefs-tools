#!/bin/sh

# SPDX-License-Identifier: GPL-2.0
#
# Standalone driver for the MT compression micro-benchmark.
#
# Mounts a bcachefs filesystem, triggers fs/debug/mt_compress_bench.c in
# the kernel module via its sysfs entry, and prints a pass/fail summary
# with timing numbers.  Designed to be runnable by hand outside of the
# debian/tests framework:
#
#   tests/mt_compress_bench.sh                  # default codecs (lz4, zstd:3, gzip:6)
#   tests/mt_compress_bench.sh lz4             # one specific codec
#   tests/mt_compress_bench.sh lz4 zstd:9      # several codecs
#
# Exits 0 on PASS for all requested modes, 1 otherwise.  Modes that the
# kernel marks SKIP (e.g. WQ has < 2 workers) are reported but do not
# fail the script.
#
# Requirements:
#   - root (for losetup / mount)
#   - bcachefs module built with BCACHEFS_TESTS=1
#   - the fs/debug/mt_compress_bench.o file present in the loaded module

set +e

modes="${*:-lz4 zstd:3 gzip:6}"
echo "MT compression micro-benchmark: ${modes}"

STORAGE_SIZE_MB=256
STORAGE="$(mktemp)"
dd if=/dev/zero of="$STORAGE" bs=1M count=$STORAGE_SIZE_MB > /dev/null 2>&1
LODEVICE="$(losetup --find --show $STORAGE)"

MOUNTPOINT=""
cleanup() {
	if [ -n "$MOUNTPOINT" ] && mountpoint -q "$MOUNTPOINT" 2>/dev/null; then
		umount "$MOUNTPOINT" 2>/dev/null
	fi
	losetup -d "$LODEVICE" 2>/dev/null
	rm -rf "${MOUNTPOINT:-}" 2>/dev/null
}
trap cleanup EXIT

bcachefs format --compression=zstd "$LODEVICE" > /dev/null 2>&1
if [ $? -ne 0 ]; then
	echo "FAILED: bcachefs format failed"
	exit 1
fi

MOUNTPOINT="$(mktemp -d)"
mount -t bcachefs "$LODEVICE" "$MOUNTPOINT"
if [ $? -ne 0 ]; then
	echo "FAILED: bcachefs mount failed"
	exit 1
fi

SYSFS_BASE=""
for i in 1 2 3 4 5 10 20 50; do
	SYSFS_BASE="$(ls -d /sys/fs/bcachefs/*/mt_compress_bench 2>/dev/null | head -1)"
	[ -n "$SYSFS_BASE" ] && break
	sleep 0.1
done
if [ -z "$SYSFS_BASE" ]; then
	echo "FAILED: mt_compress_bench sysfs attribute not present"
	echo "  (kernel module was likely built without BCACHEFS_TESTS=1)"
	exit 1
fi

OVERALL=0
for mode in $modes; do
	echo
	echo "=== mode=$mode ==="

	dmesg -c > /dev/null 2>&1
	echo "$mode" > "$SYSFS_BASE"

	BENCH_OUT="$(dmesg | grep '^MT_COMPRESS_BENCH:' || true)"
	if [ -z "$BENCH_OUT" ]; then
		echo "FAILED: no MT_COMPRESS_BENCH: output for mode=$mode"
		dmesg | tail -50
		OVERALL=1
		continue
	fi

	printf '%s\n' "$BENCH_OUT"

	last_line="$(printf '%s\n' "$BENCH_OUT" | tail -1)"
	case "$last_line" in
		*PASS*) : ;;
		*SKIP*) echo "  (skipped - parallel path not meaningful here)";;
		*FAIL*) OVERALL=1;;
		*)      OVERALL=1;;
	esac
done

if [ "$OVERALL" -eq 0 ]; then
	echo
	echo "ALL MODES PASSED"
	exit 0
else
	echo
	echo "AT LEAST ONE MODE FAILED"
	exit 1
fi
