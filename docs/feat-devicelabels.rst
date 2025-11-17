Device labels and targets
~~~~~~~~~~~~~~~~~~~~~~~~~

By default, writes are striped across all devices in a filesystem, but
they may be directed to a specific device or set of devices with the
various target options. The allocator only prefers to allocate from
devices matching the specified target; if those devices are full, it
will fall back to allocating from any device in the filesystem.

Target options may refer to a device directly, e.g.
``foreground_target=/dev/sda1``, or they may refer to a device label. A
device label is a path delimited by periods - e.g. ssd.ssd1 (and labels
need not be unique). This gives us ways of referring to multiple devices
in target options: If we specify ssd in a target option, that will refer
to all devices with the label ssd or labels that start with ssd. (e.g.
ssd.ssd1, ssd.ssd2).

Four target options exist. These options all may be set at the
filesystem level (at format time, at mount time, or at runtime via
sysfs), or on a particular file or directory:

.. container:: description

   | ``foreground_target``: normal foreground data writes, and metadata
     if
   | ``metadata_target`` is not set

   ``metadata_target``: btree writes

   ``background_target``: If set, user data (not metadata) will be moved
   to this target in the background

   ``promote_target``: If set, a cached copy will be added to this
   target on read, if none exists