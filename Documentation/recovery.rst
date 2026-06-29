.. SPDX-License-Identifier: GPL-2.0

Recovery notes
==============

Damaged superblocks
-------------------

If one member reports a default superblock checksum error after an unclean
shutdown, stop and preserve evidence before attempting repair. Do not run
multiple mounts or fsck processes against the same filesystem at the same time:
superblock sequence numbers are used to detect concurrent modification, and a
repair attempt can be aborted if another process writes a newer superblock.

Useful first checks are:

.. code-block:: bash

  bcachefs show-super /dev/bcachefs-member
  bcachefs show-super -l /dev/bcachefs-member

Run those checks on every available member. If the primary superblock on one
device is unreadable but another copy or member still validates and shows the
same filesystem UUID and member list, ``bcachefs recover-super`` can rewrite the
damaged member's superblock from a backup copy or from another member:

.. code-block:: bash

  bcachefs recover-super /dev/damaged-member
  bcachefs recover-super --src_device /dev/good-member --dev_idx N /dev/damaged-member

Use the second form when the damaged member's own backup copies are not usable.
``N`` is the damaged member's device index from the filesystem member list. The
command prints the superblock it found and asks for confirmation unless
``--yes`` is supplied.

After recovering a member superblock, run offline fsck before mounting
read-write:

.. code-block:: bash

  bcachefs fsck /dev/member0:/dev/member1:...

If the filesystem cannot start because a member is missing or unreadable, use
the normal degraded recovery options only after verifying which member copies
are present. Prefer a read-only/nochanges attempt while collecting evidence; use
more aggressive degraded modes only when the missing-data risk is understood.

Whenever possible, make block-device images or hardware snapshots first. A
superblock repair changes on-disk metadata, and a failed recovery attempt is
much easier to debug when the original bytes are still available.
