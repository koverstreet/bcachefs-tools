


Device management
-----------------

Filesystem resize
~~~~~~~~~~~~~~~~~

A filesystem can be resized on a particular device with the
``bcachefs device resize`` subcommand. Currently only growing is
supported, not shrinking.

Device add/removal
~~~~~~~~~~~~~~~~~~

The following subcommands exist for adding and removing devices from a
mounted filesystem:

-  ``bcachefs device add``: Formats and adds a new device to an existing
   filesystem.

-  ``bcachefs device remove``: Permenantly removes a device from an
   existing filesystem.

-  ``bcachefs device online``: Connects a device to a running filesystem
   that was mounted without it (i.e. in degraded mode)

-  ``bcachefs device offline``: Disconnects a device from a mounted
   filesystem without removing it.

-  ``bcachefs device evacuate``: Migrates data off of a particular
   device to prepare for removal, setting it read-only if necessary.

-  ``bcachefs device set-state``: Changes the state of a member device:
   one of rw (readwrite), ro (readonly), failed, or spare.

   A failed device is considered to have 0 durability, and replicas on
   that device won’t be counted towards the number of replicas an extent
   should have by rereplicate - however, bcachefs will still attempt to
   read from devices marked as failed.

The ``bcachefs device remove``, ``bcachefs device offline`` and
``bcachefs device set-state`` commands take force options for when they
would leave the filesystem degraded or with data missing. Todo:
regularize and improve those options.

Data management
---------------

Data rereplicate
~~~~~~~~~~~~~~~~

The ``bcachefs data rereplicate`` command may be used to scan for
extents that have insufficient replicas and write additional replicas,
e.g. after a device has been removed from a filesystem or after
replication has been enabled or increased.

Rebalance
~~~~~~~~~

To be implemented: a command for moving data between devices to equalize
usage on each device. Not normally required because the allocator
attempts to equalize usage across devices as it stripes, but can be
necessary in certain scenarios - i.e. when a two-device filesystem with
replication enabled that is very full has a third device added.

Scrub
~~~~~

To be implemented: a command for reading all data within a filesystem
and ensuring that checksums are valid, fixing bitrot when a valid copy
can be found.

Options
=======

Most bcachefs options can be set filesystem wide, and a significant
subset can also be set on inodes (files and directories), overriding the
global defaults. Filesystem wide options may be set when formatting,
when mounting, or at runtime via ``/sys/fs/bcachefs/<uuid>/options/``.
When set at runtime via sysfs the persistent options in the superblock
are updated as well; when options are passed as mount parameters the
persistent options are unmodified.

File and directory options
--------------------------

<say something here about how attrs must be set via bcachefs attr
command>

Options set on inodes (files and directories) are automatically
inherited by their descendants, and inodes also record whether a given
option was explicitly set or inherited from their parent. When renaming
a directory would cause inherited attributes to change we fail the
rename with -EXDEV, causing userspace to do the rename file by file so
that inherited attributes stay consistent.

Inode options are available as extended attributes. The options that
have been explicitly set are available under the ``bcachefs`` namespace,
and the effective options (explicitly set and inherited options) are
available under the ``bcachefs_effective`` namespace. Examples of
listing options with the getfattr command:

   ::

      $ getfattr -d -m '^bcachefs\.' filename
      $ getfattr -d -m '^bcachefs_effective\.' filename

Options may be set via the extended attribute interface, but it is
preferable to use the ``bcachefs setattr`` command as it will correctly
propagate options recursively.

Full option list
----------------

.. container:: tabbing

   | ̄ ``block_size`` **format**

   Filesystem block size (default 4k)

   | 
   | ``btree_node_size`` **format**
   | Btree node size, default 256k
   | ``errors`` **format,mount,rutime**
   | Action to take on filesystem error
   | ``metadata_replicas`` **format,mount,runtime**
   | Number of replicas for metadata (journal and btree)
   | ``data_replicas`` **format,mount,runtime,inode**
   | Number of replicas for user data
   | ``replicas`` **format**
   | Alias for both metadata_replicas and data_replicas
   | ``metadata_checksum`` **format,mount,runtime**
   | Checksum type for metadata writes
   | ``data_checksum`` **format,mount,runtime,inode**
   | Checksum type for data writes
   | ``compression`` **format,mount,runtime,inode**
   | Compression type
   | ``background_compression`` **format,mount,runtime,inode**
   | Background compression type
   | ``str_hash`` **format,mount,runtime,inode**
   | Hash function for string hash tables (directories and xattrs)
   | ``metadata_target`` **format,mount,runtime,inode**
   | Preferred target for metadata writes
   | ``foreground_target`` **format,mount,runtime,inode**
   | Preferred target for foreground writes
   | ``background_target`` **format,mount,runtime,inode**
   | Target for data to be moved to in the background
   | ``promote_target`` **format,mount,runtime,inode**
   | Target for data to be copied to on read
   | ``erasure_code`` **format,mount,runtime,inode**
   | Enable erasure coding
   | ``inodes_32bit`` **format,mount,runtime**
   | Restrict new inode numbers to 32 bits
   | ``shard_inode_numbers`` **format,mount,runtime**
   | Use CPU id for high bits of new inode numbers.
   | ``wide_macs`` **format,mount,runtime**
   | Store full 128 bit cryptographic MACs (default 80)
   | ``inline_data`` **format,mount,runtime**
   | Enable inline data extents (default on)
   | ``journal_flush_delay`` **format,mount,runtime**
   | Delay in milliseconds before automatic journal commit (default
     1000)
   | ``journal_flush_disabled``\ **format,mount,runtime**

   Disables journal flush on sync/fsync. ``journal_flush_delay`` remains
   in effect, thus with the default setting not more than 1 second of
   work will be lost.

   | 
   | ``journal_reclaim_delay``\ **format,mount,runtime**
   | Delay in milliseconds before automatic journal reclaim
   | ``acl`` **format,mount**
   | Enable POSIX ACLs
   | ``usrquota`` **format,mount**
   | Enable user quotas
   | ``grpquota`` **format,mount**
   | Enable group quotas
   | ``prjquota`` **format,mount**
   | Enable project quotas
   | ``degraded`` **mount**
   | Allow mounting with data degraded
   | ``very_degraded`` **mount**
   | Allow mounting with data missing
   | ``verbose`` **mount**
   | Extra debugging info during mount/recovery
   | ``fsck`` **mount**
   | Run fsck during mount
   | ``fix_errors`` **mount**
   | Fix errors without asking during fsck
   | ``ratelimit_errors`` **mount**
   | Ratelimit error messages during fsck
   | ``read_only`` **mount**
   | Mount in read only mode
   | ``nochanges`` **mount**
   | Issue no writes, even for journal replay
   | ``norecovery`` **mount**
   | Don’t replay the journal (not recommended)
   | ``noexcl`` **mount**
   | Don’t open devices in exclusive mode
   | ``version_upgrade`` **mount**
   | Upgrade on disk format to latest version
   | ``discard`` **device**
   | Enable discard/TRIM support

Error actions
-------------

The ``errors`` option is used for inconsistencies that indicate some
sort of a bug. Valid error actions are:

``continue``
   Log the error but continue normal operation

``ro``
   Emergency read only, immediately halting any changes to the
   filesystem on disk

``panic``
   Immediately halt the entire machine, printing a backtrace on the
   system console

Checksum types
--------------

Valid checksum types are:

``none``
``crc32c``
   (default)

``crc64``

Compression types
-----------------

Valid compression types are:

``none``
   (default)

``lz4``
``gzip``
``zstd``

String hash types
-----------------

Valid hash types for string hash tables are:

``crc32c``
``crc64``
``siphash``
   (default)

Debugging tools
===============

Sysfs interface
---------------

Mounted filesystems are available in sysfs at
``/sys/fs/bcachefs/<uuid>/`` with various options, performance counters
and internal debugging aids.

.. _options-1:

Options
~~~~~~~

| Filesystem options may be viewed and changed via
| ``/sys/fs/bcachefs/<uuid>/options/``, and settings changed via sysfs
  will be persistently changed in the superblock as well.

Time stats
~~~~~~~~~~

bcachefs tracks the latency and frequency of various operations and
events, with quantiles for latency/duration in the
``/sys/fs/bcachefs/<uuid>/time_stats/`` directory.

.. container:: description

   | ``blocked_allocate``
   | Tracks when allocating a bucket must wait because none are
     immediately available, meaning the copygc thread is not keeping up
     with evacuating mostly empty buckets or the allocator thread is not
     keeping up with invalidating and discarding buckets.

   | ``blocked_allocate_open_bucket``
   | Tracks when allocating a bucket must wait because all of our
     handles for pinning open buckets are in use (we statically allocate
     1024).

   | ``blocked_journal``
   | Tracks when getting a journal reservation must wait, either because
     journal reclaim isn’t keeping up with reclaiming space in the
     journal, or because journal writes are taking too long to complete
     and we already have too many in flight.

   | ``btree_gc``
   | Tracks when the btree_gc code must walk the btree at runtime - for
     recalculating the oldest outstanding generation number of every
     bucket in the btree.

   ``btree_lock_contended_read``

   ``btree_lock_contended_intent``

   | ``btree_lock_contended_write``
   | Track when taking a read, intent or write lock on a btree node must
     block.

   | ``btree_node_mem_alloc``
   | Tracks the total time to allocate memory in the btree node cache
     for a new btree node.

   | ``btree_node_split``
   | Tracks btree node splits - when a btree node becomes full and is
     split into two new nodes

   | ``btree_node_compact``
   | Tracks btree node compactions - when a btree node becomes full and
     needs to be compacted on disk.

   | ``btree_node_merge``
   | Tracks when two adjacent btree nodes are merged.

   | ``btree_node_sort``
   | Tracks sorting and resorting entire btree nodes in memory, either
     after reading them in from disk or for compacting prior to creating
     a new sorted array of keys.

   | ``btree_node_read``
   | Tracks reading in btree nodes from disk.

   | ``btree_interior_update_foreground``
   | Tracks foreground time for btree updates that change btree topology
     - i.e. btree node splits, compactions and merges; the duration
     measured roughly corresponds to lock held time.

   | ``btree_interior_update_total``
   | Tracks time to completion for topology changing btree updates;
     first they have a foreground part that updates btree nodes in
     memory, then after the new nodes are written there is a transaction
     phase that records an update to an interior node or a new btree
     root as well as changes to the alloc btree.

   | ``data_read``
   | Tracks the core read path - looking up a request in the extents
     (and possibly also reflink) btree, allocating bounce buffers if
     necessary, issuing reads, checksumming, decompressing, decrypting,
     and delivering completions.

   | ``data_write``
   | Tracks the core write path - allocating space on disk for a new
     write, allocating bounce buffers if necessary, compressing,
     encrypting, checksumming, issuing writes, and updating the extents
     btree to point to the new data.

   | ``data_promote``
   | Tracks promote operations, which happen when a read operation
     writes an additional cached copy of an extent to
     ``promote_target``. This is done asynchronously from the original
     read.

   | ``journal_flush_write``
   | Tracks writing of flush journal entries to disk, which first issue
     cache flush operations to the underlying devices then issue the
     journal writes as FUA writes. Time is tracked starting from after
     all journal reservations have released their references or the
     completion of the previous journal write.

   | ``journal_noflush_write``
   | Tracks writing of non-flush journal entries to disk, which do not
     issue cache flushes or FUA writes.

   | ``journal_flush_seq``
   | Tracks time to flush a journal sequence number to disk by
     filesystem sync and fsync operations, as well as the allocator
     prior to reusing buckets when none that do not need flushing are
     available.

Internals
~~~~~~~~~

.. container:: description

   | ``btree_cache``
   | Shows information on the btree node cache: number of cached nodes,
     number of dirty nodes, and whether the cannibalize lock (for
     reclaiming cached nodes to allocate new nodes) is held.

   | ``dirty_btree_nodes``
   | Prints information related to the interior btree node update
     machinery, which is responsible for ensuring dependent btree node
     writes are ordered correctly.

   For each dirty btree node, prints:

   -  Whether the ``need_write`` flag is set

   -  The level of the btree node

   -  The number of sectors written

   -  Whether writing this node is blocked, waiting for other nodes to
      be written

   -  Whether it is waiting on a btree_update to complete and make it
      reachable on-disk

   | ``btree_key_cache``
   | Prints infromation on the btree key cache: number of freed keys
     (which must wait for a sRCU barrier to complete before being
     freed), number of cached keys, and number of dirty keys.

   | ``btree_transactions``
   | Lists each running btree transactions that has locks held, listing
     which nodes they have locked and what type of lock, what node (if
     any) the process is blocked attempting to lock, and where the btree
     transaction was invoked from.

   | ``btree_updates``
   | Lists outstanding interior btree updates: the mode (nothing updated
     yet, or updated a btree node, or wrote a new btree root, or was
     reparented by another btree update), whether its new btree nodes
     have finished writing, its embedded closure’s refcount (while
     nonzero, the btree update is still waiting), and the pinned journal
     sequence number.

   | ``journal_debug``
   | Prints a variety of internal journal state.

   ``journal_pins`` Lists items pinning journal entries, preventing them
   from being reclaimed.

   | ``new_stripes``
   | Lists new erasure-coded stripes being created.

   | ``stripes_heap``
   | Lists erasure-coded stripes that are available to be reused.

   | ``open_buckets``
   | Lists buckets currently being written to, along with data type and
     refcount.

   | ``io_timers_read``

   | ``io_timers_write``
   | Lists outstanding IO timers - timers that wait on total reads or
     writes to the filesystem.

   | ``trigger_journal_flush``
   | Echoing to this file triggers a journal commit.

   | ``trigger_gc``
   | Echoing to this file causes the GC code to recalculate each
     bucket’s oldest_gen field.

   | ``prune_cache``
   | Echoing to this file prunes the btree node cache.

   | ``read_realloc_races``
   | This counts events where the read path reads an extent and
     discovers the bucket that was read from has been reused while the
     IO was in flight, causing the read to be retried.

   | ``extent_migrate_done``
   | This counts extents moved by the core move path, used by copygc and
     rebalance.

   | ``extent_migrate_raced``
   | This counts extents that the move path attempted to move but no
     longer existed when doing the final btree update.

Unit and performance tests
~~~~~~~~~~~~~~~~~~~~~~~~~~

Echoing into ``/sys/fs/bcachefs/<uuid>/perf_test`` runs various low
level btree tests, some intended as unit tests and others as performance
tests. The syntax is

   ::

          echo <test_name> <nr_iterations> <nr_threads> > perf_test

When complete, the elapsed time will be printed in the dmesg log. The
full list of tests that can be run can be found near the bottom of
``fs/bcachefs/tests.c``.

Debugfs interface
-----------------

The contents of every btree, as well as various internal per-btree-node
information, are available under ``/sys/kernel/debug/bcachefs/<uuid>/``.

For every btree, we have the following files:

.. container:: description

   | *btree_name*
   | Entire btree contents, one key per line

   | *btree_name*\ ``-formats``
   | Information about each btree node: the size of the packed bkey
     format, how full each btree node is, number of packed and unpacked
     keys, and number of nodes and failed nodes in the in-memory search
     trees.

   | *btree_name*\ ``-bfloat-failed``
   | For each sorted set of keys in a btree node, we construct a binary
     search tree in eytzinger layout with compressed keys. Sometimes we
     aren’t able to construct a correct compressed search key, which
     results in slower lookups; this file lists the keys that resulted
     in these failed nodes.

Listing and dumping filesystem metadata
---------------------------------------

bcachefs show-super
~~~~~~~~~~~~~~~~~~~

This subcommand is used for examining and printing bcachefs superblocks.
It takes two optional parameters:

.. container:: description

   ``-l``: Print superblock layout, which records the amount of space
   reserved for the superblock and the locations of the backup
   superblocks.

   ``-f, –fields=(fields)``: List of superblock sections to print,
   ``all`` to print all sections.

bcachefs list
~~~~~~~~~~~~~

This subcommand gives access to the same functionality as the debugfs
interface, listing btree nodes and contents, but for offline
filesystems.

bcachefs list_journal
~~~~~~~~~~~~~~~~~~~~~

This subcommand lists the contents of the journal, which primarily
records btree updates ordered by when they occured.

bcachefs dump
~~~~~~~~~~~~~

This subcommand can dump all metadata in a filesystem (including multi
device filesystems) as qcow2 images: when encountering issues that
``fsck`` can not recover from and need attention from the developers,
this makes it possible to send the developers only the required
metadata. Encrypted filesystems must first be unlocked with
``bcachefs remove-passphrase``.

ioctl interface
===============

This section documents bcachefs-specific ioctls:

.. container:: description

   | ``BCH_IOCTL_QUERY_UUID``
   | Returs the UUID of the filesystem: used to find the sysfs directory
     given a path to a mounted filesystem.

   | ``BCH_IOCTL_FS_USAGE``
   | Queries filesystem usage, returning global counters and a list of
     counters by ``bch_replicas`` entry.

   | ``BCH_IOCTL_DEV_USAGE``
   | Queries usage for a particular device, as bucket and sector counts
     broken out by data type.

   | ``BCH_IOCTL_READ_SUPER``
   | Returns the filesystem superblock, and optionally the superblock
     for a particular device given that device’s index.

   | ``BCH_IOCTL_DISK_ADD``
   | Given a path to a device, adds it to a mounted and running
     filesystem. The device must already have a bcachefs superblock;
     options and parameters are read from the new device’s superblock
     and added to the member info section of the existing filesystem’s
     superblock.

   | ``BCH_IOCTL_DISK_REMOVE``
   | Given a path to a device or a device index, attempts to remove it
     from a mounted and running filesystem. This operation requires
     walking the btree to remove all references to this device, and may
     fail if data would become degraded or lost, unless appropriate
     force flags are set.

   | ``BCH_IOCTL_DISK_ONLINE``
   | Given a path to a device that is a member of a running filesystem
     (in degraded mode), brings it back online.

   | ``BCH_IOCTL_DISK_OFFLINE``
   | Given a path or device index of a device in a multi device
     filesystem, attempts to close it without removing it, so that the
     device may be re-added later and the contents will still be
     available.

   | ``BCH_IOCTL_DISK_SET_STATE``
   | Given a path or device index of a device in a multi device
     filesystem, attempts to set its state to one of read-write,
     read-only, failed or spare. Takes flags to force if the filesystem
     would become degraded.

   | ``BCH_IOCTL_DISK_GET_IDX``

   | ``BCH_IOCTL_DISK_RESIZE``

   | ``BCH_IOCTL_DISK_RESIZE_JOURNAL``

   | ``BCH_IOCTL_DATA``
   | Starts a data job, which walks all data and/or metadata in a
     filesystem performing, performaing some operation on each btree
     node and extent. Returns a file descriptor which can be read from
     to get the current status of the job, and closing the file
     descriptor (i.e. on process exit stops the data job.

   | ``BCH_IOCTL_SUBVOLUME_CREATE``

   | ``BCH_IOCTL_SUBVOLUME_DESTROY``

   | ``BCHFS_IOC_REINHERIT_ATTRS``

On disk format
==============

Superblock
----------

The superblock is the first thing to be read when accessing a bcachefs
filesystem. It is located 4kb from the start of the device, with
redundant copies elsewhere - typically one immediately after the first
superblock, and one at the end of the device.

The ``bch_sb_layout`` records the amount of space reserved for the
superblock as well as the locations of all the superblocks. It is
included with every superblock, and additionally written 3584 bytes from
the start of the device (512 bytes before the first superblock).

Most of the superblock is identical across each device. The exceptions
are the ``dev_idx`` field, and the journal section which gives the
location of the journal.

The main section of the superblock contains UUIDs, version numbers,
number of devices within the filesystem and device index, block size,
filesystem creation time, and various options and settings. The
superblock also has a number of variable length sections:

.. container:: description

   | ``BCH_SB_FIELD_journal``
   | List of buckets used for the journal on this device.

   | ``BCH_SB_FIELD_members``
   | List of member devices, as well as per-device options and settings,
     including bucket size, number of buckets and time when last
     mounted.

   | ``BCH_SB_FIELD_crypt``
   | Contains the main chacha20 encryption key, encrypted by the user’s
     passphrase, as well as key derivation function settings.

   | ``BCH_SB_FIELD_replicas``
   | Contains a list of replica entries, which are lists of devices that
     have extents replicated across them.

   | ``BCH_SB_FIELD_quota``
   | Contains timelimit and warnlimit fields for each quota type (user,
     group and project) and counter (space, inodes).

   | ``BCH_SB_FIELD_disk_groups``
   | Formerly referred to as disk groups (and still is throughout the
     code); this section contains device label strings and records the
     tree structure of label paths, allowing a label once parsed to be
     referred to by integer ID by the target options.

   | ``BCH_SB_FIELD_clean``
   | When the filesystem is clean, this section contains a list of
     journal entries that are normally written with each journal write
     (``struct jset``): btree roots, as well as filesystem usage and
     read/write counters (total amount of data read/written to this
     filesystem). This allows reading the journal to be skipped after
     clean shutdowns.

.. _journal-1:

Journal
-------

Every journal write (``struct jset``) contains a list of entries:
``struct jset_entry``. Below are listed the various journal entry types.

.. container:: description

   | ``BCH_JSET_ENTRY_btree_key``
   | This entry type is used to record every btree update that happens.
     It contains one or more btree keys (``struct bkey``), and the
     ``btree_id`` and ``level`` fields of ``jset_entry`` record the
     btree ID and level the key belongs to.

   | ``BCH_JSET_ENTRY_btree_root``
   | This entry type is used for pointers btree roots. In the current
     implementation, every journal write still records every btree root,
     although that is subject to change. A btree root is a bkey of type
     ``KEY_TYPE_btree_ptr_v2``, and the btree_id and level fields of
     ``jset_entry`` record the btree ID and depth.

   | ``BCH_JSET_ENTRY_clock``
   | Records IO time, not wall clock time - i.e. the amount of reads and
     writes, in 512 byte sectors since the filesystem was created.

   | ``BCH_JSET_ENTRY_usage``
   | Used for certain persistent counters: number of inodes, current
     maximum key version, and sectors of persistent reservations.

   | ``BCH_JSET_ENTRY_data_usage``
   | Stores replica entries with a usage counter, in sectors.

   | ``BCH_JSET_ENTRY_dev_usage``
   | Stores usage counters for each device: sectors used and buckets
     used, broken out by each data type.

Btrees
------

Btree keys
----------

.. container:: description

   ``KEY_TYPE_deleted``

   ``KEY_TYPE_whiteout``

   ``KEY_TYPE_error``

   ``KEY_TYPE_cookie``

   ``KEY_TYPE_hash_whiteout``

   ``KEY_TYPE_btree_ptr``

   ``KEY_TYPE_extent``

   ``KEY_TYPE_reservation``

   ``KEY_TYPE_inode``

   ``KEY_TYPE_inode_generation``

   ``KEY_TYPE_dirent``

   ``KEY_TYPE_xattr``

   ``KEY_TYPE_alloc``

   ``KEY_TYPE_quota``

   ``KEY_TYPE_stripe``

   ``KEY_TYPE_reflink_p``

   ``KEY_TYPE_reflink_v``

   ``KEY_TYPE_inline_data``

   ``KEY_TYPE_btree_ptr_v2``

   ``KEY_TYPE_indirect_inline_data``

   ``KEY_TYPE_alloc_v2``

   ``KEY_TYPE_subvolume``

   ``KEY_TYPE_snapshot``

   ``KEY_TYPE_inode_v2``

   ``KEY_TYPE_alloc_v3``
