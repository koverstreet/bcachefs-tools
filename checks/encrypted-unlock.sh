#!/usr/bin/env bash
set -euxo pipefail

blkdev="/dev/vdb"
mnt=$(mktemp -d)
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

bcachefs unlock -c "$blkdev"

echo "$pw" | bcachefs unlock "$blkdev"
key_id=$(keyctl search @u user "bcachefs:$uuid")

bcachefs mount "$blkdev" "$mnt"
umount "$mnt"

keyctl unlink "$key_id"

echo "$pw" | bcachefs unlock -k session "$blkdev"
key_id=$(keyctl search @s user "bcachefs:$uuid")

mount -t bcachefs "$blkdev" "$mnt"
umount "$mnt"

keyctl unlink "$key_id"

bcachefs mount -f <(echo "$pw") "$blkdev" "$mnt"
key_id=$(keyctl search @u user "bcachefs:$uuid")
umount "$mnt"
keyctl unlink "$key_id"

echo "$pw" | bcachefs mount -k stdin "$blkdev" "$mnt"
key_id=$(keyctl search @u user "bcachefs:$uuid")
umount "$mnt"
keyctl unlink "$key_id"

echo "$pw" | bcachefs mount "$blkdev" "$mnt"
key_id=$(keyctl search @u user "bcachefs:$uuid")
umount "$mnt"
bcachefs mount -k fail "$blkdev"
bcachefs mount -k wait "$blkdev" "$mnt"
umount "$mnt"
keyctl unlink "$key_id"
