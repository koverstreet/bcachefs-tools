
Status of data
--------------

The ``bcachefs fs usage`` may be used to display filesystem usage broken
out in various ways. Data usage is broken out by type: superblock,
journal, btree, data, cached data, and parity, and by which sets of
devices extents are replicated across. We also give per-device usage
which includes fragmentation due to partially used buckets.
