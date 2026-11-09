// SPDX-License-Identifier: GPL-2.0

/* DOC_LATEX(btrees)
 * bcachefs is, at its core, a transactional key-value store built on b+trees.
 * Every piece of filesystem state---file data mappings, inodes, directory
 * entries, allocation tracking, accounting---is a key-value pair in one of 28
 * btrees. There are no separate inode tables, bitmap allocators, or per-inode
 * extent trees. This uniform design means all metadata operations share the
 * same transaction, caching, and recovery infrastructure.
 *
 * \subsubsection{The btrees}
 *
 * The btrees fall into several functional groups:
 *
 * \textbf{Core filesystem data:}
 * \begin{description}
 * \item[\texttt{extents}] Maps file offsets to physical disk locations (extent
 *   pointers). Snapshot-aware: different snapshots of the same file can have
 *   different extents in the same btree.
 * \item[\texttt{inodes}] Inode metadata (size, permissions, timestamps, etc.).
 *   Snapshot-aware.
 * \item[\texttt{dirents}] Directory entries mapping names to inode numbers.
 *   Snapshot-aware.
 * \item[\texttt{xattrs}] Extended attributes. Snapshot-aware.
 * \item[\texttt{reflink}] Shared extent pointers for reflinked (deduplicated) data.
 * \end{description}
 *
 * \textbf{Allocation and space management:}
 * \begin{description}
 * \item[\texttt{alloc}] Bucket allocation state---one key per bucket, tracking
 *   data type, dirty sectors, generation number, and other per-bucket metadata.
 * \item[\texttt{freespace}] Free space index, keyed so free extents can be found
 *   by size and location.
 * \item[\texttt{need\_discard}] Buckets waiting for TRIM/discard before reuse.
 * \item[\texttt{bucket\_gens}] Bucket generation numbers, used to detect stale
 *   pointers cheaply without reading the full alloc key.
 * \item[\texttt{backpointers}] Reverse mappings from physical disk locations
 *   back to the btree keys that reference them (see
 *   \hyperref[sec:backpointers]{Backpointers}). Enables efficient device
 *   removal, scrubbing, and data migration.
 * \end{description}
 *
 * \textbf{Snapshots and subvolumes:}
 * \begin{description}
 * \item[\texttt{subvolumes}] Subvolume metadata (root inode, parent, flags);
 *   see \hyperref[sec:snapshots]{Subvolumes and snapshots}.
 * \item[\texttt{snapshots}] Snapshot tree structure (parent/child/sibling links,
 *   depth).
 * \item[\texttt{snapshot\_trees}] Roots of snapshot trees.
 * \item[\texttt{subvolume\_children}] Parent-child relationships between
 *   subvolumes.
 * \item[\texttt{deleted\_inodes}] Inodes pending deletion (may be visible in
 *   some snapshots but not others).
 * \end{description}
 *
 * \textbf{Reconcile (background maintenance):}
 * \begin{description}
 * \item[\texttt{reconcile\_work}, \texttt{reconcile\_hipri}] Work items for
 *   re-replication, migration, and other background data operations, at
 *   normal and high priority.
 * \item[\texttt{reconcile\_pending}] Work items not yet ready to process.
 * \item[\texttt{reconcile\_scan}] Scan state for discovering new work.
 * \item[\texttt{reconcile\_work\_phys}, \texttt{reconcile\_hipri\_phys}]
 *   Physical (device-keyed) variants of the work queues.
 * \end{description}
 *
 * \textbf{Other:}
 * \begin{description}
 * \item[\texttt{quotas}] User, group, and project quota counters.
 * \item[\texttt{stripes}] Erasure coding stripe descriptors.
 * \item[\texttt{bucket\_to\_stripe}, \texttt{stripe\_backpointers}] Mappings
 *   between buckets and erasure coding stripes.
 * \item[\texttt{lru}] Least-recently-used tracking for cache eviction.
 * \item[\texttt{logged\_ops}] In-progress logged operations for crash recovery
 *   of multi-transaction operations.
 * \item[\texttt{accounting}] Space accounting broken down by replica set, disk
 *   group, compression type, and snapshot.
 * \end{description}
 *
 * \subsubsection{What the btree design enables}
 *
 * The uniform btree design has consequences that are visible to users:
 *
 * \textbf{Consistent performance at scale.} Btree nodes are large
 * (128K--256K), making trees very shallow---typically 2--3 levels even at
 * petabyte scale. Combined with the filesystem's own node cache (independent
 * of the Linux page cache), this means metadata lookups rarely need more than
 * one disk read, even under memory pressure.
 *
 * \textbf{Atomic cross-object operations.} Because all metadata lives in the
 * same transactional system, operations that touch multiple objects---creating
 * a file (inode + dirent + accounting), writing data (extent + backpointer +
 * allocation)---are atomic. There is no window where the filesystem is
 * inconsistent, even on crash.
 *
 * \textbf{Online fsck and repair.} The btree transaction system supports
 * online fsck: recovery passes can run on a mounted filesystem, checking
 * and repairing metadata while normal IO continues. This is possible because
 * the same transaction/locking infrastructure protects both normal operations
 * and fsck.
 *
 * \textbf{Efficient background maintenance.}
 * \hyperref[sec:backpointers]{Backpointers} enable operations that would
 * otherwise require full btree scans: device removal, data scrubbing,
 * re-replication, and rebalancing can all find the relevant data by walking
 * backpointers rather than scanning the entire extent tree.
 *
 * \textbf{Snapshot efficiency.}
 * \hyperref[sec:snapshots]{Snapshot}-aware btrees store all snapshot versions
 * of a key in the same btree, sharing the tree structure. Taking a snapshot
 * is O(1)---no data or metadata is copied. Reads in a snapshot
 * context find the correct version through the snapshot ID in the key
 * position.
 *
 * \subsubsection{Important invariants}
 *
 * \textbf{No duplicate keys.} There are no duplicate keys in a btree;
 * insertions implicitly overwrite existing keys at the same position.
 * Internally, when an insertion overwrites an existing key, the old key is
 * marked as deleted (by setting \texttt{k->type = KEY\_TYPE\_deleted})---but
 * until the btree node is fully rewritten, the old key still exists on disk.
 * When a btree node is read, we mergesort all bsets it contains, and as part
 * of the mergesort duplicate keys are found and older versions are dropped.
 *
 * \textbf{Deletion via whiteouts.} Deletion is not exposed as a primitive
 * operation. Instead, deletion is performed by inserting a key of type
 * \texttt{KEY\_TYPE\_deleted} (a whiteout). This is a direct consequence of
 * the log-structured btree design: it is not possible to delete a key from a
 * bset that has already been written to disk---we can only append new keys.
 * Whiteouts are dropped when the btree node eventually fills up and is
 * rewritten with all bsets merged.
 *
 * \textbf{Ordering always preserved.} The ordering of insertions and updates
 * is always preserved, across unclean shutdowns and without any need for
 * explicit flushes. This is critical for filesystem correctness. For example,
 * creating a file requires two operations: creating the inode, then creating
 * the directory entry. By doing these in the correct order, we guarantee that
 * after an unclean shutdown we never have directory entries pointing to
 * nonexistent inodes---we might leak inode references, but those can be
 * garbage collected. See the \hyperref[sec:journal]{journal} section for the
 * mechanism that provides this guarantee.
 *
 * \subsubsection{Node structure}
 *
 * Btree nodes are log structured internally: new keys are appended rather
 * than inserted in sorted order. Each node consists of multiple sorted sets
 * (bsets)---the initial sorted set from the last full rewrite, plus additional
 * bsets appended by subsequent updates. Lookups merge results from all bsets.
 * When a node is split or compacted, all bsets are merged into a single sorted
 * set.
 *
 * Once a bset has been written out, it may also be sorted in memory with other
 * written bsets---we do this periodically so that a given btree node has at
 * most a few (typically three) bsets in memory: the one currently being
 * inserted into (at most 8--16K), and the rest roughly forming a geometric
 * progression in size. This in-memory resorting is one of the main ways
 * bcachefs efficiently uses such large btree nodes, keeping lookup cost
 * bounded even as the node accumulates updates. Sorting the entire node into
 * a single bset is relatively infrequent.
 *
 * This log-structured format has two important properties: writes are
 * sequential (good for both SSDs and spinning disks), and a node can be
 * written to disk under a read lock rather than a write lock, since new
 * data is only appended. This keeps btree lock hold times short and avoids
 * blocking readers during writeback.
 *
 * Keys within a bset are packed: a per-bset format descriptor records which
 * fields are common across all keys, and those fields are stored in a
 * compressed representation. This typically saves 30--50\% of metadata space
 * and improves cache utilization.
 *
 * \subsubsection{On-disk format}
 *
 * A btree node occupies a contiguous region on disk (typically 128K--256K).
 * The first sector contains a \texttt{btree\_node} header; subsequent sectors
 * contain \texttt{btree\_node\_entry} records. Each of these structures wraps
 * a single \textbf{bset}---a sorted array of packed keys.
 *
 * \textbf{The btree\_node header} contains:
 * \begin{itemize}
 * \item A checksum and magic number for validation.
 * \item A sequence number (\texttt{BTREE\_NODE\_SEQ}) used to detect stale
 *   reads and order writes.
 * \item Flags encoding the btree ID and the node's level in the tree (0 for
 *   leaves).
 * \item A closed \texttt{[min\_key, max\_key]} interval describing the key
 *   range this node covers.
 * \item A \texttt{bkey\_format} descriptor used for packing keys in the
 *   node's first bset.
 * \item The first bset, inline in the header.
 * \end{itemize}
 *
 * The first bset in a node is the result of the last full compaction---it
 * contains all live keys, fully sorted. Subsequent
 * \texttt{btree\_node\_entry} records are journal-order appends: each contains
 * a bset of keys that were written since the last compaction, sorted within
 * the bset but not merged with earlier bsets. On read, all bsets are merged
 * to produce the full sorted key set.
 *
 * \textbf{Bset structure.} A bset (\texttt{struct bset}) is a sorted array of
 * packed keys preceded by a small header: a sequence number, the journal
 * sequence number of the most recent key in the set (used during recovery to
 * discard bsets whose journal entries were lost), flags, and a count of the
 * data that follows. The packed keys are laid out contiguously, each
 * self-describing its total size via the \texttt{u64s} field.
 *
 * \textbf{Key position (bpos).} Every btree key is addressed by a
 * \texttt{bpos}: an (inode, offset, snapshot) triple. The inode and offset
 * fields together identify the logical object and position; the snapshot field
 * enables snapshot-aware btrees to store multiple versions of the same
 * logical key in a single btree, distinguished by their snapshot ID. The
 * entire bpos is treated as a single large integer for comparison, so keys
 * are sorted first by inode, then offset, then snapshot.
 *
 * \textbf{Key structure (bkey).} A \texttt{bkey} is the full unpacked key:
 * a bpos, a size field (used by extent keys to describe a range of offsets),
 * a version (for versioned key-value pairs), and a type tag identifying the
 * value format. The \texttt{u64s} field gives the total size of key plus
 * value in 8-byte units. Following the key header is a type-specific value
 * (e.g., extent pointers, inode data, a directory entry hash).
 *
 * \textbf{Key packing.} On disk, keys are stored in a packed representation
 * (\texttt{bkey\_packed}) to reduce metadata size. A \texttt{bkey\_format}
 * descriptor---stored in the btree node header---specifies, for each of the
 * six key fields (inode, offset, snapshot, size, version\_hi, version\_lo), a
 * base offset and a bit width. Fields that are identical across all keys in a
 * bset (e.g., inode number within a single file's extents) can be stored in
 * zero bits. The packed key retains only the 3-byte header (u64s, format,
 * type) followed by the variable-length packed fields as a single bitstring.
 * This typically compresses keys to 8--16 bytes instead of the 40-byte
 * unpacked form. The format field in each key selects between the node-local
 * packed format (\texttt{KEY\_FORMAT\_LOCAL\_BTREE}) and the full unpacked
 * format (\texttt{KEY\_FORMAT\_CURRENT}), so unpacked keys can coexist with
 * packed keys in the same bset when a key doesn't fit the format.
 *
 * \textbf{Interior node pointers.} Interior (non-leaf) btree nodes contain
 * keys whose values are \texttt{bch\_btree\_ptr\_v2} structures: pointers to
 * child nodes. Each pointer contains the child's sequence number (for
 * staleness detection), the number of sectors written, a \texttt{min\_key}
 * (the actual minimum key in the child, which may differ from the key's bpos
 * in the parent after node splits), and one or more \texttt{bch\_extent\_ptr}
 * entries giving the physical device and offset where the child is stored.
 * The key's bpos in the parent gives the child's \emph{maximum} key; the
 * child covers the range \texttt{(prev\_key, this\_key]}.
 *
 * \subsubsection{Node cache and locking}
 *
 * \bchdoc{btree-node-cache}
 *
 * \bchdoc{btree-locking}
 *
 * \subsubsection{Auxiliary search trees}
 *
 * \bchdoc{auxiliary-search-trees}
 *
 * \subsubsection{Programmer interface}
 *
 * \bchdoc{btree-programmer-interface}
 *
 * \subsubsection{Iterator internals}
 *
 * \bchdoc{btree-iterators}
 *
 * \subsubsection{Key cache and write buffer}
 *
 * Some btrees benefit from specialized access patterns:
 *
 * The \textbf{key cache}: \bchdoc{btree-key-cache}
 *
 * The \textbf{write buffer}: \bchdoc{btree-write-buffer}
 */

use crate::btree::cache::{bch2_fs_btree_evicted_size_exit, bch2_fs_btree_evicted_size_init};
use crate::btree::trans::TransList;
use crate::btree::write_buffer::{bch2_fs_btree_write_buffer_exit, bch2_fs_btree_write_buffer_init};
use crate::c;
use crate::errcode::{ret_to_result_void, BchError};
use crate::fs::Fs;
use crate::locking::RawLockBase;
use crate::new_lock;
use crate::util::log::bch2_ratelimit_state_init;
use crate::util::percpu::PerCpu;
use crate::util::rcu::Flavor;
use crate::util::Printbuf;
use core::ffi::c_int;
use core::ptr::{self, NonNull};

#[cfg(all(kernel, CONFIG_LOCKDEP))]
unsafe extern "C" {
    fn fs_reclaim_acquire(gfp_mask: c::gfp_t);
    fn fs_reclaim_release(gfp_mask: c::gfp_t);
}

/// # Safety
/// Nothing uses @c's transactions after.
unsafe fn btree_iter_exit(c: *mut Fs) {
    let fs = unsafe { &*c };
    let t = &fs.btree.trans;

    if t.bufs.allocated() {
        for buf in t.bufs.get().cpus() {
            let trans = buf.get().trans;

            if let Some(trans) = NonNull::new(trans) {
                fs.with_trans_list(|list| unsafe { list.remove(trans.as_ref()) });
                unsafe { t.pool.free_raw(trans.cast()) };
            }
        }
    }
    unsafe { (*c).btree.trans.bufs.free() };

    fs.with_trans_list(|list| {
        if let Some(trans) = list.first() {
            panic!("{} leaked btree_trans", trans.fn_name());
        }
    });

    for s in &t.stats {
        let mut s = s.lock.lock();
        unsafe { c::rust_kfree(s.max_paths_text.cast()) };
        s.max_paths_text = ptr::null_mut();
    }

    /* Drain pending trans_rcu_free callbacks before the pool is gone. */
    Flavor::RCU.barrier();
}

/// # Safety
/// Nothing else has @c yet.
unsafe fn btree_iter_init_early(c: *mut Fs) {
    unsafe {
        let t = &raw mut (*c).btree.trans;

        TransList::from_c(&raw mut (*t).list).init();
        let Ok(()) = c::seqmutex::init(&raw mut (*t).lock, c"&c->btree.trans.lock", new_lock!(@key));
        let Ok(()) = c::mutex::init(&raw mut (*t).stats_json_lock, c"&c->btree.trans.stats_json_lock",
                                    new_lock!(@key));
        (&raw mut (*t).stats_json_buf).write(Printbuf::new());
    }
}

/// # Safety
/// Nothing else has @c yet.
unsafe fn btree_iter_init(c: *mut Fs) -> Result<(), BchError> {
    match PerCpu::alloc() {
        Ok(p)  => unsafe { (*c).btree.trans.bufs = p },
        Err(_) => return Err(BchError::from_raw(-(c::ENOMEM as c_int))),
    }

    /*
     * static annotation (hackily done) for lock ordering of reclaim vs.
     * btree node locks:
     */
    #[cfg(all(kernel, CONFIG_LOCKDEP))]
    {
        unsafe { fs_reclaim_acquire(c::GFP_KERNEL) };
        let trans = crate::btree_trans!(unsafe { &*c });
        trans.mark_locked(false);
        drop(trans);
        unsafe { fs_reclaim_release(c::GFP_KERNEL) };
    }

    Ok(())
}

/// # Safety
/// Nothing uses @c's btrees after.
pub(crate) unsafe fn btree_exit(c: *mut Fs) {
    let fs = unsafe { &*c };

    bch2_fs_btree_write_buffer_exit(fs);
    fs.btree.key_cache.exit();
    unsafe {
        btree_iter_exit(c);
        bch2_fs_btree_evicted_size_exit(c);
        fs.btree_cache_exit();
    }

    if fs.btree.read_complete_wq.is_some() {
        unsafe { c::drain_workqueue(fs.btree.read_complete_wq.as_ptr()) };
    }
}

/// # Safety
/// Nothing else has @c yet.
pub(crate) unsafe fn btree_init_early(c: *mut Fs) {
    unsafe { btree_iter_init_early(c) }
}

/// # Safety
/// Nothing else has @c yet.
pub(crate) unsafe fn btree_init(c: *mut Fs) -> Result<(), BchError> {
    let fs = unsafe { &*c };

    unsafe { (*c).btree.foreground_merge_threshold = (fs.btree_max_u64s() / 3) as u16 };

    fs.btree_cache_init()?;
    unsafe { btree_iter_init(c) }?;
    fs.btree.key_cache.setup()?;

    unsafe {
        bch2_ratelimit_state_init(&raw mut (*c).btree.read_errors_soft);
        bch2_ratelimit_state_init(&raw mut (*c).btree.read_errors_hard);
    }

    Ok(())
}

/// # Safety
/// Nothing else uses what it makes yet.
pub(crate) unsafe fn btree_init_rw(c: *mut Fs) -> Result<(), BchError> {
    ret_to_result_void(bch2_fs_btree_write_buffer_init(unsafe { &*c }))?;
    ret_to_result_void(unsafe { bch2_fs_btree_evicted_size_init(c) })
}
