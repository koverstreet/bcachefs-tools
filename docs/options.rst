
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