Bucket based allocation
-----------------------

As mentioned bcachefs is descended from bcache, where the ability to
efficiently invalidate cached data and reuse disk space was a core
design requirement. To make this possible the allocator divides the disk
up into buckets, typically 512k to 2M but possibly larger or smaller.
Buckets and data pointers have generation numbers: we can reuse a bucket
with cached data in it without finding and deleting all the data
pointers by incrementing the generation number.

In keeping with the copy-on-write theme of avoiding update in place
wherever possible, we never rewrite or overwrite data within a bucket -
when we allocate a bucket, we write to it sequentially and then we don’t
write to it again until the bucket has been invalidated and the
generation number incremented.

This means we require a copying garbage collector to deal with internal
fragmentation, when patterns of random writes leave us with many buckets
that are partially empty (because the data they contained was
overwritten) - copy GC evacuates buckets that are mostly empty by
writing the data they contain to new buckets. This also means that we
need to reserve space on the device for the copy GC reserve when
formatting - typically 8% or 12%.

There are some advantages to structuring the allocator this way, besides
being able to support cached data:

-  By maintaining multiple write points that are writing to different
   buckets, we’re able to easily and naturally segregate unrelated IO
   from different processes, which helps greatly with fragmentation.

-  The fast path of the allocator is essentially a simple bump allocator
   - the disk space allocation is extremely fast

-  Fragmentation is generally a non issue unless copygc has to kick in,
   and it usually doesn’t under typical usage patterns. The allocator
   and copygc are doing essentially the same things as the flash
   translation layer in SSDs, but within the filesystem we have much
   greater visibility into where writes are coming from and how to
   segregate them, as well as which data is actually live - performance
   is generally more predictable than with SSDs under similar usage
   patterns.

-  The same algorithms will in the future be used for managing SMR hard
   drives directly, avoiding the translation layer in the hard drive -
   doing this work within the filesystem should give much better
   performance and much more predictable latency.