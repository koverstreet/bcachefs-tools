
Caching
~~~~~~~

When an extent has multiple copies on different devices, some of those
copies may be marked as cached. Buckets containing only cached data are
discarded as needed by the allocator in LRU order.

| When data is moved from one device to another according to the
| ``background_target`` option, the original copy is left in place but
  marked as cached. With the ``promote_target`` option, the original
  copy is left unchanged and the new copy on the ``promote_target``
  device is marked as cached.

To do writeback caching, set ``foreground_target`` and
``promote_target`` to the cache device, and ``background_target`` to the
backing device. To do writearound caching, set ``foreground_target`` to
the backing device and ``promote_target`` to the cache device.
