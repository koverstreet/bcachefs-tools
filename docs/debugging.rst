

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
