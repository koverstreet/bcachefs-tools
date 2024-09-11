#!/usr/bin/env bash
set -euxo pipefail

blkdev="/dev/vdb"
blkdev2="/dev/vdc"
mnt=$(mktemp -d)
pw=$(genpass)
uuid=$(uuidgen)

# link user and session keyrings so that the key can be found by the kernel
keyctl link @u @s

sfdisk "$blkdev" <<EOF
label: gpt
type=linux, size=1G
type=linux, size=1G
type=linux
EOF

udevadm settle

echo "$pw" | bcachefs format \
    --verbose \
    --encrypted \
    --replicas=2 \
    --uuid "$uuid" \
    --fs_label test-fs \
    "${blkdev}"{1,2}

udevadm settle

echo "$pw" | bcachefs mount "UUID=$uuid" "$mnt"

bcachefs device add "$mnt" "${blkdev}3"
bcachefs device add "$mnt" "$blkdev2"

udevadm settle

blkid

keyctl search @u user "bcachefs:$uuid"

umount "$mnt"

bcachefs mount "UUID=$uuid" "$mnt"
