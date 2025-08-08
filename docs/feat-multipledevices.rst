Multiple devices
----------------

bcachefs is a multi-device filesystem. Devices need not be the same
size: by default, the allocator will stripe across all available devices
but biasing in favor of the devices with more free space, so that all
devices in the filesystem fill up at the same rate. Devices need not
have the same performance characteristics: we track device IO latency
and direct reads to the device that is currently fastest.

.. toctree::
   :maxdepth: 1

   feat-replication
   feat-erasurecoding
   feat-devicelabels
   feat-caching
   feat-durability
