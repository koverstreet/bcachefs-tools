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
