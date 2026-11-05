# RFC: incremental insertion into the in-memory accounting table

This is a conservative candidate for maintainer discussion, based on
`b20fc75581caec355ce390ab39135595cc94d97c`. It replaces per-insertion
Eytzinger sorting with binary search and a linear shift. It is not an RCU
conversion, a final answer for arbitrarily large tables, or a patch that has
been tested on a mounted production filesystem.

## Problem and evidence

A 66-SATA-device filesystem converting data to 14+2 EC stripes repeatedly
left all SATA disks idle. A 30-second system-wide cycles profile of the
unmodified module captured 21,934 samples with no lost samples. Of sampled
cycles whose leaf instruction was in bcachefs, 93.24% were in
`accounting_mem::insert_locked` and its Eytzinger sort closures. Call chains
included both stripe-key creation and `stripe_update_extents` through
transaction commit and accounting insertion.

A separate stack snapshot found 37 reconcile workers waiting on the read
side of `mark_lock` in `durability_available`, before data I/O. Journal
reclaim was also waiting for that lock in transaction commit. Kernel logs
reported long transaction SRCU holds in stripe creation/update paths.
These observations identify a shared metadata bottleneck; they do not
measure time asleep or establish that it is the only bottleneck.

`MarkRead::insert` upgrades to exclusive `mark_lock`. On an absent accounting
key, `insert_locked` appends an entry through `Table::with_entries`, which
heap-sorts the entire table into Eytzinger order. New device-set accounting
keys therefore cost O(N log N) under the exclusive lock. The issue predates
the Rust conversion; this proposal does not claim a Rust regression.

## Change

Keep `KVVec<Entry>`, but maintain ordinary ascending `Bpos` order:

- Exact lookup uses binary search; lower-bound lookup uses partition_point.
- Insertion finds the position, fallibly appends the entry, and rotates the
  suffix once: O(log N) comparisons and O(N) movement.
- Single-entry removal rotates the suffix and pops the removed entry.
- GC's existing retain preserves order and no longer needs a sort.
- Recovery still collects entries separately and sorts once before publishing
  the table and running fixups. Its duplicate check walks the sorted slice.

The on-disk format, allocation flags, lock type and lock scopes, transaction
rollback, replica validation, and per-CPU counter ownership are unchanged.
No new unsafe code is required. The private table has no external consumers
that interpret its array layout.

## Invariants reviewed

- All table reads still require `mark_lock`; structural mutations still
  require its exclusive side. The recheck for a raced duplicate remains
  inside `insert_locked`, after acquiring the write lock.
- An `Entry::new` or `KVVec::push` failure precedes any rotation. The existing
  table remains sorted and owns its original entries. Failed entry ownership
  follows the existing fallible-allocation/drop path.
- Rotations move owning entries without copying or freeing their counter
  allocations. Removal drops just the removed entry.
- GC retains entries with either live or GC counters; `free_gc` and its
  allocation-failure cleanup are unchanged.
- Callers that release the lock must re-find entries by position. Existing
  code already requires this because Eytzinger sorting moves every entry.
  `gc_done` continues from a saved `Bpos`, not a persistent vector index.
- Recovery publishes a sorted table even when its walk returns an error,
  preserving the previous error-path behavior.

## Validation

Validation completed locally:

- 114 library tests passed; the manual benchmark is ignored by default and
  passed when invoked separately.
- The production change built successfully as an out-of-tree kernel module
  against 7.2.14-arch1-1 headers. The module was staged under /tmp and was not
  installed or loaded. The builds emit pre-existing dependency/compiler
  warnings; they are not warning-free validation.
- `git diff --check` passed.

Run from the repository root:

```sh
cargo test --offline --release -p bcachefs-kernel --lib -- --test-threads=1
cargo test --offline --release -p bcachefs-kernel sorted_table_benchmark -- --ignored --nocapture --test-threads=1
```

The mutation test compares exact and lower-bound lookups against BTreeMap,
including missing keys and inode/offset/snapshot boundaries. It checks live
and GC counter sums after shuffled insertion, first/middle/last removal,
retention of entries with only live or only GC counters, draining the table,
and reinsertion at the minimum/maximum position.

The ignored benchmark compares the old full-sort insertion with the new
Table::insert on actual Entry values. Both vectors have spare capacity;
per-CPU entry allocation happens outside the timed region. There are eight
insertions at each size, followed by 100,000 mixed hit/miss exact lookups.
It isolates table maintenance, excluding allocator cost, lock acquisition,
concurrency, kernel scheduling, and filesystem I/O.

One optimized userspace run on a Ryzen Threadripper PRO 3945WX:

| Initial entries | Old insertion (us) | New insertion (us) | Old lookup (ns) | New lookup (ns) |
| ---: | ---: | ---: | ---: | ---: |
| 1,024 | 104.630 | 0.535 | 37.673 | 41.342 |
| 16,384 | 2,407.651 | 8.439 | 78.749 | 92.351 |
| 131,072 | 22,056.911 | 247.341 | 141.759 | 147.490 |

At the largest measured size, insertion is about 89x faster and lookup about
4% slower. This is a single run on an active machine without CPU pinning;
it is not a reconcile throughput prediction. It also does not measure the
production filesystem's accounting-table size.

## Tradeoffs and remaining work

Insertion and deletion still move O(N) entries under the global lock. This
is an incremental mitigation, not the O(log N) insertion of a balanced tree.
Losing Eytzinger layout may hurt lookup-heavy workloads, and an ordered tree
or an RCU design may be a better long-term choice. The maintainer's previous
GC/kill-path design constraints should guide that decision.

Before deployment, exercise actual filesystem recovery, accounting GC and
replica deletion, fault-injected allocation failures, transaction restarts,
and concurrent reconcile in a disposable VM. Compare throughput, blocked
worker time, and profiles on equivalent data and device layouts. The unit
tests do not replace these checks. No running module or filesystem settings
were changed during this work.
