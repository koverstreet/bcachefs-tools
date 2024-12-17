
Erasure coding
~~~~~~~~~~~~~~

bcachefs also supports Reed-Solomon erasure coding - the same algorithm
used by most RAID5/6 implementations) When enabled with the ``ec``
option, the desired redundancy is taken from the ``data_replicas``
option - erasure coding of metadata is not supported.

Erasure coding works significantly differently from both conventional
RAID implementations and other filesystems with similar features. In
conventional RAID, the "write hole" is a significant problem - doing a
small write within a stripe requires the P and Q (recovery) blocks to be
updated as well, and since those writes cannot be done atomically there
is a window where the P and Q blocks are inconsistent - meaning that if
the system crashes and recovers with a drive missing, reconstruct reads
for unrelated data within that stripe will be corrupted.

ZFS avoids this by fragmenting individual writes so that every write
becomes a new stripe - this works, but the fragmentation has a negative
effect on performance: metadata becomes bigger, and both read and write
requests are excessively fragmented. Btrfs’s erasure coding
implementation is more conventional, and still subject to the write hole
problem.

bcachefs’s erasure coding takes advantage of our copy on write nature -
since updating stripes in place is a problem, we simply don’t do that.
And since excessively small stripes is a problem for fragmentation, we
don’t erasure code individual extents, we erasure code entire buckets -
taking advantage of bucket based allocation and copying garbage
collection.

When erasure coding is enabled, writes are initially replicated, but one
of the replicas is allocated from a bucket that is queued up to be part
of a new stripe. When we finish filling up the new stripe, we write out
the P and Q buckets and then drop the extra replicas for all the data
within that stripe - the effect is similar to full data journalling, and
it means that after erasure coding is done the layout of our data on
disk is ideal.

Since disks have write caches that are only flushed when we issue a
cache flush command - which we only do on journal commit - if we can
tweak the allocator so that the buckets used for the extra replicas are
reused (and then overwritten again) immediately, this full data
journalling should have negligible overhead - this optimization is not
implemented yet, however.