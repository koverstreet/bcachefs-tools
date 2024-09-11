#!/usr/bin/env bash
set -euxo pipefail

blkdev="/dev/vdb"
mnt1=$(mktemp -d)
mnt2=$(mktemp -d)
pw=$(genpass)
uuid=$(uuidgen)

# link user and session keyrings so that the key can be found by the kernel
keyctl link @u @s

echo "$pw" | bcachefs format \
    --verbose \
    --encrypted \
    --uuid "$uuid" \
    --fs_label test-fs \
    "$blkdev"

udevadm settle

echo "$pw" | bcachefs mount "UUID=$uuid" "$mnt1"

fallocate --length 2G "$mnt1/fs.img"

bcachefs format \
    --verbose \
    "$mnt1/fs.img"

loopdev=$(losetup --find --show "$mnt1/fs.img")

udevadm settle

mount "$loopdev" "$mnt2"

f3write "$mnt1"
f3write "$mnt2"

f3read "$mnt1"
f3read "$mnt2"
