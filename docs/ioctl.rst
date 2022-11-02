
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
