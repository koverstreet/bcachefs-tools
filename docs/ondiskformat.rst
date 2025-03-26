
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
