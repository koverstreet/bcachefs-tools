// SPDX-License-Identifier: GPL-2.0

/* DOC_LATEX(allocator)
 * \subsubsection{Buckets}
 *
 * The allocator manages space in units called \emph{buckets} --- contiguous
 * regions on each device, typically 512 KB to 16 MB (set at format time with
 * \texttt{--bucket\_size}, default auto-selected based on device size). Each
 * bucket tracks how many dirty and cached sectors it contains, plus metadata
 * such as the oldest journal sequence number referencing it, stripe membership,
 * and generation number.
 *
 * Buckets are append-only: when allocated, a bucket is written to sequentially
 * and never overwritten until the entire bucket is invalidated and reused.
 * This log-structured approach means that stale data cannot be reclaimed in
 * place---copygc must move live data to new buckets before fragmented buckets
 * can be freed.
 *
 * Buckets cycle through a series of states:
 *
 * \begin{description}
 * \item[\texttt{dirty}] Contains live data or metadata. Cannot be reused until
 *   all data is moved or deleted.
 * \item[\texttt{cached}] Contains only cached copies (data with durable replicas
 *   elsewhere). Can be discarded when free space is needed.
 * \item[\texttt{need\_gc\_gens}] Legacy state, retained for compatibility.
 *   Previously used to prevent generation number wraparound; now effectively
 *   unused since the invalidate worker uses backpointers instead of generation
 *   bumping.
 * \item[\texttt{need\_discard}] All data invalidated; waiting for a discard
 *   (TRIM) command to be sent to the device.
 * \item[\texttt{free}] Discarded and ready for allocation.
 * \end{description}
 *
 * Bucket size affects fragmentation and overhead. Larger buckets reduce metadata
 * overhead and improve sequential write performance, but increase internal
 * fragmentation: if a bucket is only partially filled with live data, the
 * remaining space is wasted until copygc moves the live data elsewhere and frees
 * the entire bucket. The \texttt{bcachefs fs usage} command shows per-device
 * bucket counts and fragmentation.
 *
 * \subsubsection{Foreground allocator}
 *
 * The foreground allocator handles allocation requests from active writes. When
 * a write needs space, the allocator selects devices based on the applicable
 * target option (\texttt{foreground\_target}, \texttt{metadata\_target}, etc.).
 * Within the target group, devices are selected by striping across all
 * available devices, weighted by free space --- devices with more free space
 * receive proportionally more allocations, so all devices in the filesystem
 * fill up at roughly the same rate.
 *
 * If the target devices are full, the allocator falls back to any device in
 * the filesystem rather than failing the write. This fallback is deliberate:
 * target options express a preference, not a hard constraint. The only way to
 * get a hard constraint is to use separate filesystems.
 *
 * Each allocation request also specifies a watermark level (see Watermarks
 * below) and a required number of replicas. The allocator picks devices that
 * satisfy the replica count and durability requirements, avoiding placing
 * multiple replicas on the same device.
 *
 * \paragraph{Write points}
 * \label{sec:write-points}
 *
 * \bchdoc{foreground-allocator}
 *
 * \subsubsection{Background allocator}
 *
 * The background allocator runs continuously, producing free buckets for the
 * foreground allocator to consume. It manages three pipelines:
 *
 * \begin{description}
 * \item[Invalidation] Scans for buckets containing only cached data (or no data)
 *   and invalidates them. The invalidate worker walks backpointers to verify no
 *   live references remain before marking buckets for discard. Invalidated
 *   buckets move to the \texttt{need\_discard} state.
 * \item[Discard] Sends TRIM/discard commands to the device for invalidated
 *   buckets, then moves them to the \texttt{free} state. On devices that do not
 *   support discard, this step is a no-op.
 * \item[Freelist management] Maintains a pool of free buckets ready for
 *   immediate allocation. The target free bucket count is tunable and determines
 *   how far ahead the background allocator works.
 * \end{description}
 *
 * When free space is low, copygc kicks in to move live data out of
 * mostly-empty buckets, freeing them for reuse. The copygc reserve ensures that
 * copygc itself always has enough free space to make forward progress, even when
 * the filesystem appears full to user writes.
 *
 * \subsubsection{Watermarks}
 *
 * The allocator uses a tiered watermark system to manage space pressure. Each
 * watermark level reserves progressively more free buckets:
 *
 * \begin{description}
 * \item[\texttt{stripe}] Highest watermark; used for
 *   \hyperref[sec:erasure-coding]{erasure coding} stripe allocation. Most free
 *   space is available.
 * \item[\texttt{normal}] Standard user data writes.
 * \item[\texttt{copygc}] Copygc is allowed to dip into space reserved from
 *   normal writes.
 * \item[\texttt{btree}] Btree node allocation, which must succeed even under
 *   heavy space pressure.
 * \item[\texttt{btree\_copygc}] Btree allocation during copygc.
 * \item[\texttt{reclaim}] \hyperref[sec:journal]{Journal} reclaim---must always
 *   be able to flush dirty btree nodes to free journal space.
 * \item[\texttt{interior\_updates}] The lowest watermark, for btree interior
 *   node updates during splits and merges that must never fail.
 * \end{description}
 *
 * This layered approach ensures that critical internal operations (journal
 * reclaim, btree splits) can always make progress, even when the filesystem is
 * full from the user's perspective.
 *
 * \subsubsection{Accounting}
 *
 * The accounting subsystem maintains exact, transactional counters for all
 * space usage in the filesystem. Every write, delete, or metadata change that
 * affects space usage atomically updates the corresponding accounting entries
 * as part of the same transaction.
 *
 * The system is designed to be extensible: accounting keys are type-tagged
 * unions, so adding a new class of counters requires only defining a new
 * tag and its associated fields. No schema changes, no migration --- new
 * counter types appear in the btree alongside existing ones, and old code
 * simply ignores tags it does not recognize.
 *
 * Accounting entries are stored in a dedicated btree as actual counter values,
 * but updates are applied as deltas and aggregated by the btree write buffer
 * before being flushed. This is how accounting can live in a btree without
 * killing performance: many small increments are batched into a single btree
 * update. Version numbers derived from journal position ensure that journal
 * replay can safely deduplicate updates.
 *
 * \paragraph{What is tracked}
 *
 * \begin{description}
 * \item[\texttt{replicas}] On-disk usage by replication strategy --- which
 *   devices hold copies and how many. This is what \texttt{bcachefs fs usage}
 *   reports as the main usage breakdown.
 * \item[\texttt{dev\_data\_type}] Per-device usage broken down by data type
 *   (user data, btree, cached, parity, etc.), tracking bucket count, live
 *   sectors, and fragmentation.
 * \item[\texttt{compression}] Per-compression-type statistics: number of
 *   extents, uncompressed size, and compressed size on disk.
 * \item[\texttt{nr\_inodes}] Total inode count.
 * \item[\texttt{snapshot}] Per-snapshot on-disk usage.
 * \item[\texttt{btree}] Per-btree metadata usage (total sectors, node count).
 * \item[\texttt{reconcile\_work}] Pending work for the reconcile subsystem,
 *   broken down by type.
 * \item[\texttt{persistent\_reserved}] Sectors reserved by
 *   \texttt{KEY\_TYPE\_reservation} keys (e.g.\ fallocate).
 * \end{description}
 *
 * In memory, frequently-accessed counters (replicas, per-device, compression)
 * are maintained in percpu arrays for lock-free reads. Less frequently accessed
 * counters (per-snapshot, reconcile work) are read from the btree on demand.
 * The \texttt{bcachefs fs usage} command and the
 * \texttt{BCH\_IOCTL\_QUERY\_ACCOUNTING} ioctl both read from this system.
 *
 * \subsubsection{Replicas tracking}
 *
 * The replicas superblock field records every unique data replication
 * configuration in use by the filesystem --- each entry describes a data type
 * (journal, btree, user data, parity) and the set of devices that hold copies.
 * This is the authoritative source for determining whether the filesystem can
 * operate with a given set of devices.
 *
 * \paragraph{Mount decisions}
 *
 * At mount time, bcachefs checks every replicas entry against the set of
 * online devices. For each entry, it counts how many of the listed devices
 * are present and compares against the entry's \texttt{nr\_required} field
 * (normally 1; higher for erasure coding where multiple blocks are needed for
 * reconstruction). If any entry cannot be satisfied, the filesystem cannot
 * mount --- the data described by that entry would be inaccessible.
 *
 * Write-side checks are stricter: the filesystem must have enough read-write
 * devices to satisfy the configured replication levels for journal, metadata,
 * and user data. A filesystem can mount read-only with fewer devices than it
 * needs for read-write operation.
 *
 * \paragraph{Lifecycle}
 *
 * Replicas entries are added lazily: when new data is written with a
 * previously-unseen device combination, the entry is added to the superblock
 * as part of the transaction commit (via integration with the accounting
 * subsystem). Entries are removed when their corresponding accounting counters reach
 * zero --- meaning no data with that replication
 * pattern exists on disk anymore.
 *
 * The tight coupling with accounting means the replicas field stays accurate
 * without expensive scans: as data is written, moved, or deleted, accounting
 * deltas flow through the write buffer, and the replicas field is updated to
 * match.
 *
 * As of version 1.36, user data replicas entries are no longer stored in the
 * superblock --- only journal and metadata entries are. With large numbers of
 * devices, the combinatorial explosion of possible device sets for user data
 * made superblock replicas entries a scalability bottleneck. User data
 * replication is now tracked entirely through the accounting subsystem.
 *
 * \subsubsection{Backpointers}
 * \label{sec:backpointers}
 *
 * Every sector range on disk that contains data or metadata has a corresponding
 * backpointer: a reverse reference from the physical location back to the
 * logical btree entry that owns it. Backpointers answer the question ``what
 * data lives in this bucket?'' without scanning the entire extents btree.
 *
 * Backpointers are stored in a write-buffered btree, keyed by (device, sector
 * offset, discriminator). The value records the btree ID, level, and position
 * of the owning key, plus the data type and bucket generation number.
 *
 * \paragraph{Maintenance}
 *
 * Backpointers are created and deleted automatically by extent triggers: when
 * an extent is written, a backpointer is inserted for each data pointer; when
 * an extent is overwritten or deleted, the corresponding backpointers are
 * removed. Updates go through the write buffer for batching.
 *
 * The discriminator field handles cases where multiple extents share ownership
 * of the same physical block (e.g.\ compressed extents that have been partially
 * overwritten). Erasure code stripes get their own backpointers in a separate
 * \texttt{stripe\_backpointers} btree, since stripe backpointers have different
 * position semantics and lifecycle from extent backpointers.
 *
 * \paragraph{Operations that use backpointers}
 *
 * \begin{itemize}
 * \item \textbf{Copygc}: Finds live extents in fragmented buckets to relocate
 *   them, freeing the bucket for reuse.
 * \item \textbf{Device evacuation}: Finds all extents on a device being removed
 *   and migrates them to other devices.
 * \item \textbf{Scrub}: Walks backpointers in physical order to verify data
 *   integrity without random seeks across the extents btree.
 * \item \textbf{Reconcile}: Tracks extents that need to be moved on rotational
 *   devices for optimal LBA ordering.
 * \end{itemize}
 *
 * \paragraph{Consistency and self-healing}
 *
 * Missing or inconsistent backpointers are detected at runtime --- for example,
 * when the move path looks up backpointers for a bucket and finds they do not
 * match the bucket's sector counts. When a mismatch is detected, the relevant
 * recovery pass is automatically scheduled and run (with rate limiting to avoid
 * overwhelming the system).
 *
 * Three recovery passes verify backpointer integrity bidirectionally:
 * \texttt{check\_extents\_to\_backpointers} ensures every extent has matching
 * backpointers, \texttt{check\_backpointers\_to\_extents} ensures every
 * backpointer points to a valid extent, and
 * \texttt{check\_btree\_backpointers} validates backpointers against bucket
 * allocation state.
 *
 * The key optimization is comparing backpointer sector counts against bucket
 * sector counts: if they agree, the backpointers for that bucket are known to
 * be consistent without walking the extents btree. Only buckets with mismatches
 * need the more expensive bidirectional verification --- essential for larger
 * filesystems where a complete scan would be prohibitively expensive. Each
 * backpointer also records the bucket generation number at creation time, so
 * stale backpointers from reused buckets are detected and cleaned up
 * automatically.
 *
 * \subsubsection{Data structures}
 *
 * The allocator's persistent state is spread across several btrees, each
 * optimized for a different access pattern. The alloc btree is the
 * authoritative record of per-bucket state; the others are derived indexes
 * that accelerate specific operations.
 *
 * \paragraph{Alloc key (\texttt{bch\_alloc\_v4})}
 *
 * Every bucket on every device has a corresponding key in the alloc btree,
 * keyed by (device, bucket number). The value is a \texttt{bch\_alloc\_v4}
 * struct containing:
 *
 * \begin{itemize}
 * \item \texttt{gen} / \texttt{oldest\_gen} --- the current generation number
 *   and the oldest generation still referenced by extents. The difference
 *   between these determines whether the bucket needs a GC-gens pass.
 * \item \texttt{data\_type} --- what kind of data the bucket holds (user, btree,
 *   cached, parity, stripe, etc.), computed by \texttt{alloc\_data\_type()} from
 *   the other fields (see below).
 * \item \texttt{dirty\_sectors} / \texttt{cached\_sectors} /
 *   \texttt{stripe\_sectors} --- sector counts by category.
 * \item \texttt{stripe\_refcount} --- number of erasure-coded stripes
 *   referencing this bucket.
 * \item \texttt{io\_time[READ/WRITE]} --- timestamps for LRU eviction of
 *   cached data.
 * \item \texttt{journal\_seq\_nonempty} / \texttt{journal\_seq\_empty} ---
 *   journal sequence numbers tracking bucket state transitions, used by the
 *   noflush write optimization and the discard path.
 * \item \texttt{nr\_external\_backpointers} --- count of backpointers stored in
 *   the backpointers btree (as opposed to inline backpointers).
 * \end{itemize}
 *
 * The format has evolved through four versions (v1 through v4). Earlier
 * versions used variable-length varint encoding for fields; v4 switched to a
 * fixed-layout struct for simpler access. v4 also added support for inline
 * backpointers stored directly in the alloc key value, avoiding a separate
 * btree lookup for buckets with few backpointers. All versions are converted
 * to \texttt{bch\_alloc\_v4} in memory; the on-disk format is upgraded lazily
 * as keys are rewritten.
 *
 * \paragraph{Bucket state derivation}
 *
 * A bucket's logical state is not stored as a field --- it is \emph{derived}
 * from the alloc key contents by \texttt{alloc\_data\_type()}. The derivation
 * follows a priority chain:
 *
 * \begin{enumerate}
 * \item If \texttt{stripe\_refcount > 0}: the bucket belongs to an erasure-coded
 *   stripe (\texttt{BCH\_DATA\_stripe} or \texttt{BCH\_DATA\_parity}).
 * \item Else if \texttt{dirty\_sectors > 0} or \texttt{stripe\_sectors > 0}:
 *   the bucket contains live data; the type comes from the data that was
 *   written (user, btree, etc.).
 * \item Else if \texttt{cached\_sectors > 0}: the bucket contains only cached
 *   data (\texttt{BCH\_DATA\_cached}).
 * \item Else if the \texttt{NEED\_DISCARD} flag is set: the bucket is
 *   invalidated but awaiting TRIM (\texttt{BCH\_DATA\_need\_discard}).
 * \item Else if \texttt{gen - oldest\_gen >= BUCKET\_GC\_GEN\_MAX}: the
 *   generation gap is too large (\texttt{BCH\_DATA\_need\_gc\_gens}).
 * \item Otherwise: the bucket is free (\texttt{BCH\_DATA\_free}).
 * \end{enumerate}
 *
 * This derivation means bucket state is always consistent with the underlying
 * counters --- there is no separate state field that could get out of sync.
 *
 * \paragraph{Freespace btree}
 *
 * The freespace btree indexes free buckets for fast allocation. Keys are
 * (device, bucket number) with generation bits encoded in the high bits of the
 * offset, so the allocator can scan for free buckets on a given device with a
 * simple btree range scan. This is a derived index: entries are
 * inserted/removed by the alloc key trigger when a bucket transitions to or
 * from the free state. The foreground allocator cross-checks freespace entries
 * against the alloc btree before using a bucket, catching any inconsistencies.
 *
 * \paragraph{Need-discard btree}
 *
 * A simple presence/absence index of buckets in the
 * \texttt{BCH\_DATA\_need\_discard} state. The discard worker iterates this
 * btree to find buckets needing TRIM commands, sends the discards, then
 * updates the alloc key to clear the need-discard flag (which removes the
 * entry from this btree via the trigger). Maintaining a separate index avoids
 * scanning the entire alloc btree to find the small fraction of buckets
 * awaiting discard.
 *
 * \paragraph{Bucket-gens btree}
 *
 * Packs 256 bucket generation numbers into a single btree key
 * (\texttt{bch\_bucket\_gens}). This provides cheap stale-pointer detection:
 * when checking whether an extent pointer is stale, the code only needs to
 * read a small bucket-gens key rather than the full alloc key. This is
 * particularly important for the RCU read path, where looking up a full alloc
 * key would be too expensive.
 *
 * \paragraph{LRU btree}
 *
 * Indexes cached buckets by their last-read timestamp, enabling the
 * invalidation worker to evict the least-recently-used cached data first.
 * Also used with a separate LRU ID for fragmentation-based eviction ordering,
 * so copygc can prioritize the most fragmented buckets. Like the other
 * auxiliary btrees, entries are maintained by the alloc key trigger.
 *
 * \subsubsection{Device labels and targets}
 * \label{sec:disk-groups}
 *
 * Device labels are hierarchical paths delimited by periods --- for example,
 * \texttt{ssd.fast}, \texttt{ssd.slow}, \texttt{hdd.archive}. A target option
 * can reference any prefix of the path: specifying \texttt{ssd} as a target
 * matches all devices whose label starts with \texttt{ssd} (e.g.\
 * \texttt{ssd.fast}, \texttt{ssd.slow}), while \texttt{ssd.fast} matches only
 * that specific label. Labels need not be unique --- multiple devices can share
 * the same label, forming a group.
 *
 * Targets can also reference a device directly by path (e.g.\
 * \texttt{foreground\_target=/dev/sda1}). Internally, both device references
 * and label references resolve to entries in the disk groups superblock field,
 * which maps label strings to device sets.
 * Disk group entries may be retained after the last device in a label is
 * removed, because filesystem and per-file target options may still refer to
 * the label; preserving the entry avoids creating dangling target references.
 *
 * Four target options control where data is placed:
 *
 * \begin{description}
 * \item[\texttt{foreground\_target}] Normal foreground data writes, and
 *   metadata if \texttt{metadata\_target} is not set.
 * \item[\texttt{metadata\_target}] Btree node writes.
 * \item[\texttt{background\_target}] If set, user data is moved to this target
 *   in the background by the reconcile subsystem. The durable copies are moved
 *   off the foreground target; a copy is left on the original target only if it
 *   is a cache (see \texttt{promote\_target} or \texttt{durability=0}).
 * \item[\texttt{promote\_target}] If set, a cached copy is created on this
 *   target when data is read, if no copy exists there already.
 * \end{description}
 *
 * All four options can be set at the filesystem level (format time, mount time,
 * or runtime via sysfs) or on individual files and directories. Target options
 * express a preference, not a hard constraint: if the target devices are full,
 * the allocator falls back to any device in the filesystem.
 *
 * \subsubsection{Consistency and self-healing}
 *
 * The allocator performs runtime consistency checks during normal operation,
 * detecting and repairing problems without requiring an offline fsck.
 *
 * \paragraph{Runtime checks}
 *
 * The foreground allocator validates every bucket before use: the freespace
 * btree entry is cross-checked against the alloc btree to confirm the bucket
 * is actually free, the generation number matches, and no other subsystem has
 * a claim on it (open bucket, nocow lock, superblock region). If a mismatch
 * is detected, the bucket is skipped and an asynchronous repair job is queued
 * to fix the inconsistent entry without blocking allocation.
 *
 * Bucket state transitions are also validated: if a bucket is being marked
 * with a data type that conflicts with its current state (e.g.\ writing user
 * data to a bucket the alloc btree says contains metadata), the inconsistency
 * is flagged and a recovery pass is scheduled. Accounting counters are checked
 * for sanity (e.g.\ negative sector counts indicate lost writes or corruption)
 * and trigger recovery when anomalies are found.
 *
 * \paragraph{Recovery passes}
 *
 * When runtime checks detect problems, they automatically schedule the
 * appropriate recovery pass with rate limiting to avoid overwhelming the system.
 * The key allocator recovery passes are:
 *
 * \begin{description}
 * \item[\texttt{check\_allocations}] Full garbage collection: marks all
 *   referenced buckets by walking extents, btree nodes, and stripes, then
 *   compares against the alloc btree and repairs data types, sector counts,
 *   and stripe references.
 * \item[\texttt{check\_alloc\_info}] Cross-checks the alloc btree against the
 *   freespace, need\_discard, and bucket\_gens btrees, repairing any
 *   mismatches. Can run online.
 * \item[\texttt{check\_lrus}] Verifies LRU entries (used for cached bucket
 *   eviction order) match alloc btree timestamps; removes stale entries.
 * \item[\texttt{check\_alloc\_to\_lru\_refs}] Ensures every cached bucket has
 *   a correct LRU entry.
 * \end{description}
 *
 * Recovery passes are ordered by dependency: \texttt{check\_allocations} must
 * run before the others, since it establishes the ground truth for bucket
 * state. Passes marked \texttt{PASS\_ONLINE} can run on a mounted filesystem
 * without interrupting normal operation.
 */

//! Alloc keys, and the alloc trigger - background.c's.
//!
//! An alloc key has had four versions. v1 packs its fields at fixed widths,
//! a bit in @fields for each present; v2 and v3 have them as varints, and
//! each must fit the width BCH_ALLOC_FIELDS_V2() gives it; v4 is a plain
//! struct, grown since - a value shorter than today's struct reads as zeroes
//! past its end. Every user reads one as a v4: alloc_to_v4(); an edit makes
//! it one - alloc_to_v4_mut() - and an update, start_alloc_update(), edits
//! the update a transaction has already queued for the bucket, so triggers
//! on one bucket each edit the same key.
//!
//! bucket_gens keys hold a generation for each of KEY_TYPE_BUCKET_GENS_NR
//! buckets; alloc_read loads a device's generations from them - or, before
//! they existed, from the alloc keys.
//!
//! Where a bucket belongs in the lrus, and in need_discard, freespace and
//! bucket_gens, is here: the alloc trigger writes its entries, and fsck
//! checks them, by the same functions. C's allocator reads freespace by
//! background.h's alloc_freespace_genbits(), which FreespacePos must agree
//! with exactly - to the C integer types, wrapping and all.

use crate::accounting::{bucket_data_type, data_type, data_type_from_u8, data_type_is_empty, data_type_movable};
use crate::alloc::buckets::{bucket_to_u64, u64_to_bucket};
use crate::accounting;
use crate::alloc::check::need_discard_or_freespace_err;
use crate::alloc::lru::{dev_remove_lrus, lru_change, LruId};
use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{pos, BkeyI, BkeyS, BkeySC, BkeyWhole, POS_MIN};
use crate::btree::iter::{commit_do, BtreeIter, BtreeIterFlags, BtreeTrans, CommitFlags, LoopFlow, TransAttempt,
                         TransBkey, UpdateTriggerFlags};
use crate::btree::bkey_methods::TriggerOp;
use crate::c;
use crate::c::bch_errcode;
use crate::errcode::{err_ptr, ret_to_c, BchError};
use crate::fs::Fs;
use crate::init::error::{id, BkeyValidate};
use crate::opts::{data_type_str, prt_data_type};
use crate::util::ffi::Opaque;
use crate::util::kernel::{READ, WRITE};
use crate::util::rcu::read_lock;
use crate::util::log::LogMsg;
use crate::util::varint;
use crate::util::vstructs::{ArmRef, Validated};
use crate::util::Printbuf;
use crate::{bkey_fsck_err_on, btree_trans, prt};
use core::ffi::{c_int, c_uint};
use core::mem::{offset_of, size_of};
use core::sync::atomic::Ordering;
use cstruct_macros::c_export;

// ── Alloc keys before v4 ─────────────────────────────────────────────────

macro_rules! uint {
    (8) => { u8 };
    (16) => { u16 };
    (32) => { u32 };
    (64) => { u64 };
}

macro_rules! alloc_unpacked {
    ($(($name:ident, $bits:tt)),* $(,)?) => {
        /// An alloc key from before v4, unpacked: bkey_alloc_unpacked. Its
        /// varint fields are BCH_ALLOC_FIELDS_V2()'s, each as wide.
        #[derive(Default)]
        struct AllocUnpacked {
            journal_seq:  u64,
            generation:   u8,
            oldest_gen:   u8,
            data_type:    u8,
            need_discard: bool,
            $($name: uint!($bits),)*
        }

        impl AllocUnpacked {
            /// v2's and v3's fields, @nr of them varints at @data, the rest
            /// 0: None if one doesn't decode, or doesn't fit its field.
            fn varint_fields(mut self, mut data: &[u8], nr: u8) -> Option<Self> {
                let mut v = [0u64; [$(stringify!($name)),*].len()];
                for f in v.iter_mut().take(nr.into()) {
                    let (x, n) = varint::decode(data).ok()?;
                    *f = x;
                    data = &data[n..];
                }

                let [$($name),*] = v;
                $(self.$name = $name.try_into().ok()?;)*
                Some(self)
            }
        }
    };
}
c::BCH_ALLOC_FIELDS_V2!(alloc_unpacked);

macro_rules! alloc_v1_fields {
    ($(($name:ident, $bits:tt)),* $(,)?) => {
        /// The width of each of v1's fields, in bytes.
        const ALLOC_V1_FIELD_BYTES: &[usize] = &[$($bits / 8),*];

        impl AllocUnpacked {
            /// v1's fields, those present - @fields' bits - little endian at
            /// @data, each its width: alloc_field_v1_get(). Those past a value
            /// cut short are 0: validate refuses it, and to_text prints it.
            fn v1_fields(mut self, fields: u8, mut data: &[u8]) -> Self {
                let mut v = [0u64; ALLOC_V1_FIELD_BYTES.len()];
                for (i, (f, &bytes)) in v.iter_mut().zip(ALLOC_V1_FIELD_BYTES).enumerate() {
                    if fields & 1 << i != 0 {
                        let Some(b) = data.get(..bytes) else { break };
                        let mut le = [0u8; 8];
                        le[..bytes].copy_from_slice(b);
                        *f = u64::from_le_bytes(le);
                        data = &data[bytes..];
                    }
                }

                // no wider than v2's, which they're unpacked as
                let [$($name),*] = v;
                $(self.$name = $name as _;)*
                self
            }
        }
    };
}
c::BCH_ALLOC_FIELDS_V1!(alloc_v1_fields);

impl AllocUnpacked {
    fn v1(a: BkeySC<'_, c::Alloc<Validated>>) -> Self {
        let v = a.v();
        Self { generation: v.generation, ..Default::default() }.v1_fields(v.fields, &v.data)
    }

    fn v2(a: BkeySC<'_, c::AllocV2<Validated>>) -> Option<Self> {
        let v = a.v();
        Self {
            generation: v.generation,
            oldest_gen: v.oldest_gen,
            data_type:  v.data_type,
            ..Default::default()
        }.varint_fields(&v.data, v.nr_fields)
    }

    fn v3(a: BkeySC<'_, c::AllocV3<Validated>>) -> Option<Self> {
        let v = a.v();
        Self {
            journal_seq:  v.journal_seq.get(),
            generation:   v.generation,
            oldest_gen:   v.oldest_gen,
            data_type:    v.data_type,
            need_discard: v.need_discard(),
            ..Default::default()
        }.varint_fields(&v.data, v.nr_fields)
    }

    /// @k, an alloc key before v4 - zeroed, if it isn't one: bch2_alloc_unpack().
    /// Validate has seen v2 and v3 unpack.
    fn new(k: BkeySC<'_>) -> Self {
        if let Some(a) = k.arm::<c::Alloc>() {
            Self::v1(a)
        } else if let Some(a) = k.arm::<c::AllocV2>() {
            Self::v2(a).unwrap_or_default()
        } else if let Some(a) = k.arm::<c::AllocV3>() {
            Self::v3(a).unwrap_or_default()
        } else {
            Self::default()
        }
    }
}

/// v1's value size, in u64s, by the fields it says it has:
/// bch_alloc_v1_val_u64s().
fn alloc_v1_val_u64s(a: &c::Alloc<Validated>) -> usize {
    let fields: usize = ALLOC_V1_FIELD_BYTES.iter().enumerate()
        .filter(|&(i, _)| a.fields & 1 << i != 0)
        .map(|(_, &bytes)| bytes)
        .sum();
    (offset_of!(c::Alloc, repr.data) + fields).div_ceil(size_of::<u64>())
}

// ── Alloc keys as v4 ─────────────────────────────────────────────────────

/// Alloc key @k, of whatever version, as a current one - one that isn't an
/// alloc key, the slot of a bucket without one, as a zeroed one:
/// bch2_alloc_to_v4().
pub fn alloc_to_v4(k: BkeySC<'_>) -> c::AllocV4 {
    // A v4 by its length: fields past it are zero, as written before they
    // existed. Not by BACKPOINTERS_START - a key widened without updating
    // it has fields written past it since
    let mut a = c::AllocV4::val_copy_pad(k).unwrap_or_else(|| {
        let u = AllocUnpacked::new(k);
        let mut a = c::AllocV4::default();
        a.journal_seq_nonempty = u.journal_seq;
        a.flags                = u.need_discard.into();
        a.generation           = u.generation;
        a.oldest_gen           = u.oldest_gen;
        a.data_type            = u.data_type;
        a.dirty_sectors        = u.dirty_sectors;
        a.cached_sectors       = u.cached_sectors;
        a.io_time              = [u.read_time, u.write_time];
        a.stripe_refcount      = (u.stripe != 0).into();
        a
    });
    a.set_backpointers_start(c::BCH_ALLOC_V4_U64s as u64);
    a
}

#[c_export]
pub fn __bch2_alloc_to_v4(k: BkeySC<'_>, out: &mut c::AllocV4) {
    *out = alloc_to_v4(k);
}

impl Fs {
    /// The io clock for @rw, as a bucket's io_time holds it - never 0, which
    /// is none: bch2_current_io_time().
    pub fn current_io_time(&self, rw: usize) -> u64 {
        (self.io_clock[rw].now.load(Ordering::Relaxed) as u64 & c::LRU_TIME_MAX).max(1)
    }
}

impl<'a, 't> TransAttempt<'a, 't> {
    /// alloc_to_v4(), as a key in transaction memory to edit and update with
    /// - not queued as an update: bch2_alloc_to_v4_mut(). A v4 is grown to
    /// all of today's struct, or kept as long as it was if that's longer.
    pub fn alloc_to_v4_mut(&self, k: BkeySC<'_>) -> Result<TransBkey<'a, 't, c::AllocV4>, BchError> {
        if let Some(a) = k.arm::<c::AllocV4>() {
            let mut a = self.bkey_make_mut_noupdate(a)?;
            // Widened to all of today's struct: say so, for a kernel that
            // reads by BACKPOINTERS_START - one from before it read by length
            // would drop what's written past an older start
            a.v_mut().set_backpointers_start(c::BCH_ALLOC_V4_U64s as u64);
            return Ok(a);
        }

        let mut a = self.bkey_alloc_typed::<c::AllocV4>()?;
        a.k_mut().p = k.k.p;
        *a.v_mut() = alloc_to_v4(k);
        Ok(a)
    }

    /// The alloc key of bucket @pos, queued as its update, to edit - the
    /// update already queued, if there is one: bch2_trans_start_alloc_update().
    pub fn start_alloc_update(&self, pos: c::bpos, flags: UpdateTriggerFlags)
        -> Result<TransBkey<'a, 't, c::AllocV4>, BchError>
    {
        let mut iter = BtreeIter::new(self, c::btree_id::alloc, pos,
                                      BtreeIterFlags::CACHED | BtreeIterFlags::INTENT);
        let k = iter.peek_slot(self)?.expect("a slot always has a key");

        if self.in_mem(k.v.cast()) {
            let a = k.arm::<c::AllocV4>().expect("an alloc update, as alloc_to_v4_mut() makes one");
            // alloc_to_v4_mut() made it, all of an alloc_v4 or longer; by its
            // value, as C's container_of(): the header @k has may be a copy
            let u64s = u32::from(a.k.u64s).max((size_of::<BkeyI<c::AllocV4>>() / size_of::<u64>()) as u32);
            let key = k.v.cast::<u8>().wrapping_sub(size_of::<c::bkey>()).cast_mut();
            // SAFETY: a key in our memory, its value at @k.v, with @u64s u64s of room
            let a = unsafe { TransBkey::from_raw_room(self, key.cast(), u64s) };
            // SAFETY: all of an alloc_v4
            return Ok(unsafe { a.whole_unchecked() });
        }

        let a = self.alloc_to_v4_mut(k)?;
        self.update(&iter, &a, flags)?;
        Ok(a)
    }
}

/// An alloc_v4 key for C: in transaction memory, or an ERR_PTR.
fn alloc_v4_to_c(a: Result<TransBkey<'_, '_, c::AllocV4>, BchError>) -> *mut c::bkey_i_alloc_v4 {
    a.map_or_else(err_ptr, |a| a.as_ptr().cast())
}

#[c_export]
pub fn bch2_alloc_to_v4_mut<'t>(trans: &'t BtreeTrans<'t>, k: BkeySC<'_>) -> *mut c::bkey_i_alloc_v4 {
    // SAFETY: C calls it in an attempt
    let t = unsafe { trans.attempt_in_progress() };
    alloc_v4_to_c(t.alloc_to_v4_mut(k))
}

// ── Alloc keys: validate, swab, to_text ──────────────────────────────────

fn alloc_v1_validate(v: &BkeyValidate<'_, '_>) -> Result<(), BchError> {
    let a = v.k.arm::<c::Alloc>().expect("an alloc key");
    let (have, want) = (v.k.k.val_bytes() / size_of::<u64>(), alloc_v1_val_u64s(a.v()));

    // allow for unknown fields
    bkey_fsck_err_on!(v, have < want, id::alloc_v1_val_size_bad, "incorrect value size ({have} < {want})")
}

fn alloc_v4_validate(v: &BkeyValidate<'_, '_>) -> Result<(), BchError> {
    use c::bch_data_type as dt;

    let a = c::AllocV4::val_copy_pad(v.k).expect("an alloc_v4");
    let (want, have) = (a.u64s_noerror(), v.k.k.val_bytes() / size_of::<u64>());

    bkey_fsck_err_on!(v, want > have, id::alloc_v4_val_size_bad, "bad val size ({want} > {have})")?;

    // Backpointers stored after the value, as before they moved to the
    // backpointers btree (no_bps_in_alloc_keys): a value is read by its
    // length, where these would read as fields added since. Dropping the key
    // flags the alloc btree as having lost data, and check_allocations
    // rebuilds it.
    bkey_fsck_err_on!(v, a.nr_backpointers() != 0, id::alloc_v4_inline_backpointers,
                      "{} backpointers inline, from before they moved to the backpointers btree",
                      a.nr_backpointers())?;

    let data_type = a.alloc_data_type(a.data_type());
    bkey_fsck_err_on!(v, data_type.0 != u32::from(a.data_type), id::alloc_key_data_type_bad,
                      "invalid data type (got {} should be {})", a.data_type, data_type.0)?;

    for (rw, name) in [(READ, "read"), (WRITE, "write")] {
        bkey_fsck_err_on!(v, a.io_time[rw] > c::LRU_TIME_MAX, id::alloc_key_io_time_bad,
                          "invalid io_time[{name}]: {}, max {}", a.io_time[rw], c::LRU_TIME_MAX)?;
    }

    let (stripe, dirty, cached, refcount) = (a.stripe_sectors, a.dirty_sectors, a.cached_sectors, a.stripe_refcount);
    match a.data_type() {
        dt::BCH_DATA_free | dt::BCH_DATA_need_gc_gens | dt::BCH_DATA_need_discard =>
            bkey_fsck_err_on!(v, stripe != 0 || dirty != 0 || cached != 0 || refcount != 0,
                              id::alloc_key_empty_but_have_data,
                              "empty data type free but have data {stripe}.{dirty}.{cached} {refcount}"),
        dt::BCH_DATA_sb | dt::BCH_DATA_journal | dt::BCH_DATA_btree | dt::BCH_DATA_user | dt::BCH_DATA_parity =>
            bkey_fsck_err_on!(v, dirty == 0 && stripe == 0 && refcount == 0, id::alloc_key_dirty_sectors_0,
                              "data_type {} but dirty_sectors==0", data_type_str(a.data_type())),
        dt::BCH_DATA_cached =>
            bkey_fsck_err_on!(v, cached == 0 || dirty != 0 || stripe != 0 || refcount != 0,
                              id::alloc_key_cached_inconsistency, "data type inconsistency"),
        _ => Ok(()),
    }
}

#[c_export]
pub fn bch2_alloc_v1_validate(c: &Opaque<c::bch_fs>, k: BkeySC<'_>, from: &c::bkey_validate_context) -> c_int {
    ret_to_c(alloc_v1_validate(&BkeyValidate { fs: Fs::from_c(c), k, from }))
}

#[c_export]
pub fn bch2_alloc_v2_validate(c: &Opaque<c::bch_fs>, k: BkeySC<'_>, from: &c::bkey_validate_context) -> c_int {
    let v = BkeyValidate { fs: Fs::from_c(c), k, from };
    let a = k.arm::<c::AllocV2>().expect("an alloc_v2");
    ret_to_c(bkey_fsck_err_on!(&v, AllocUnpacked::v2(a).is_none(), id::alloc_v2_unpack_error, "unpack error"))
}

#[c_export]
pub fn bch2_alloc_v3_validate(c: &Opaque<c::bch_fs>, k: BkeySC<'_>, from: &c::bkey_validate_context) -> c_int {
    let v = BkeyValidate { fs: Fs::from_c(c), k, from };
    let a = k.arm::<c::AllocV3>().expect("an alloc_v3");
    ret_to_c(bkey_fsck_err_on!(&v, AllocUnpacked::v3(a).is_none(), id::alloc_v3_unpack_error, "unpack error"))
}

#[c_export]
pub fn bch2_alloc_v4_validate(c: &Opaque<c::bch_fs>, k: BkeySC<'_>, from: &c::bkey_validate_context) -> c_int {
    ret_to_c(alloc_v4_validate(&BkeyValidate { fs: Fs::from_c(c), k, from }))
}

/// Each field in the value's bytes byte swapped - only those the value has:
/// one from before the struct grew ends short of the later fields.
#[c_export]
pub fn bch2_alloc_v4_swab(_c: Option<&Fs>, mut k: BkeyS<'_>) {
    fn swab<const N: usize>(v: &mut [u8], at: usize) {
        if let Some(b) = v.get_mut(at..at + N) {
            b.reverse();
        }
    }
    let v = k.val_bytes_mut();

    swab::<8>(v, offset_of!(c::AllocV4, repr.journal_seq_nonempty));
    swab::<8>(v, offset_of!(c::AllocV4, repr.journal_seq_empty));
    swab::<4>(v, offset_of!(c::AllocV4, repr.flags));
    swab::<4>(v, offset_of!(c::AllocV4, repr.dirty_sectors));
    swab::<4>(v, offset_of!(c::AllocV4, repr.cached_sectors));
    swab::<8>(v, offset_of!(c::AllocV4, repr.io_time));
    swab::<8>(v, offset_of!(c::AllocV4, repr.io_time) + size_of::<u64>());
    swab::<4>(v, offset_of!(c::AllocV4, repr.stripe_refcount));
    swab::<4>(v, offset_of!(c::AllocV4, repr.nr_external_backpointers));
    swab::<4>(v, offset_of!(c::AllocV4, repr.stripe_sectors));
}

/// @a, read from alloc key @k: what's printed of a v4's later fields depends
/// on @k's length, whatever its version.
fn alloc_v4_to_text(out: &mut Printbuf, fs: Option<&Fs>, k: BkeySC<'_>, a: &c::AllocV4) {
    let has = |at: usize, len: usize| k.k.val_bytes() >= at + len;
    let ca = fs.and_then(|fs| fs.dev_get(k.k.p.inode as u32));

    prt!(out, "\ngen {} oldest_gen {} data_type ", a.generation, a.oldest_gen);
    prt_data_type(out, a.data_type());
    prt!(out, "\njournal_seq_nonempty {}\n", { a.journal_seq_nonempty });
    if has(offset_of!(c::AllocV4, repr.journal_seq_empty), size_of::<u64>()) {
        prt!(out, "journal_seq_empty    {}\n", { a.journal_seq_empty });
    }
    prt!(out, "need_discard         {}\n", u8::from(a.need_discard()));
    prt!(out, "need_inc_gen         {}\n", u8::from(a.need_inc_gen()));
    prt!(out, "dirty_sectors        {}\n", { a.dirty_sectors });
    if has(offset_of!(c::AllocV4, repr.stripe_sectors), size_of::<u32>()) {
        prt!(out, "stripe_sectors       {}\n", { a.stripe_sectors });
    }
    prt!(out, "cached_sectors       {}\n", { a.cached_sectors });
    prt!(out, "stripe_refcount      {}\n", { a.stripe_refcount });
    prt!(out, "io_time[READ]        {}\n", { a.io_time[READ] });
    prt!(out, "io_time[WRITE]       {}\n", { a.io_time[WRITE] });
    if let Some(ca) = &ca {
        prt!(out, "fragmentation     {}\n", a.lru_idx_fragmentation(ca));
    }
    prt!(out, "bp_start          {}\n", a.backpointers_start());
}

#[cold]
#[c_export]
pub fn bch2_alloc_to_text(out: &mut Printbuf, c: Option<&Opaque<c::bch_fs>>, k: BkeySC<'_>) {
    alloc_v4_to_text(out, c.map(Fs::from_c), k, &alloc_to_v4(k));
}

#[cold]
#[c_export]
pub fn bch2_alloc_v4_to_text(out: &mut Printbuf, c: Option<&Opaque<c::bch_fs>>, k: BkeySC<'_>) {
    let a = c::AllocV4::val_copy_pad(k).expect("an alloc_v4");
    alloc_v4_to_text(out, c.map(Fs::from_c), k, &a);
}

impl c::AllocV4 {
    fn data_type(&self) -> data_type {
        data_type_from_u8(self.data_type)
    }

    /// bch2_bucket_sectors_dirty(): u32s added as C's, wrapping.
    fn sectors_dirty(&self) -> i64 {
        i64::from(self.stripe_sectors.wrapping_add(self.dirty_sectors))
    }

    /// bch2_bucket_sectors().
    fn sectors(&self) -> i64 {
        if self.data_type() == data_type::cached {
            i64::from(self.cached_sectors)
        } else {
            self.sectors_dirty()
        }
    }

    /// The sectors of @ca's bucket that aren't in use, a full bucket of them if
    /// it's empty but not free: bch2_bucket_sectors_fragmented(), which takes
    /// the sectors as an int.
    fn sectors_fragmented(&self, ca: &c::bch_dev) -> i64 {
        let d = self.sectors() as i32;
        let bucket_size = i32::from(ca.mi.bucket_size);

        i64::from(if d != 0 {
            (bucket_size - d).max(0)
        } else if !data_type_is_empty(self.data_type()) {
            bucket_size
        } else {
            0
        })
    }

    /// Its time in its device's read lru, 0 for none - only cached data is
    /// in it: alloc_lru_idx_read().
    pub fn lru_idx_read(&self) -> u64 {
        if self.data_type() == data_type::cached {
            self.io_time[READ] & c::LRU_TIME_MAX
        } else {
            0
        }
    }

    /// Its time in @ca's fragmentation lru, 0 for none - how full it is, as a
    /// fraction of 1 << 31, for movable data with room it isn't using:
    /// alloc_lru_idx_fragmentation().
    pub fn lru_idx_fragmentation(&self, ca: &c::bch_dev) -> u64 {
        if self.data_type >= c::BCH_DATA_NR as u8 ||
           !data_type_movable(self.data_type()) ||
           self.sectors_fragmented(ca) == 0 {
            return 0;
        }

        // on a corrupt fs, more dirty sectors than the bucket has
        let bucket_size = u64::from(ca.mi.bucket_size);
        let d = self.sectors_dirty().min(bucket_size as i64) as u64;
        d * (1 << 31) / bucket_size
    }

    /// Its value's size in u64s by its own account: from where its
    /// backpointers start - v4's original size, if that's unset - and any
    /// inline: alloc_v4_u64s_noerror().
    fn u64s_noerror(&self) -> usize {
        let start = match self.backpointers_start() {
            0 => u64::from(c::BCH_ALLOC_V4_U64s_V0),
            s => s,
        };
        (start + self.nr_backpointers() * (size_of::<c::Backpointer>() / size_of::<u64>()) as u64) as usize
    }

    /// How far its generation is past its oldest pointer's, wrapping:
    /// alloc_gc_gen().
    pub fn gc_gen(&self) -> u8 {
        self.generation.wrapping_sub(self.oldest_gen)
    }

    /// Its type, from what it holds: @t is what's being added to it, or
    /// need_discard for one just emptied.
    pub fn alloc_data_type(&self, t: data_type) -> data_type {
        if self.data_type() == data_type::multiple {
            data_type::multiple
        } else if self.stripe_refcount != 0 {
            if t == data_type::parity { t } else { data_type::stripe }
        } else if self.sectors_dirty() != 0 {
            bucket_data_type(t)
        } else if self.cached_sectors != 0 {
            data_type::cached
        } else if t == data_type::need_discard {
            data_type::need_discard
        } else if u32::from(self.gc_gen()) >= c::BUCKET_GC_GEN_MAX {
            data_type::need_gc_gens
        } else {
            data_type::free
        }
    }

    pub fn set_data_type(&mut self, t: data_type) {
        self.data_type = self.alloc_data_type(t).0 as u8;
    }
}

// ── Positions in the btrees an alloc key indexes into ────────────────────
//
// need_discard, freespace and bucket_gens are indexes of the alloc btree,
// each with its own encoding of a bucket's position - background.h's
// inlines, for Rust: positions as their own types, converted to and from
// bpos at the btree.

/// A bucket's entry in the need_discard btree: the journal seq it became
/// empty at, then the bucket, as bucket_to_u64().
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NeedDiscardPos {
    pub journal_seq_empty: u64,
    pub bucket:            c::bpos,
}

impl NeedDiscardPos {
    /// Where bucket @bucket, alloc key @a, belongs.
    pub fn new(bucket: c::bpos, a: &c::AllocV4) -> Self {
        Self { journal_seq_empty: a.journal_seq_empty, bucket }
    }

    pub fn from_bpos(p: c::bpos) -> Self {
        Self { journal_seq_empty: p.inode, bucket: u64_to_bucket(p.offset) }
    }

    pub fn to_bpos(self) -> c::bpos {
        pos(self.journal_seq_empty, bucket_to_u64(self.bucket))
    }
}

/// Where in a free bucket's offset, in the freespace btree, the high bits of
/// its gc gen go.
const FREESPACE_GENBITS_SHIFT: u32 = 56;

/// A free bucket's place in the freespace btree: the bucket, with the high
/// bits of its gc gen in the top bits of the offset - alloc_freespace_pos().
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FreespacePos {
    pub bucket:  c::bpos,
    /// The bucket's gc gen >> 4: alloc_freespace_genbits() >> 56. Read from
    /// the btree, whatever the top 8 bits hold.
    pub genbits: u8,
}

impl FreespacePos {
    /// Where bucket @bucket, alloc key @a, belongs.
    pub fn new(bucket: c::bpos, a: &c::AllocV4) -> Self {
        Self { bucket, genbits: a.gc_gen() >> 4 }
    }

    pub fn from_bpos(p: c::bpos) -> Self {
        Self {
            bucket:  pos(p.inode, p.offset & ((1 << FREESPACE_GENBITS_SHIFT) - 1)),
            genbits: (p.offset >> FREESPACE_GENBITS_SHIFT) as u8,
        }
    }

    pub fn to_bpos(self) -> c::bpos {
        pos(self.bucket.inode, self.bucket.offset | u64::from(self.genbits) << FREESPACE_GENBITS_SHIFT)
    }
}

/// Where a bucket's generation is in the bucket_gens btree: the key - one per
/// KEY_TYPE_BUCKET_GENS_NR buckets - and its index in the key's gens[]:
/// alloc_gens_pos().
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BucketGensPos {
    pub key: c::bpos,
    /// < KEY_TYPE_BUCKET_GENS_NR
    pub idx: usize,
}

impl BucketGensPos {
    pub fn new(bucket: c::bpos) -> Self {
        Self {
            key: pos(bucket.inode, bucket.offset >> c::KEY_TYPE_BUCKET_GENS_BITS),
            idx: (bucket.offset & u64::from(c::KEY_TYPE_BUCKET_GENS_MASK)) as usize,
        }
    }

    /// The bucket it's the gen of: bucket_gens_pos_to_alloc().
    pub fn bucket(self) -> c::bpos {
        pos(self.key.inode, self.key.offset << c::KEY_TYPE_BUCKET_GENS_BITS | self.idx as u64)
    }

    /// The bucket's gen in @k, its bucket_gens key - 0 if it hasn't one:
    /// alloc_gen().
    pub fn bucket_gen(self, k: BkeySC<'_>) -> u8 {
        k.arm::<c::BucketGens>().map_or(0, |g| g.v().gens[self.idx])
    }
}

// ── bucket_gens keys ─────────────────────────────────────────────────────

fn bucket_gens_validate(v: &BkeyValidate<'_, '_>) -> Result<(), BchError> {
    let (have, want) = (v.k.k.val_bytes(), size_of::<c::BucketGens>());
    bkey_fsck_err_on!(v, have != want, id::bucket_gens_val_size_bad, "bad val size ({have} != {want})")
}

#[c_export]
pub fn bch2_bucket_gens_validate(c: &Opaque<c::bch_fs>, k: BkeySC<'_>, from: &c::bkey_validate_context) -> c_int {
    ret_to_c(bucket_gens_validate(&BkeyValidate { fs: Fs::from_c(c), k, from }))
}

#[cold]
#[c_export]
pub fn bch2_bucket_gens_to_text(out: &mut Printbuf, _c: Option<&Opaque<c::bch_fs>>, k: BkeySC<'_>) {
    if let Some(g) = k.arm::<c::BucketGens>() {
        for (i, gen) in g.v().gens.iter().enumerate() {
            prt!(out, "{}{gen}", if i != 0 { " " } else { "" });
        }
    }
}

/// The bucket_gens btree, made from the alloc btree: for the upgrade that
/// added it.
fn bucket_gens_init(fs: &Fs) -> Result<(), BchError> {
    let trans = btree_trans!(fs);
    // the key being filled, and its position
    let mut g: Option<(c::bpos, c::BucketGens)> = None;

    fn insert(t: &TransAttempt<'_, '_>, (p, gens): (c::bpos, c::BucketGens)) -> Result<(), BchError> {
        let mut k = t.bkey_alloc_typed::<c::BucketGens>()?;
        k.k_mut().p = p;
        *k.v_mut() = gens;
        t.insert(c::btree_id::bucket_gens, k, UpdateTriggerFlags::empty())
    }

    let mut iter = BtreeIter::new(&trans, c::btree_id::alloc, POS_MIN, BtreeIterFlags::PREFETCH);
    iter.for_each(&trans, |t, _, k| {
        // Not a fsck error: check_alloc_key(), later, checks it
        if fs.dev_bucket_tryget_noerror(k.k.p).is_none() {
            return Ok(());
        }

        let gens_pos = BucketGensPos::new(k.k.p);
        // cleared once committed: a restart redoes this key, the full one with it
        if let Some(full) = g.filter(|(p, _)| *p != gens_pos.key) {
            insert(t, full)?;
            t.commit(None, CommitFlags::NO_ENOSPC)?;
            g = None;
        }

        let (_, gens) = g.get_or_insert_with(|| (gens_pos.key, c::BucketGens::default()));
        gens.gens[gens_pos.idx] = alloc_to_v4(k).generation;
        Ok(())
    })?;

    match g {
        Some(last) => commit_do(&trans, None, CommitFlags::NO_ENOSPC, |t| insert(t, last)),
        None       => Ok(()),
    }
}

crate::recovery_pass!(bch2_bucket_gens_init => bucket_gens_init);

/// Each device's bucket generations, in memory: from the bucket_gens btree,
/// or before it existed, the alloc btree.
fn alloc_read(fs: &Fs) -> Result<(), BchError> {
    let _state = fs.state_lock_read();
    let trans = btree_trans!(fs);
    let mut ca = None;

    let ver = fs.version_upgrade_complete().0;
    if ver >= c::bcachefs_metadata_version::bcachefs_metadata_version_bucket_gens.0 {
        let mut iter = BtreeIter::new(&trans, c::btree_id::bucket_gens, POS_MIN, BtreeIterFlags::PREFETCH);
        iter.for_each(&trans, |_, _, k| {
            let Some(g) = k.arm::<c::BucketGens>() else { return Ok(LoopFlow::Continue) };

            // Not a fsck error: check_bucket_gens_key(), later, checks it
            fs.dev_iterate(&mut ca, k.k.p.inode as u32);
            let Some(ca) = &ca else { return Ok(LoopFlow::ContinueFrom(pos(k.k.p.inode + 1, 0))) };

            let start = BucketGensPos { key: k.k.p, idx: 0 }.bucket().offset;
            let end = BucketGensPos { key: k.k.p.nosnap_successor(), idx: 0 }.bucket().offset;
            let rcu = read_lock();
            for b in start.max(ca.mi.first_bucket.into())..end.min(ca.mi.nbuckets) {
                ca.bucket_gen_ref(b, &rcu).expect("one of the device's buckets")
                    .store(g.v().gens[(b & u64::from(c::KEY_TYPE_BUCKET_GENS_MASK)) as usize], Ordering::Relaxed);
            }
            Ok(LoopFlow::Continue)
        })
    } else {
        let mut iter = BtreeIter::new(&trans, c::btree_id::alloc, POS_MIN, BtreeIterFlags::PREFETCH);
        iter.for_each(&trans, |_, _, k| {
            // Not a fsck error: check_alloc_key(), later, checks it
            fs.dev_iterate(&mut ca, k.k.p.inode as u32);
            let Some(ca) = &ca else { return Ok(LoopFlow::ContinueFrom(pos(k.k.p.inode + 1, 0))) };

            if k.k.p.offset < ca.mi.first_bucket.into() {
                return Ok(LoopFlow::ContinueFrom(pos(k.k.p.inode, ca.mi.first_bucket.into())));
            }
            if k.k.p.offset >= ca.mi.nbuckets {
                return Ok(LoopFlow::ContinueFrom(pos(k.k.p.inode + 1, 0)));
            }

            ca.bucket_gen_ref(k.k.p.offset, &read_lock()).expect("one of the device's buckets")
                .store(alloc_to_v4(k).generation, Ordering::Relaxed);
            Ok(LoopFlow::Continue)
        })
    }
}

crate::recovery_pass!(bch2_alloc_read => alloc_read);

#[cfg(test)]
mod tests {
    //! Unpacking alloc keys from before v4: nothing writes those any more, and
    //! only an upgrade from a filesystem old enough to have them reads them -
    //! nothing else would notice them wrong.

    use super::*;

    fn varints(vals: &[u64]) -> ([u8; 64], usize) {
        let mut out = [0u8; 64];
        let n = vals.iter().fold(0, |n, &v| n + varint::encode(&mut out[n..], v));
        (out, n)
    }

    #[test]
    fn alloc_v1_fields() {
        // read_time, data_type and stripe present: bits 0, 2, 6
        let data = [0x34, 0x12, 0x05, 0x78, 0x56, 0x34, 0x12];
        let u = AllocUnpacked::default().v1_fields(1 << 0 | 1 << 2 | 1 << 6, &data);

        assert_eq!((u.read_time, u.write_time, u.data_type, u.dirty_sectors, u.stripe),
                   (0x1234, 0, 5, 0, 0x1234_5678));

        // cut short - to_text prints what validate refuses
        let u = AllocUnpacked::default().v1_fields(1 << 0 | 1 << 6, &data[..4]);
        assert_eq!((u.read_time, u.stripe), (0x1234, 0));
    }

    #[test]
    fn alloc_varint_fields() {
        let (d, n) = varints(&[1 << 40, 7, 100, 200, 3]);
        let u = AllocUnpacked::default().varint_fields(&d[..n], 5).expect("fields that fit");
        assert_eq!((u.read_time, u.write_time, u.dirty_sectors, u.cached_sectors, u.stripe, u.stripe_redundancy),
                   (1 << 40, 7, 100, 200, 3, 0));

        // more fields than this version knows: the rest ignored
        let (d, n) = varints(&[1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(AllocUnpacked::default().varint_fields(&d[..n], 8).is_some());

        // dirty_sectors is 32 bits
        let (d, n) = varints(&[0, 0, 1 << 32]);
        assert!(AllocUnpacked::default().varint_fields(&d[..n], 3).is_none());

        // a varint cut short
        let (d, n) = varints(&[0, 0, 1 << 20]);
        assert!(AllocUnpacked::default().varint_fields(&d[..n - 1], 3).is_none());
    }
}

// ── The alloc trigger ────────────────────────────────────────────────────
//
// An alloc key's update is a step of the bucket state machine. The
// transactional phase works out the new state - a bucket emptied goes
// need_discard, its generation's bumped once it's free to - checks the
// transition, and updates what indexes the alloc btree: freespace, the lrus,
// bucket_gens, and the device's accounting. The atomic phase, with the
// commit's journal seq, stamps the bucket's journal seqs, its in-memory
// generation and open bucket, and kicks what the new state wants: a discard,
// invalidates, gc_gens.
//
// It edits the key going in, in place, as all of today's alloc_v4 - every
// alloc update is made by alloc_to_v4_mut(), which grows it - and anything
// else is a bug the filesystem goes read-only for. Gc runs it on the keys in
// the btree, of whatever version, and only reads them; a deletion is
// worked through as an empty bucket's state, which goes nowhere.

impl c::AllocV4 {
    /// bch2_bucket_sectors_total(): u32s added as C's, wrapping.
    fn sectors_total(&self) -> i64 {
        i64::from(self.stripe_sectors.wrapping_add(self.dirty_sectors).wrapping_add(self.cached_sectors))
    }

    /// A stripe bucket's dirty sectors: bch2_bucket_sectors_unstriped().
    fn sectors_unstriped(&self) -> i64 {
        if self.data_type() == data_type::stripe { i64::from(self.dirty_sectors) } else { 0 }
    }
}

/// @f holds of @new and didn't of @old.
fn became(f: impl Fn(&c::AllocV4) -> bool, old: &c::AllocV4, new: &c::AllocV4) -> bool {
    !f(old) && f(new)
}

/// @f held of @old and doesn't of @new.
fn ceased(f: impl Fn(&c::AllocV4) -> bool, old: &c::AllocV4, new: &c::AllocV4) -> bool {
    f(old) && !f(new)
}

fn changed(f: impl Fn(&c::AllocV4) -> bool, old: &c::AllocV4, new: &c::AllocV4) -> bool {
    f(old) != f(new)
}

fn nonempty(a: &c::AllocV4) -> bool {
    !data_type_is_empty(a.data_type())
}

fn is(t: data_type) -> impl Fn(&c::AllocV4) -> bool {
    move |a| a.data_type() == t
}

/// @k's value, all of today's alloc_v4, to edit - None if it's another
/// type, or a v4 shorter than today's.
fn alloc_v4_whole_mut<'k>(k: &'k mut BkeyS<'_>) -> Option<&'k mut c::AllocV4> {
    if !k.k.is::<c::AllocV4>() {
        return None;
    }
    <c::AllocV4 as ArmRef>::arm_mut(k.as_s().into_val_bytes_mut())
}

/// A copy of the trigger's new key as it's been edited, header @k and value
/// @a, for what reads it whole - to print it, or find its place.
fn alloc_key(k: &c::bkey, a: &c::AllocV4) -> BkeyI<c::AllocV4> {
    BkeyI { k: *k, v: *a }
}

/// @k, as a key of any type.
fn as_sc(k: &BkeyI<c::AllocV4>) -> BkeySC<'_> {
    BkeySC::<c::AllocV4>::from(k).untyped()
}

/// A transition a trigger shouldn't have seen: @what, the keys going out and
/// coming in, and where from.
fn transition_text(fs: &Fs, what: core::fmt::Arguments<'_>, old: BkeySC<'_>, new: BkeySC<'_>) -> Printbuf {
    let mut buf = Printbuf::new();
    let out = &mut buf;
    prt!(out, "{what}\nold: {}\nnew: {}\n", old.to_text(fs), new.to_text(fs));
    // SAFETY: a printbuf; skipping this frame, as C's from the trigger did
    unsafe { c::bch2_prt_current_backtrace(buf.as_raw(), 1, c::GFP_KERNEL) };
    buf
}

/// A free bucket's entry in the freespace btree, for alloc key @alloc_k - @a,
/// as a v4 - set, or cleared: reported, and left, if it's already as @set
/// says. Not free, it has none.
pub(crate) fn bucket_do_freespace_index(t: &TransAttempt<'_, '_>, ca: &c::bch_dev, alloc_k: BkeySC<'_>,
                                        a: &c::AllocV4, set: bool) -> Result<(), BchError> {
    if a.data_type() != data_type::free {
        return Ok(());
    }

    let pos = FreespacePos::new(alloc_k.k.p, a).to_bpos();
    let mut iter = BtreeIter::new(t, c::btree_id::freespace, pos, BtreeIterFlags::INTENT | BtreeIterFlags::TRIM);
    let old = iter.peek_slot(t)?.expect("a slot always has a key");

    if ca.mi.freespace_initialized != 0 && (old.k.type_ == 0) != set &&
       !need_discard_or_freespace_err(t, alloc_k, set, false, false)? {
        return Ok(());
    }
    t.bit_mod_iter(&iter, set)
}

/// Bucket @bucket's need_discard entry, through the write buffer: from the
/// atomic phase, as its position - the journal seq the bucket went empty at
/// - is the commit's.
fn bucket_do_discard_index(t: &TransAttempt<'_, '_>, bucket: c::bpos, a: &c::AllocV4, set: bool)
    -> Result<(), BchError>
{
    if a.data_type() != data_type::need_discard {
        return Ok(());
    }
    t.bit_mod_buffered(c::btree_id::need_discard, NeedDiscardPos::new(bucket, a).to_bpos(), set)
}

/// Bucket @bucket's generation in the bucket_gens btree, made @gen.
fn bucket_gen_update(t: &TransAttempt<'_, '_>, bucket: c::bpos, gen: u8) -> Result<(), BchError> {
    let pos = BucketGensPos::new(bucket);
    let mut iter = BtreeIter::new(t, c::btree_id::bucket_gens, pos.key, BtreeIterFlags::INTENT);
    let k = iter.peek_slot(t)?.expect("a slot always has a key");

    let mut g = match k.arm::<c::BucketGens>() {
        Some(k) => t.bkey_make_mut_noupdate(k)?,
        None => {
            let mut g = t.bkey_alloc_typed::<c::BucketGens>()?;
            g.k_mut().p = iter.pos();
            g
        }
    };
    g.v_mut().gens[pos.idx] = gen;
    t.update(&iter, &g, UpdateTriggerFlags::empty())
}

/// @ca's counters for its buckets of @data_type - gc's, with @gc: @d their
/// number, sectors and fragmented sectors.
fn dev_data_type_accounting_mod(t: &TransAttempt<'_, '_>, ca: &c::bch_dev, data_type: data_type, d: [i64; 3],
                                gc: bool) -> Result<(), BchError> {
    let pos = c::disk_accounting_pos::from_arm(c::bch_acct_dev_data_type { dev: ca.dev_idx, data_type: data_type.0 as u8 });
    accounting::add(t, &pos, &d, gc)
}

impl TransAttempt<'_, '_> {
    /// Device @ca's bucket counts and sectors, for a bucket going from @old
    /// to @new: bch2_alloc_key_to_dev_counters().
    pub fn alloc_key_to_dev_counters(&self, ca: &c::bch_dev, old: &c::AllocV4, new: &c::AllocV4,
                                     flags: UpdateTriggerFlags) -> Result<(), BchError> {
        let gc = flags.contains(UpdateTriggerFlags::GC);
        let (old_sectors, new_sectors) = (old.sectors(), new.sectors());
        if old.data_type != new.data_type {
            dev_data_type_accounting_mod(self, ca, new.data_type(),
                                         [1, new_sectors, new.sectors_fragmented(ca)], gc)?;
            dev_data_type_accounting_mod(self, ca, old.data_type(),
                                         [-1, -old_sectors, -old.sectors_fragmented(ca)], gc)?;
        } else if old_sectors != new_sectors {
            dev_data_type_accounting_mod(self, ca, new.data_type(),
                                         [0, new_sectors - old_sectors,
                                          new.sectors_fragmented(ca) - old.sectors_fragmented(ca)], gc)?;
        }

        let (old_unstriped, new_unstriped) = (old.sectors_unstriped(), new.sectors_unstriped());
        if old_unstriped != new_unstriped {
            dev_data_type_accounting_mod(self, ca, data_type::unstriped,
                                         [i64::from(new_unstriped != 0) - i64::from(old_unstriped != 0),
                                          new_unstriped - old_unstriped, 0], gc)?;
        }
        Ok(())
    }
}

/// # Safety
/// @trans is in an attempt.
#[c_export]
pub unsafe fn bch2_alloc_key_to_dev_counters(trans: &BtreeTrans<'_>, ca: &c::bch_dev, old: &c::AllocV4,
                                             new: &c::AllocV4, flags: c_uint) -> c_int {
    let t = unsafe { trans.attempt_in_progress() };
    ret_to_c(t.alloc_key_to_dev_counters(ca, old, new, UpdateTriggerFlags::from_bits_retain(flags)))
}

/// A key for a bucket its device hasn't: inconsistent.
#[cold]
fn inval_bucket_key(fs: &Fs, k: BkeySC<'_>) -> Result<(), BchError> {
    crate::fs_inconsistent!(fs, "reference to invalid bucket\n{}", k.to_text(fs));
    fs.throw(bch_errcode::BCH_ERR_trigger_alloc)
}

/// stripe_refcount counts the bucket_to_stripe entries for the bucket. Gone
/// to 0 while a stripe still points here, nothing stops the bucket going
/// empty, being discarded and reused under the stripe - whose pointers go
/// stale, which fsck doesn't otherwise catch till the stripe's read (GH
/// #960): name the transaction that let it happen.
#[cold]
fn alloc_check_no_stripe_refs(t: &TransAttempt<'_, '_>, old: BkeySC<'_>, new: BkeySC<'_>) -> Result<(), BchError> {
    let fs = t.fs();
    let b = bucket_to_u64(new.k.p);
    let mut iter = BtreeIter::new(t, c::btree_id::bucket_to_stripe, pos(b, 0), BtreeIterFlags::empty());

    if let Some(k) = iter.peek_max(t, pos(b, u64::MAX))? {
        let buf = transition_text(fs, format_args!("bucket stripe_refcount went to 0 in {}, but stripe {} still references it",
                                                   t.trans().fn_name(), { k.k.p.offset }),
                                  old, new);
        crate::log_fsck_err!(t, id::alloc_key_stripe_refcount_wrong, "{buf}")?;
    }
    Ok(())
}

/// The alloc key going in, @op.new, edited in place - see the top of the
/// section.
fn trigger_alloc(t: &TransAttempt<'_, '_>, op: &mut TriggerOp<'_>) -> Result<(), BchError> {
    use UpdateTriggerFlags as F;
    let fs = t.fs();
    let flags = op.flags;
    let hdr = *op.new.k;
    let p = hdr.p;
    let (dev, b) = (p.inode as u32, p.offset);

    // An alloc key for a bucket that doesn't exist is check_alloc_info's to
    // delete (alloc_key_to_missing_dev_bucket): gc has nothing to account for
    // it, and deleting it nothing to undo. Inserting one is the inconsistency
    if !fs.dev_bucket_exists(p) && (flags.contains(F::GC) || hdr.is_deleted()) {
        return Ok(());
    }

    let ca = fs.dev_bucket_tryget(p).map_err(|_| fs.err(bch_errcode::BCH_ERR_trigger_alloc))?;

    let old_a = alloc_to_v4(op.old);
    // A copy, for gc, which only reads it - its keys are the btree's, of any
    // version - and for a deletion. Otherwise it's edited in place
    let mut copy;
    let new_a: &mut c::AllocV4 = if hdr.is_deleted() || !flags.intersects(F::TRANSACTIONAL | F::ATOMIC) {
        copy = alloc_to_v4(BkeySC::from(&op.new));
        &mut copy
    } else {
        match alloc_v4_whole_mut(&mut op.new) {
            Some(a) => a,
            None => {
                let mut msg = LogMsg::new(fs);
                prt!(msg, "alloc trigger in {}: key isn't all of a current alloc_v4 - not from alloc_to_v4_mut()\n{}",
                     t.trans().fn_name(), BkeySC::from(&op.new).to_text(fs));
                fs.emergency_read_only(&mut msg);
                return fs.throw(bch_errcode::BCH_ERR_emergency_ro);
            }
        }
    };
    let new_key = |a: &c::AllocV4| alloc_key(&hdr, a);

    if flags.contains(F::TRANSACTIONAL) {
        new_a.set_data_type(new_a.data_type());

        // Non-empty to empty: the bucket had data, and needs discarding
        // before it's reused. Not sb or journal, which don't go empty this
        // way - the WARN below catches one that does
        if ceased(nonempty, &old_a, new_a) &&
           old_a.data_type() != data_type::sb && old_a.data_type() != data_type::journal {
            new_a.data_type = data_type::need_discard.0 as u8;
        }

        if became(nonempty, &old_a, new_a) {
            if new_a.data_type() != data_type::sb && new_a.data_type() != data_type::journal &&
               !fs.bucket_is_open_safe(dev, b) && !ca.bucket_nouse(b) {
                let new = new_key(new_a);
                let buf = transition_text(fs, format_args!("bucket going nonempty but not open in {}",
                                                           t.trans().fn_name()),
                                          op.old, as_sc(&new));
                crate::log_fsck_err!(t, id::alloc_key_bucket_nonempty_to_empty_not_open, "{buf}")?;
            }

            new_a.io_time[READ] = fs.current_io_time(READ);
            new_a.io_time[WRITE] = fs.current_io_time(WRITE);
            new_a.set_need_inc_gen(true);
            // kept for downgrades: alloc_data_type() on older kernels reads
            // it to gate need_discard
            new_a.set_need_discard(true);
        }

        // Normal operation's invariants: fsck's alloc info repair
        // re-establishes bucket state from the stripes and backpointers, and
        // makes these transitions itself - need_discard to parity, re-marking
        // a stripe's parity bucket - so not while it runs
        let in_fsck = fs.flag(c::bch_fs_flags::BCH_FS_in_fsck);

        if !in_fsck && old_a.stripe_refcount != 0 && new_a.stripe_refcount == 0 {
            let new = new_key(new_a);
            alloc_check_no_stripe_refs(t, op.old, as_sc(&new))?;
        }

        // To free legitimately: from need_discard, by the discard path; from
        // need_gc_gens, by gc_gens bumping oldest_gen - empty all along
        if became(is(data_type::free), &old_a, new_a) {
            crate::warn_on!(!in_fsck && old_a.data_type() != data_type::need_discard &&
                            old_a.data_type() != data_type::need_gc_gens);
        }

        if ceased(is(data_type::need_discard), &old_a, new_a) {
            if data_type_is_empty(new_a.data_type()) {
                crate::warn_on!(!in_fsck && !flags.contains(F::IS_DISCARD));
            } else {
                crate::warn_on!(!in_fsck && !fs.bucket_is_open_safe(dev, b));
            }
        }

        // Free, still with discard bookkeeping: written free other than by
        // the discard path, Worker::mark_free_key(), which clears it all - or
        // an upgrade's residue, NEED_DISCARD set by a kernel from before these
        // data types, which set_data_type() above made free. With NEED_DISCARD
        // it may yet need discarding: back to need_discard, for the discard
        // worker. Otherwise the journal seqs are residue
        if new_a.data_type() == data_type::free &&
           (new_a.need_discard() || new_a.journal_seq_nonempty != 0 || new_a.journal_seq_empty != 0) {
            crate::log_fsck_err!(t, id::alloc_key_data_type_free_dirty_bookkeeping,
                "bucket marked free still has discard/journal bookkeeping\n\
                 NEED_DISCARD={}, journal_seq_nonempty={}, journal_seq_empty={}\n{}",
                u8::from(new_a.need_discard()), new_a.journal_seq_nonempty, new_a.journal_seq_empty,
                as_sc(&new_key(new_a)).to_text(fs))?;
            if new_a.need_discard() {
                new_a.data_type = data_type::need_discard.0 as u8;
            } else {
                new_a.journal_seq_nonempty = 0;
                new_a.journal_seq_empty = 0;
            }
        }

        if data_type_is_empty(new_a.data_type()) && new_a.need_inc_gen() && !fs.bucket_is_open_safe(dev, b) {
            if new_a.oldest_gen == new_a.generation && new_a.sectors_total() == 0 {
                new_a.oldest_gen = new_a.oldest_gen.wrapping_add(1);
            }
            new_a.generation = new_a.generation.wrapping_add(1);
            new_a.set_need_inc_gen(false);
            new_a.set_data_type(new_a.data_type());
        }

        if changed(is(data_type::free), &old_a, new_a) ||
           (new_a.data_type() == data_type::free &&
            FreespacePos::new(p, &old_a).genbits != FreespacePos::new(p, new_a).genbits) {
            bucket_do_freespace_index(t, &ca, op.old, &old_a, false)?;
            let new = new_key(new_a);
            bucket_do_freespace_index(t, &ca, as_sc(&new), new_a, true)?;
        }

        // Journal space for the need_discard update the atomic phase makes,
        // through the write buffer: a key's jset_u64s(), twice
        if changed(is(data_type::need_discard), &old_a, new_a) ||
           (old_a.data_type() == data_type::need_discard && old_a.journal_seq_empty != new_a.journal_seq_empty) {
            t.trans().reserve_extra_journal_u64s(2 * (c::BKEY_U64s + c::JSET_KEYS_U64s) as u32);
        }

        if new_a.data_type() == data_type::cached && new_a.io_time[READ] == 0 {
            new_a.io_time[READ] = fs.current_io_time(READ);
        }

        // the ids as C's were, u16s: the device's own as its read lru's, and
        // bucket_fragmentation_lru()'s
        lru_change(t, LruId::from(p.inode as u16), bucket_to_u64(p),
                   old_a.lru_idx_read(), new_a.lru_idx_read())?;
        lru_change(t, LruId::from((c::BCH_LRU_BUCKET_FRAGMENTATION_START + dev) as u16), bucket_to_u64(p),
                   old_a.lru_idx_fragmentation(&ca), new_a.lru_idx_fragmentation(&ca))?;

        if old_a.generation != new_a.generation {
            bucket_gen_update(t, p, new_a.generation)?;
        }

        t.alloc_key_to_dev_counters(&ca, &old_a, new_a, flags)?;
    }

    if flags.contains(F::ATOMIC) {
        let seq = t.journal_res_seq();
        assert!(seq != 0, "an atomic alloc trigger with no journal reservation");

        if new_a.journal_seq_nonempty > seq {
            crate::log_fsck_err!(t, id::alloc_key_journal_seq_in_future,
                "bucket journal seq in future (currently at {})\n{}",
                fs.journal_cur_seq(), as_sc(&new_key(new_a)).to_text(fs))?;
            new_a.journal_seq_nonempty = seq;
        }

        if new_a.generation != old_a.generation {
            let rcu = read_lock();
            let Some(gen) = ca.bucket_gen_ref(b, &rcu) else {
                return inval_bucket_key(fs, as_sc(&new_key(new_a)));
            };
            gen.store(new_a.generation, Ordering::Relaxed);
        }

        if became(is(data_type::free), &old_a, new_a) {
            ca.alloc_wake();
        }

        if changed(is(data_type::free), &old_a, new_a) {
            // nr_open_buckets counts open buckets that are still free: a
            // written one is gone from buckets[free] already, and
            // free_buckets_at() would take it off twice
            let _freelist = fs.allocator.freelist_lock.lock();
            if let Some(ob) = fs.open_bucket(dev, b) {
                let uncount = ceased(is(data_type::free), &old_a, new_a);
                if ob.free_uncounted.get() != uncount {
                    let n = ca.nr_open_buckets.get();
                    ca.nr_open_buckets.set(if uncount { n.wrapping_sub(1) } else { n.wrapping_add(1) });
                    ob.free_uncounted.set(uncount);
                }
            }
        }

        // The empty to nonempty transition's journal seq - the first: data
        // in a bucket may be overwritten while it's still being written
        if became(nonempty, &old_a, new_a) && new_a.journal_seq_nonempty == 0 {
            new_a.journal_seq_nonempty = seq;
        }

        if became(is(data_type::need_discard), &old_a, new_a) {
            // Emptied: waiting for a journal flush - unless nothing since it
            // went nonempty was flushed, and the journal can be asked not to
            // flush the seqs in between
            new_a.journal_seq_empty = seq;

            if new_a.journal_seq_nonempty == new_a.journal_seq_empty ||
               fs.journal_noflush_seq(new_a.journal_seq_nonempty, new_a.journal_seq_empty) {
                new_a.journal_seq_nonempty = 0;
                new_a.journal_seq_empty = 0;

                if !fs.bucket_set_discard_fast(dev, b) {
                    crate::alloc::discard::bch2_fast_discard_bucket_add(&ca, b);
                }
            }
        }

        if changed(is(data_type::need_discard), &old_a, new_a) {
            bucket_do_discard_index(t, p, &old_a, false)?;
            bucket_do_discard_index(t, p, new_a, true)?;
        }

        if became(is(data_type::cached), &old_a, new_a) && !fs.bucket_is_open(dev, b) &&
           crate::alloc::discard::bch2_should_invalidate_buckets(&ca, ca.usage_read_fast()) != 0 {
            ca.do_invalidates();
        }

        if became(is(data_type::need_gc_gens), &old_a, new_a) {
            fs.gc_gens_async();
        }
    }

    if flags.contains(F::GC) && flags.contains(F::INSERT) {
        let Some(g) = ca.gc_bucket(b) else {
            return inval_bucket_key(fs, as_sc(&new_key(new_a)));
        };
        let mut g = g.lock.lock();
        g.set_gen_valid(1);
        g.generation = new_a.generation;
    }
    Ok(())
}

crate::key_trigger!(bch2_trigger_alloc => trigger_alloc);

// ── Removing a device ────────────────────────────────────────────────────

/// Device @dev's need_discard entries: keyed by journal seq, so found by
/// scanning.
fn dev_remove_need_discard(fs: &Fs, dev: u64) -> Result<(), BchError> {
    let trans = btree_trans!(fs);
    let mut iter = BtreeIter::new(&trans, c::btree_id::need_discard, POS_MIN,
                                  BtreeIterFlags::INTENT | BtreeIterFlags::PREFETCH);
    iter.for_each_commit(&trans, None, CommitFlags::NO_ENOSPC, |t, iter, k| {
        if u64_to_bucket(k.k.p.offset).inode == dev {
            t.delete_at(iter, UpdateTriggerFlags::NORUN)?;
        }
        Ok(())
    })
}

/// Everything the alloc btrees have of device @ca, and its usage accounting,
/// as it's removed - the lrus and need_discard first, so the invalidate and
/// discard workers find nothing of it to race with.
fn dev_remove_alloc(fs: &Fs, ca: &c::bch_dev) -> Result<(), BchError> {
    let dev = u64::from(ca.dev_idx);

    dev_remove_lrus(fs, ca)?;
    dev_remove_need_discard(fs, dev)?;
    for btree in [c::btree_id::freespace, c::btree_id::backpointers, c::btree_id::bucket_gens, c::btree_id::alloc] {
        fs.btree_delete_range(btree, pos(dev, 0), pos(dev, u64::MAX), BtreeIterFlags::empty(),
                              UpdateTriggerFlags::NORUN)?;
    }
    fs.dev_usage_remove(ca)
}

#[c_export]
pub fn bch2_dev_remove_alloc(c: &Opaque<c::bch_fs>, ca: &c::bch_dev) -> c_int {
    ret_to_c(crate::bch_err_msg_dev!(ca, dev_remove_alloc(Fs::from_c(c), ca), "removing dev alloc info"))
}

// ── Bucket IO clocks ─────────────────────────────────────────────────────

/// @bucket's io_time for @rw, made now: a read of its cached data, so its
/// place in the read lru is as recently used.
fn bucket_io_time_reset(t: &TransAttempt<'_, '_>, bucket: c::bpos, rw: usize) -> Result<(), BchError> {
    let mut iter = BtreeIter::new(t, c::btree_id::alloc, bucket, BtreeIterFlags::CACHED | BtreeIterFlags::INTENT);
    let k = iter.peek_slot(t)?.expect("a slot always has a key");
    let mut a = t.alloc_to_v4_mut(k)?;

    let now = t.fs().current_io_time(rw);
    if a.v().io_time[rw] == now {
        return Ok(());
    }
    a.v_mut().io_time[rw] = now;

    t.update(&iter, &a, UpdateTriggerFlags::empty())?;
    t.commit(None, CommitFlags::empty())
}

/// From the read path, partway through its attempt - begun again if its
/// locks can't be had back - and a nested one of its own.
///
/// # Safety
/// @trans has an attempt in progress.
#[c_export]
pub unsafe fn bch2_bucket_io_time_reset(trans: &BtreeTrans<'_>, dev: c_uint, bucket_nr: usize, rw: c_int) -> c_int {
    let t = unsafe { trans.attempt_in_progress() };
    let t = if t.relock().is_err() { trans.begin() } else { t };
    ret_to_c(t.nested(|t| bucket_io_time_reset(t, pos(dev.into(), bucket_nr as u64), rw as usize)))
}
