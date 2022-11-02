Introduction
=========================

Bcachefs is a modern, general purpose, copy on write filesystem
descended from bcache, a block layer cache.

The internal architecture is very different from most existing
filesystems where the inode is central and many data structures hang off
of the inode. Instead, bcachefs is architected more like a filesystem on
top of a relational database, with tables for the different filesystem
data types - extents, inodes, dirents, xattrs, et cetera.

bcachefs supports almost all of the same features as other modern COW
filesystems, such as ZFS and btrfs, but in general with a cleaner,
simpler, higher performance design.
