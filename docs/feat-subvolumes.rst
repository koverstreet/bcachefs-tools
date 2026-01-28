Subvolumes and snapshots
------------------------

bcachefs supports subvolumes and snapshots with a similar userspace
interface as btrfs. A new subvolume may be created empty, or it may be
created as a snapshot of another subvolume. Snapshots are writeable and
may be snapshotted again, creating a tree of snapshots.

Snapshots are very cheap to create: they’re not based on cloning of COW
btrees as with btrfs, but instead are based on versioning of individual
keys in the btrees. Many thousands or millions of snapshots can be
created, with the only limitation being disk space.

The following subcommands exist for managing subvolumes and snapshots:

-  ``bcachefs subvolume create``: Create a new, empty subvolume

-  ``bcachefs subvolume destroy``: Delete an existing subvolume or
   snapshot

-  ``bcachefs subvolume snapshot``: Create a snapshot of an existing
   subvolume

A subvolume can also be deleting with a normal rmdir after deleting all
the contents, as with ``rm -rf``. Still to be implemented: read-only
snapshots, recursive snapshot creation, and a method for recursively
listing subvolumes.
