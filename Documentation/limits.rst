.. SPDX-License-Identifier: GPL-2.0

Limits and on-disk geometry
===========================

This page records user-visible limits that are fixed by the current bcachefs
source. It is not a sizing recommendation; practical filesystem size and file
count limits also depend on block size, bucket size, available metadata space,
replication settings, erasure coding settings, and kernel/VFS limits.

Names and paths
---------------

File and directory names are limited to ``BCH_NAME_MAX`` bytes, currently 512
bytes. This is a byte limit, not a character count; multibyte UTF-8 characters
consume more than one byte.

Like other Linux filesystems, path component names cannot contain ``/`` or
NUL. ``/`` is the path separator and NUL terminates userspace strings passed to
the kernel.

Regular file size
-----------------

bcachefs reports ``MAX_LFS_FILESIZE`` as its maximum regular file size to the
VFS, currently ``LLONG_MAX`` bytes: 8 EiB minus 1 byte.

Devices and encoded filesystem size
-----------------------------------

The current superblock member table supports up to ``BCH_SB_MEMBERS_MAX``
devices, currently 256.

For each member device, the on-disk member record stores:

* ``nbuckets`` as a 64-bit value, with current validation rejecting values above
  ``BCH_MEMBER_NBUCKETS_MAX`` (``INT_MAX - 64`` buckets).
* ``bucket_size`` as a 16-bit sector count.

Those field limits give an encoded per-device ceiling of about 64 PiB with the
largest representable bucket size, and an encoded total ceiling just below 16
EiB across 256 member slots. These are on-disk format ceilings, not promises
that every kernel, block device stack, memory configuration, or tool path can
usefully operate a filesystem at that size.

File count and link count
-------------------------

There is no separately configured "maximum number of files" setting. The
practical file count is limited by available space for file data, extents,
directory entries, and other metadata.

The current bcachefs link-count ceiling is ``BCH_LINK_MAX``, defined as
``U32_MAX``. Directory structure and VFS behavior may impose tighter practical
limits for a particular workload.

Journaling
----------

bcachefs journals metadata updates through its journal and replays that journal
during recovery when needed.

User data is not journaled by first copying complete file data into a separate
data journal. Data writes are copy-on-write: data blocks are written to their
final locations, and then the metadata describing those extents is committed
through the journal.
