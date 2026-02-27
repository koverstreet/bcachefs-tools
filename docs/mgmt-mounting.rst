Mounting
--------

To mount a multi device filesystem, there are two options. You can
specify all component devices, separated by hyphens, e.g.

   ::

      mount -t bcachefs /dev/sda:/dev/sdb:/dev/sdc /mnt

Or, use the mount.bcachefs tool to mount by filesystem UUID. Still todo:
improve the mount.bcachefs tool to support mounting by filesystem label.

No special handling is needed for recovering from unclean shutdown.
Journal replay happens automatically, and diagnostic messages in the
dmesg log will indicate whether recovery was from clean or unclean
shutdown.

The ``-o degraded`` option will allow a filesystem to be mounted without
all the the devices, but will fail if data would be missing. The
``-o very_degraded`` can be used to attempt mounting when data would be
missing.

Also relevant is the ``-o nochanges`` option. It disallows any and all
writes to the underlying devices, pinning dirty data in memory as
necessary if for example journal replay was necessary - think of it as a
"super read-only" mode. It can be used for data recovery, and for
testing version upgrades.

The ``-o verbose`` enables additional log output during the mount
process.
