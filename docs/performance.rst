Performance
-----------

The core of the architecture is a very high performance and very low
latency b+ tree, which also is not a conventional b+ tree but more of
hybrid, taking concepts from compacting data structures: btree nodes are
very large, log structured, and compacted (resorted) as necessary in
memory. This means our b+ trees are very shallow compared to other
filesystems.

What this means for the end user is that since we require very few seeks
or disk reads, filesystem latency is extremely good - especially cache
cold filesystem latency, which does not show up in most benchmarks but
has a huge impact on real world performance, as well as how fast the
system "feels" in normal interactive usage. Latency has been a major
focus throughout the codebase - notably, we have assertions that we
never hold b+ tree locks while doing IO, and the btree transaction layer
makes it easily to aggressively drop and retake locks as needed - one
major goal of bcachefs is to be the first general purpose soft realtime
filesystem.

Additionally, unlike other COW btrees, btree updates are journalled.
This greatly improves our write efficiency on random update workloads,
as it means btree writes are only done when we have a large block of
updates, or when required by memory reclaim or journal reclaim.
