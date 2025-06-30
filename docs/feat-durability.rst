Durability
~~~~~~~~~~

Some devices may be considered to be more reliable than others. For
example, we might have a filesystem composed of a hardware RAID array
and several NVME flash devices, to be used as cache. We can set
replicas=2 so that losing any of the NVME flash devices will not cause
us to lose data, and then additionally we can set durability=2 for the
hardware RAID device to tell bcachefs that we don’t need extra replicas
for data on that device - data on that device will count as two
replicas, not just one.

The durability option can also be used for writethrough caching: by
setting durability=0 for a device, it can be used as a cache and only as
a cache - bcachefs won’t consider copies on that device to count towards
the number of replicas we’re supposed to keep.
