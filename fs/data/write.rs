// SPDX-License-Identifier: GPL-2.0
/*
 * Copyright 2010, 2011 Kent Overstreet <kent.overstreet@gmail.com>
 * Copyright 2012 Google, Inc.
 */

/* DOC(data-write-path)
 *
 * Writes go through a pipeline of optional transformations: compression
 * (lz4/zstd/gzip), encryption (ChaCha20), and checksumming, applied in that
 * order. The data is then written to each replica device and the extent
 * metadata is updated in the btree atomically. If encryption or compression
 * are not enabled, those stages are skipped entirely.
 *
 * Write point selection determines which device(s) receive the data and how
 * IO from different sources is segregated into separate buckets — see the
 * foreground allocator documentation.
 *
 * Direct IO can bypass internal buffering when no transformations are needed
 * and the user buffer is properly aligned, avoiding an extra memory copy.
 */

/* DOC_LATEX(data-paths)
 * \subsubsection{Write path}
 *
 * \bchdoc{data-write-path}
 *
 * The normal (COW) write path allocates new disk space, encodes the data
 * (compression, then encryption, then checksumming), writes it to each replica
 * device, and inserts a new extent key into the btree. Because writes always go
 * to new locations, the old data remains intact until the btree update commits ---
 * there is no window where a crash can leave partially-written data.
 *
 * Multiple \hyperref[sec:write-points]{write points} are used, selected by
 * hashing the process ID, to segregate unrelated data and help prevent
 * fragmentation.
 *
 * Writes with checksumming or compression enabled must bounce the data through a
 * temporary buffer for checksum stability (the kernel cannot guarantee that a
 * user buffer won't be modified in flight). This is the main per-write overhead
 * of encoded extents.
 *
 * The \hyperref[time-stats:data_write]{{\tt data\_write}} time stat tracks
 * end-to-end write latency (from submission to btree update). The
 * \hyperref[counters:data_write]{{\tt data\_write}} persistent counter tracks
 * total sectors written.
 *
 * \paragraph{Nocow writes}
 *
 * The nocow write path overwrites data in place, bypassing the encoded-extent
 * pipeline entirely: no checksumming, no compression, no encryption, no COW. This
 * eliminates write amplification and bounce buffer overhead at the cost of data
 * integrity features.
 *
 * Nocow writes require per-bucket locking to avoid racing with the move path.
 * This is normally invisible, but contention can appear under heavy concurrent
 * nocow writes; the
 * \hyperref[time-stats:nocow_lock_contended]{{\tt nocow\_lock\_contended}} time
 * stat tracks this (see \hyperref[sec:debugging]{Debugging tools}). When
 * \hyperref[sec:snapshots]{snapshots} or \hyperref[sec:reflink]{reflinks} create
 * shared extents, even nocow files fall back to COW for those extents.
 *
 * Because nocow writes are not checksummed, they cannot be verified by scrub or
 * self-healed from replicas. On encrypted filesystems, nocow data is stored in
 * plaintext. The option is primarily useful for database and VM workloads that
 * manage their own data integrity and need stable disk offsets or minimal write
 * amplification.
 *
 * \subsubsection{Read path}
 *
 * \bchdoc{data-read-path}
 *
 * The read path looks up the extent covering the requested range and reads the
 * data from disk. With multiple replicas, reads stripe across replicas, preferring
 * the one with the lowest current IO latency. For encoded extents, the entire
 * extent must be read even if only a portion was requested, because the checksum
 * covers the full extent and decompression requires the complete input. This
 * per-extent granularity gives much better compression ratios and much smaller
 * metadata (fewer checksums to store) than block-granular approaches. Buffered IO
 * automatically reads entire extents into the page cache, so the only real
 * downside is to small-block random read performance that doesn't fit in cache ---
 * a workload that is rare outside of benchmarks. Block-granular checksums may be
 * added as an option in the future if there is user demand.
 *
 * \paragraph{Error handling}
 *
 * When a checksum mismatch is detected, the same replica is first re-read up to
 * \texttt{checksum\_err\_retry\_nr} times (default 3) to handle transient errors
 * such as bitflips during bus transfer. If retries do not produce a good read and
 * another replica exists, the read is retried from that replica. On successful
 * retry, the failed replica is immediately repaired by rewriting it from the good
 * copy --- this is self-healing, and it happens transparently on every read. If
 * erasure coding is available, the missing data can be reconstructed from parity
 * even with no good replica. If no valid copy can be obtained, the read returns an
 * IO error to userspace.
 *
 * When an extent with a checksum error must be moved (e.g.\ by copygc or
 * reconcile), the move path recomputes a checksum for the corrupted data so it
 * can be written to the new location, but marks the extent as \emph{poisoned}.
 * Poisoned extents are tracked by the \texttt{BCH\_EXTENT\_FLAG\_poisoned} flag:
 * the data is known bad, but the filesystem can still operate on it (move it,
 * account for it). Reads of poisoned extents return an error rather than silently
 * serving corrupt data.
 *
 * \texttt{KEY\_TYPE\_error} extents represent ranges where data has been
 * permanently lost --- for example, after a force device removal that left extents
 * with no remaining replicas. Reads to these ranges return IO errors. These error
 * keys are visible in \texttt{bcachefs list} output and can help diagnose which
 * files were affected by data loss.
 *
 * The \hyperref[time-stats:data_read]{{\tt data\_read}} time stat tracks
 * end-to-end read latency. The \hyperref[counters:data_read]{{\tt data\_read}}
 * counter tracks total sectors read;
 * \hyperref[counters:data_read_bounce]{{\tt data\_read\_bounce}} counts reads
 * that required a bounce buffer (encoded extents), and
 * \hyperref[counters:data_read_retry]{{\tt data\_read\_retry}} counts reads
 * retried due to checksum failure or stale pointers.
 *
 * \paragraph{Promote (caching)}
 *
 * When \texttt{promote\_target} is set, the read path can copy data from a slow
 * device to a fast device on read. This is how bcachefs implements tiered
 * caching: reads that hit a slow tier (e.g.\ HDD) are transparently promoted to a
 * fast tier (e.g.\ SSD) so that subsequent reads are served from the faster
 * device.
 *
 * Promotion is opportunistic: it is skipped if the data already has a copy on the
 * promote target, if the target is congested, or if the per-CPU promote
 * semaphore is exhausted. Promoted copies are written as cached pointers, so they
 * can be evicted under space pressure without data loss.
 *
 * Relevant counters and time stats:
 * \begin{itemize}
 * \item \hyperref[counters:data_read_promote]{{\tt data\_read\_promote}} ---
 *   sectors promoted
 * \item \hyperref[counters:data_read_nopromote_already_promoted]{{\tt nopromote\_already\_promoted}},
 *   \hyperref[counters:data_read_nopromote_congested]{{\tt nopromote\_congested}},
 *   \hyperref[counters:data_read_nopromote_unwritten]{{\tt nopromote\_unwritten}}
 *   --- reasons promotion was skipped
 * \item \hyperref[time-stats:data_promote]{{\tt data\_promote}} --- promotion
 *   write latency
 * \end{itemize}
 *
 * \subsubsection{Data structures}
 *
 * An extent's value (\texttt{struct bch\_extent}) is a variable-length array of
 * typed entries, each self-describing via a type field encoded in the low bits
 * of the first word (a scheme similar to UTF-8: the position of the first set
 * bit determines the type). The entries are defined by
 * \texttt{union bch\_extent\_entry} and can appear in any order, with one rule:
 * a CRC entry applies to all pointers that follow it until the next CRC entry.
 *
 * \paragraph{Extent pointers} (\texttt{struct bch\_extent\_ptr})
 *
 * Each pointer is the ``where is the data'' record: a device number, a
 * 44-bit sector offset (supporting up to 8\,PiB per device), and a generation
 * number that must match the bucket's current generation to be valid (stale
 * pointers are detected and dropped during reads). Flags distinguish cached
 * pointers (evictable copies on a faster tier) from dirty pointers, and mark
 * unwritten reservations.
 *
 * \paragraph{CRC entries} (\texttt{bch\_extent\_crc32/64/128})
 *
 * CRC entries carry the checksum, compression type, and the geometry needed
 * to handle partially-overwritten extents: \texttt{compressed\_size} and
 * \texttt{uncompressed\_size} record the original extent dimensions, and
 * \texttt{offset} records how far into the uncompressed data the currently
 * live region starts (the live region's size is in \texttt{bkey.size}).
 *
 * Three variants exist as a space optimization. Most extents need only a
 * \texttt{crc32} (8 bytes): it supports extents up to 128 sectors with a
 * 32-bit checksum and no nonce. \texttt{crc64} (16 bytes) extends this to 512
 * sectors, adds a 10-bit nonce for encryption, and carries an 80-bit checksum.
 * \texttt{crc128} (24 bytes) is the full-size form: 8192-sector extents, a
 * 13-bit nonce, and a 128-bit checksum --- required when encryption is enabled,
 * since the ChaCha20/Poly1305 MAC is 128 bits. The write path picks the
 * smallest variant that can represent the extent's parameters; the read path
 * unpacks all three into a common \texttt{bch\_extent\_crc\_unpacked} for
 * uniform handling.
 *
 * A CRC entry applies to every pointer after it until the next CRC entry.
 * Initially all replicas share one CRC, but copygc or tiering may rewrite a
 * single replica (possibly trimming it), producing a new CRC for just that
 * pointer. This is why extents can contain multiple CRC entries.
 *
 * \paragraph{Stripe pointer} (\texttt{struct bch\_extent\_stripe\_ptr})
 *
 * Links an extent to an erasure-coding stripe. The \texttt{idx} field
 * identifies the stripe, and \texttt{block} identifies which block within the
 * stripe this extent occupies. When a read fails, the EC subsystem can
 * reconstruct the data from the stripe's parity blocks.
 *
 * \paragraph{Flags entry} (\texttt{struct bch\_extent\_flags})
 *
 * A bitfield of per-extent flags. Currently the only flag is
 * \texttt{poisoned}: the extent contains data known to be corrupt (e.g.\ it
 * failed checksum verification and could not be repaired). Poisoned extents
 * are kept rather than discarded so that the filesystem can still account for
 * them and move them, but reads return an error.
 *
 * \paragraph{Reconcile entry} (\texttt{struct bch\_extent\_reconcile})
 *
 * Embeds IO options (target, compression, replicas, checksum type, erasure
 * coding) directly in the extent. This exists primarily for reflink indirect
 * extents: since an indirect extent may be referenced by many inodes, there is
 * no single ``owning'' inode to look up IO options from. The reconcile entry
 * records what the extent's options \emph{should} be so that background
 * reconciliation can bring them into compliance.
 *
 * \paragraph{Composition}
 *
 * A typical extent value is a sequence of these entries. Some examples:
 * \begin{itemize}
 *   \item Unchecksummed, single replica: \texttt{[ptr]} --- just one pointer,
 *     no CRC. The pointer's offset is adjusted directly when the extent is
 *     trimmed.
 *   \item Checksummed, 2 replicas: \texttt{[crc32, ptr, ptr]} --- one CRC
 *     covers both pointers (same data was written to both locations).
 *   \item After partial copygc of one replica: \texttt{[crc32, ptr, crc32,
 *     ptr]} --- the second pointer was rewritten to a new location covering
 *     only the live portion, so it gets its own CRC with different size/offset
 *     fields.
 *   \item EC extent with encryption: \texttt{[crc128, ptr, stripe\_ptr]} ---
 *     the 128-bit CRC is required for the encryption MAC, and the stripe
 *     pointer links to the parity stripe.
 *   \item Reflink indirect extent: \texttt{[crc32, ptr, ptr, reconcile]} ---
 *     the reconcile entry records the desired IO options for background
 *     processing.
 * \end{itemize}
 *
 * \subsubsection{Encryption}
 * \label{sec:encryption}
 *
 * bcachefs uses authenticated encryption (AEAD) with ChaCha20/Poly1305. Unlike
 * block-layer encryption (AES-XTS), which operates on fixed blocks with no room
 * for nonces or MACs, bcachefs stores a nonce and cryptographic MAC alongside
 * every data pointer, creating a chain of trust from the superblock down to
 * individual extents: any modification, deletion, reordering, or rollback of
 * metadata is detectable. Encryption is all-or-nothing at the filesystem level
 * and can only be enabled at format time.
 *
 * \paragraph{Key hierarchy}
 *
 * The key hierarchy has three levels:
 * \begin{enumerate}
 * 	\item \textbf{Passphrase}: User-supplied, never stored on disk. Fed to
 * 		the scrypt KDF (parameters stored in the
 * 		\hyperref[sec:superblock]{superblock}'s
 * 		\texttt{bch\_sb\_field\_crypt}) to derive a 256-bit
 * 		passphrase key. The KDF runs entirely in userspace, so
 * 		alternative key sources (hardware tokens, key files) can be
 * 		integrated without kernel changes.
 *
 * 	\item \textbf{Master key}: A random 256-bit key generated at format
 * 		time, stored in the superblock encrypted by the passphrase
 * 		key. A magic value (\texttt{BCH\_KEY\_MAGIC}) stored
 * 		alongside the encrypted master key allows verification of a
 * 		correct passphrase without trial decryption of filesystem
 * 		data. Changing the passphrase re-encrypts only the master key,
 * 		not any filesystem data.
 *
 * 	\item \textbf{Per-extent nonces}: Each extent is encrypted with the
 * 		master key and a 128-bit nonce composed of the extent's
 * 		96-bit version number, compression type, and uncompressed
 * 		size, combined with a per-CRC nonce offset. Data encryption
 * 		uses the \texttt{BCH\_NONCE\_EXTENT} domain separator; the
 * 		Poly1305 MAC key uses \texttt{BCH\_NONCE\_POLY}.
 * \end{enumerate}
 *
 * There is currently no key rotation mechanism: the master key is fixed for the
 * lifetime of the filesystem. Key escrow, multi-passphrase unlock, and hardware
 * key (TPM, FIDO2) support are not implemented.
 *
 * \paragraph{Kernel keyring integration}
 *
 * The kernel never sees the passphrase. Instead, userspace derives the passphrase
 * key via scrypt and adds it to the Linux kernel keyring as a \texttt{user} type
 * key with description \texttt{bcachefs:<UUID>}. At mount time, the kernel calls
 * \texttt{request\_key()} to find this key, uses it to decrypt the master key
 * from the superblock, and caches the decrypted master key in kernel memory for
 * the lifetime of the mount.
 *
 * This design inherits the well-known pain points of the Linux keyring subsystem:
 *
 * \begin{itemize}
 * 	\item \textbf{Session isolation}: Keys added to a session keyring are
 * 		not visible from other sessions of the same user. An
 * 		\texttt{ssh} session that runs \texttt{bcachefs unlock} does
 * 		not make the key available to a different terminal, to
 * 		systemd mount units, or to cron jobs. The key must be added
 * 		to \texttt{KEY\_SPEC\_USER\_KEYRING} (the per-UID keyring) to
 * 		be visible across sessions, but this is not always the
 * 		default.
 *
 * 	\item \textbf{Privilege boundaries}: \texttt{sudo mount} uses root's
 * 		keyring, not the calling user's. Systemd units run in
 * 		isolated session contexts. The key must be explicitly placed
 * 		in a keyring that the mounting process can access.
 * \end{itemize}
 *
 * \paragraph{MAC storage}
 *
 * The Poly1305 MAC is stored in the extent's CRC entry. By default, the MAC is
 * truncated to 80 bits (\texttt{chacha20\_poly1305\_80}), which is sufficient for
 * most threat models. The \texttt{wide\_macs} option stores the full 128-bit MAC
 * at the cost of 8 bytes per extent, and is recommended when the storage device
 * itself is untrusted (e.g. USB drives, network storage) and an attacker can make
 * repeated forgery attempts or perform rollback attacks. Metadata always uses
 * 128-bit MACs regardless of the \texttt{wide\_macs} setting.
 *
 * \paragraph{Nonce reuse with external snapshots}
 *
 * AEAD algorithms require that a (key, nonce) pair is never reused for different
 * plaintexts. bcachefs derives extent nonces from the extent's version number,
 * which is unique within a single filesystem instance. However, if the underlying
 * storage is snapshotted externally (LVM, ZFS zvol, VM snapshot, loop device on a
 * reflinked file) and the snapshot is mounted read-write, both instances share the
 * same master key and will derive the same nonces for new writes to the same
 * logical locations. This breaks ChaCha20's semantic security.
 *
 * bcachefs's own snapshot mechanism does not have this problem: internal snapshots
 * share extents via reflinks with COW semantics, and new writes get new version
 * numbers and therefore new nonces.
 *
 * \textbf{Mitigation}: Never mount an external snapshot of an encrypted volume
 * read-write --- keep external snapshots read-only (\texttt{-o nochanges}).
 * Alternatively, place LUKS between the snapshot layer and bcachefs (e.g.
 * LVM $\to$ LUKS $\to$ bcachefs).
 *
 * \subsubsection{Erasure coding}
 * \label{sec:erasure-coding}
 *
 * Erasure coding uses Reed-Solomon parity (the same algorithm as RAID-5/6) to
 * provide redundancy at lower storage cost than full replication. It is enabled
 * per-inode via the \texttt{erasure\_code} option and uses the
 * \texttt{data\_replicas} setting to determine parity count:
 * \texttt{data\_replicas=2} gives one parity block (RAID-5),
 * \texttt{data\_replicas=3} gives two (RAID-6).
 *
 * \paragraph{Write path}
 *
 * Writes are initially replicated: one copy goes to a bucket queued for a new
 * stripe, and an extra replica provides immediate durability. As full stripes
 * accumulate, P/Q parity is written out and the extra replicas are dropped. This
 * gives us erasure coding with no write hole and no fragmentation of writes ---
 * data is written out in the ideal layout, and since stripes are written once and
 * never updated in place, parity is always consistent.
 *
 * The extra replicas are cheap. Since device write caches are only flushed on
 * journal commit (i.e.\ fsync), the allocator can return the extra-replica buckets
 * to the write point for reuse as soon as the stripe commits. In bandwidth-heavy
 * workloads with nothing doing fsyncs, the extra replicas can be overwritten while
 * still in the device writeback cache --- they only cost bus bandwidth, not real
 * disk writes.
 *
 * The allocator segregates EC and non-EC writes at the open-bucket level: a write
 * requesting EC will only be placed in a bucket already tagged for stripe
 * membership, and non-EC writes will never use such buckets. This means a bug in
 * stripe creation, parity computation, or extent updating is structurally scoped
 * to EC-enabled data: non-EC extents never carry \texttt{stripe\_ptr} entries
 * and are never read through EC reconstruction paths. Btree nodes are never
 * placed in EC buckets; this is explicitly checked and flagged as a filesystem
 * inconsistency.
 *
 * If stripe creation fails partway (e.g.\ a crash between writing parity and
 * updating extents), the extra replicas from the staging phase remain valid;
 * the reconcile subsystem detects the incomplete state and retries. Changing the
 * \texttt{erasure\_code} option at runtime triggers reconcile to add or remove EC
 * protection on existing data.
 *
 * \paragraph{Stripe layout}
 *
 * Each block in a stripe is one bucket on one device. Stripe width is determined
 * dynamically: all eligible devices in the target group are used, up to a maximum
 * of 16 blocks per stripe. Eligible devices must be read-write, have nonzero
 * durability, and share the same bucket size (the most common bucket size among
 * candidates is chosen; devices with a different bucket size are excluded). The
 * minimum is \texttt{redundancy + 2} devices (3 for RAID-5, 4 for RAID-6). With
 * $n$ eligible devices, a stripe has $n - \mathrm{redundancy}$ data blocks and
 * \texttt{redundancy} parity blocks, maximizing storage efficiency. Data blocks
 * per stripe may be capped with the \texttt{ec\_max\_data\_blocks} option; the
 * cap excludes parity (a cap of 9 with 2 parity blocks gives stripes 11 blocks
 * wide). When \hyperref[sec:failure-domains]{failure domains} are configured, a
 * stripe takes at most one block per domain, so width is also bounded by the
 * number of available domains. Which devices are used within these bounds is
 * not configurable.
 *
 * Stripe fragmentation is tracked in the LRU btree. When all data blocks in a
 * stripe become empty (sector count zero), the stripe is automatically deleted.
 * Partially empty stripes are candidates for reuse: new stripe creation scans
 * the fragmentation LRU for a stripe with matching parameters (same disk label,
 * algorithm, and redundancy), copies the non-empty blocks into the new stripe,
 * and fills the remaining slots with fresh data. This consolidation recovers
 * space without a full copygc pass.
 *
 * \paragraph{On-disk representation}
 *
 * A stripe is stored as a \texttt{bch\_stripe} key in the stripes btree (ID 6),
 * keyed by stripe index. The fixed-size header contains:
 * \begin{itemize}
 * \item \texttt{sectors} --- bucket size (all blocks in a stripe share the same
 *   bucket size)
 * \item \texttt{algorithm} --- Reed-Solomon variant (4 bits)
 * \item \texttt{nr\_blocks} --- total blocks (data + parity)
 * \item \texttt{nr\_redundant} --- number of parity blocks
 * \item \texttt{csum\_type}, \texttt{csum\_granularity\_bits} --- checksum
 *   algorithm and block granularity for per-block checksums
 * \item \texttt{disk\_label} --- target disk label (8 bits; a limitation noted
 *   for a future \texttt{stripe\_v2})
 * \item \texttt{needs\_reconcile} --- flag indicating the stripe needs
 *   reconcile processing (e.g.\ after partial creation or option change)
 * \end{itemize}
 *
 * After the header, three variable-length sections are packed in order:
 * \texttt{nr\_blocks} \texttt{bch\_extent\_ptr} entries (one per block, giving
 * the device, offset, and generation for each bucket); a 2D array of checksums
 * indexed by \texttt{[block][csum\_block]} where the checksum block size is
 * $2^{\mathtt{csum\_granularity\_bits}}$ sectors; and \texttt{nr\_blocks}
 * \texttt{\_\_le16} sector counts tracking how many sectors of live data each
 * block contains (used for fragmentation tracking and stripe deletion).
 *
 * Each data extent that belongs to a stripe carries an inline
 * \texttt{bch\_extent\_stripe\_ptr} entry with three fields:
 * \texttt{idx} (47-bit stripe index into the stripes btree),
 * \texttt{block} (8-bit block number within the stripe), and
 * \texttt{redundancy} (4-bit copy of the stripe's \texttt{nr\_redundant}, so the
 * read path knows the parity level without looking up the stripe).
 *
 * Several auxiliary btrees support EC operations: the
 * \texttt{bucket\_to\_stripe} btree (ID 26) maps each stripe-member bucket to
 * the stripes referencing it, enabling the allocator and copygc to know when a
 * bucket is part of a stripe; the \texttt{stripe\_backpointers} btree (ID 27)
 * stores backpointers indexed by stripe pointer for data on invalid or removed
 * devices, enabling stripe repair without the original device; and the alloc
 * btree tracks per-bucket \texttt{stripe\_refcount} and \texttt{stripe\_sectors}
 * separately from \texttt{dirty\_sectors}. Backpointers for stripe blocks point
 * back to the stripes btree rather than the extents btree.
 *
 * \paragraph{Reconstruction reads}
 *
 * EC reconstruction reads happen when a device is offline or a checksum mismatch
 * is detected: the read path fetches the remaining data blocks plus parity and
 * reconstructs the missing block using the Reed-Solomon algorithm.
 *
 * \paragraph{Consistency and self-healing}
 *
 * Stripe triggers validate bucket accounting on every stripe insert or delete:
 * parity bucket refcounts and dirty sector counts must be consistent. When a
 * mismatch is detected, the bucket is protected from reuse (refcount held at a
 * safe value) and the \texttt{check\_allocations} recovery pass is automatically
 * scheduled to perform a full repair. Stale pointers detected during
 * reconstruction reads similarly trigger recovery.
 *
 * If stripe creation fails partway (e.g.\ crash between writing parity and
 * updating extents), the extra replicas from the staging phase remain valid
 * data, and the reconcile subsystem detects the incomplete state and retries
 * stripe creation.
 *
 * \subsubsection{Reflink}
 * \label{sec:reflink}
 *
 * Reflink (\texttt{cp --reflink}, \texttt{FICLONE} ioctl) creates copies that
 * share underlying storage. The original extent is moved to the reflink btree
 * with a reference count, and a lightweight pointer (\texttt{KEY\_TYPE\_reflink\_p})
 * is left in the extents btree. Reads through a reflink pointer require two btree
 * lookups instead of one: first the reflink\_p, then the actual data pointers in
 * the reflink btree.
 *
 * \paragraph{On-disk representation}
 *
 * In the extents btree, a \texttt{KEY\_TYPE\_reflink\_p} replaces the original
 * extent. It contains an index (\texttt{REFLINK\_P\_IDX}, 56 bits) pointing
 * into the reflink btree (ID 7), plus \texttt{front\_pad} and
 * \texttt{back\_pad} fields. In the reflink btree, a
 * \texttt{KEY\_TYPE\_reflink\_v} stores the actual data pointers, CRCs, and
 * compression metadata (identical to a regular \texttt{KEY\_TYPE\_extent})
 * preceded by a 64-bit reference count.
 *
 * The pad fields exist because copygc or reconcile may split an indirect extent
 * into fragments. Without the pads, fragments outside the pointer's nominal
 * range would have their refcounts leaked. The pads remember the full range
 * originally referenced so that triggers walk all of the fragments when updating
 * refcounts. If the indirect extent is missing in the live data range (e.g.
 * due to corruption), fsck sets the \texttt{REFLINK\_P\_ERROR} flag on the
 * pointer; gaps only in the padded region adjust the pad instead.
 *
 * \paragraph{Creation and lifecycle}
 *
 * When \texttt{cp --reflink} (or the \texttt{FICLONE} ioctl) creates a reflink,
 * the source extent is converted in place. A new \texttt{KEY\_TYPE\_reflink\_v}
 * is allocated at the end of the reflink btree (by seeking to
 * \texttt{POS\_MAX}), containing the original data pointers and a refcount
 * initialized to zero. The source extent is replaced with a
 * \texttt{KEY\_TYPE\_reflink\_p}. If the source is already a
 * \texttt{reflink\_p}, no conversion is needed. A new \texttt{reflink\_p} is
 * then created in the destination file; btree triggers on the inserts increment
 * the refcount.
 *
 * On insertion or deletion of a \texttt{reflink\_p}, the trigger walks the full
 * referenced range (including pad) in the reflink btree and increments or
 * decrements the refcount on each overlapping \texttt{reflink\_v} fragment,
 * expanding the pads if the indirect extent is larger than expected (due to a
 * prior split). Writing new data over a \texttt{reflink\_p} requires no special
 * logic: the normal btree update inserts a regular \texttt{KEY\_TYPE\_extent},
 * the overwrite trigger decrements the refcount, and when a
 * \texttt{reflink\_v}'s refcount reaches zero, its trigger converts the key to
 * \texttt{KEY\_TYPE\_deleted}, cascading through the normal extent trigger to
 * free disk space and remove backpointers.
 *
 * Reflink is currently a one-way transformation: once an extent becomes
 * indirect, it never converts back, even when the refcount drops to 1. The
 * \texttt{reflink\_v} trigger fires at refcount 0 to delete the indirect
 * extent, but does not de-indirect at refcount 1 because the trigger would
 * need to walk transaction updates to find the sole remaining
 * \texttt{reflink\_p}, and operations like \texttt{fcollapse} and
 * \texttt{finsert} can cause transient refcount fluctuations (1 $\to$ 0
 * $\to$ 1) within a single transaction as extents are moved around. With IO
 * option propagation, de-indirecting at refcount 1 is becoming a more
 * pressing concern, since a lone indirect extent with one reference still
 * pays the cost of an extra btree lookup on every read.
 * Additionally, \texttt{reflink\_p} keys are not merged during btree
 * compaction because a merged pointer could span an unbounded number of
 * \texttt{reflink\_v} fragments; merging requires triggers to walk pending
 * transaction updates and diff overlapping \texttt{reflink\_p} ranges.
 *
 * \paragraph{IO option propagation}
 *
 * The \texttt{reflink\_p} carries a
 * \texttt{REFLINK\_P\_MAY\_UPDATE\_OPTIONS} flag that controls whether IO path
 * options (compression, checksum type, replicas, targets) may propagate from the
 * referencing file to the shared indirect extent. This is a security boundary: a
 * reflink copy of data owned by another user must not allow the copier to
 * decrease replicas or change checksum settings on data they do not own. At
 * creation time, the source file's \texttt{reflink\_p} gets this flag set, but
 * the destination's does not (the VFS layer does not yet pass down the
 * permission context needed to determine whether the copier has write access to
 * the source).
 *
 * A \texttt{reflink\_v} has no backpointer to its owning inode, so it cannot
 * look up per-inode IO options at read time. Instead, the indirect extent
 * embeds a \texttt{bch\_extent\_reconcile} entry that stores the desired IO
 * options alongside \texttt{*\_from\_inode} flags recording which options came
 * from a per-inode setting rather than the filesystem default. At creation time
 * no reconcile entry is added; the data is simply copied verbatim from the
 * source extent.
 *
 * When reconcile scans the extents btree and encounters a \texttt{reflink\_p}
 * with \texttt{REFLINK\_P\_MAY\_UPDATE\_OPTIONS} set, it follows through to
 * the corresponding \texttt{reflink\_v} keys in the reflink btree and updates
 * their embedded reconcile entries with the referencing inode's current
 * options. If the on-disk data does not match (e.g. the inode now requests
 * zstd compression but the data is uncompressed), the reconcile entry's
 * \texttt{need\_rb} bits are set and the data is scheduled for background
 * rewrite. Without the flag, reconcile does not propagate that
 * \texttt{reflink\_p}'s inode options to the indirect extent.
 *
 * The \texttt{reflink\_v} can only hold one set of IO options at a time. Since
 * only the source file's \texttt{reflink\_p} currently gets the
 * \texttt{MAY\_UPDATE\_OPTIONS} flag, there is no conflict when multiple files
 * reference the same indirect extent: the source file's options take
 * precedence, and other referencing files cannot influence the indirect
 * extent's IO path behavior. The read path always uses the CRC and compression
 * metadata stored in the \texttt{reflink\_v}'s extent entries (reflecting how
 * the data was actually written), regardless of the referencing file's current
 * options; the referencing inode's options only affect promote decisions.
 *
 * \paragraph{Interaction with snapshots}
 *
 * The reflink btree is not snapshot-aware: \texttt{reflink\_v} keys are shared
 * across all snapshots. The \texttt{reflink\_p} keys in the extents btree are
 * snapshot-aware, so when a file is snapshotted both subvolumes see the same
 * \texttt{reflink\_p} keys through normal snapshot visibility. Writing to
 * either subvolume creates a new extent in that snapshot and decrements the
 * shared refcount; the other snapshot's \texttt{reflink\_p} is unchanged.
 *
 * \paragraph{Consistency and self-healing}
 *
 * When the read path follows a \texttt{reflink\_p} and discovers the
 * corresponding \texttt{reflink\_v} is missing or partially missing, the
 * reference is repaired in-place: \texttt{front\_pad} and \texttt{back\_pad}
 * are adjusted to trim the reference to the valid range, and the
 * \texttt{REFLINK\_P\_ERROR} flag is set if the missing range overlaps actual
 * data. If a previously-errored indirect extent reappears (e.g.\ after btree
 * node recovery), the error flag is cleared automatically.
 *
 * The \texttt{check\_indirect\_extents} recovery pass walks the reflink btree,
 * validates extent sizes, and drops stale device pointers (generation
 * mismatches). This pass can run online.
 *
 * \subsubsection{Inline data extents}
 *
 * bcachefs supports inline data extents, controlled by the \texttt{inline\_data}
 * option (on by default). When the end of a file is being written and is smaller
 * than \texttt{min(blocksize/2, 1024)} bytes, it will be written as an inline data
 * extent. Inline data extents can also be reflinked: the inline data is moved to
 * the reflink btree as a \texttt{KEY\_TYPE\_indirect\_inline\_data} (which carries
 * a refcount and the inline data bytes) and a \texttt{KEY\_TYPE\_reflink\_p} is
 * left in the extents btree, following the same mechanics as regular extent
 * reflinks.
 *
 * \subsubsection{Move path}
 *
 * The move path is the shared IO engine behind copygc, reconcile, and device
 * evacuation. It reads extents via the normal read path, writes them to a new
 * location, and atomically updates pointers.
 *
 * Background move IO is throttled by two runtime-tunable options, both adjustable
 * via sysfs:
 * \begin{itemize}
 * \item \texttt{move\_bytes\_in\_flight} (default 64\,MB) --- total bytes of
 *   outstanding move IO
 * \item \texttt{move\_ios\_in\_flight} (default 64) --- number of outstanding
 *   requests
 * \end{itemize}
 *
 * \noindent Individual consumers can be disabled: \texttt{copygc\_enabled},
 * \texttt{reconcile\_enabled}, and \texttt{reconcile\_on\_ac\_only} (pauses
 * reconcile on battery power).
 *
 * \subsubsection{Reconcile}
 *
 * The reconcile subsystem ensures that all data and metadata is stored correctly
 * according to configured IO path options. It continuously monitors for
 * mismatches between how data is actually stored and how it should be stored ---
 * whether caused by option changes, device additions or removals, degraded
 * replicas, or any other reason --- and rewrites affected extents via the move
 * path.
 *
 * If reconcile detects an inconsistency without an obvious cause (no option
 * change, no device event), it records an error: something unexpected has
 * happened and needs attention. Degraded data (under-replicated due to a device
 * going offline or being removed) is repaired automatically as soon as
 * sufficient devices are available.
 *
 * The design is state-driven rather than event-driven: reconcile looks at what
 * the current state \emph{should be} and compares it to what it \emph{is}. This
 * means multiple operations compose naturally --- for example, evacuating
 * multiple devices simultaneously just works, because each extent is evaluated
 * independently against the current desired state.
 *
 * \paragraph{Work tracking}
 *
 * Work enters the system in two ways: \emph{triggers} on individual extent
 * updates detect mismatches between current data placement and desired options,
 * and \emph{scans} propagate option changes across all affected inodes. Scans
 * are triggered by device state changes (adding, removing, or changing a
 * device's read-write state) and by inode option changes that affect a directory
 * tree.
 *
 * On SSDs, work is tracked in logical key order in the
 * \texttt{reconcile\_work} and \texttt{reconcile\_hipri} btrees, which is
 * cheap since it matches the natural extent btree ordering. On rotational
 * devices, work is additionally tracked in the \texttt{reconcile\_work\_phys}
 * and \texttt{reconcile\_hipri\_phys} btrees, which reorder work by device LBA
 * so it can be processed sequentially. This avoids random seeks on HDDs and
 * enables parallel processing with one thread per device.
 *
 * The \texttt{reconcile\_pending} btree holds work that failed due to
 * insufficient space or devices. Pending work is only retried after device
 * configuration changes, solving the ``rebalance spinning'' problem where the
 * old rebalance thread would burn CPU retrying moves that could never complete.
 *
 * \paragraph{Priority ordering}
 *
 * The reconcile thread processes work in priority order: high-priority metadata
 * (under-replicated or evacuating) first, then high-priority data, then normal
 * metadata (e.g.\ moving stray metadata to \texttt{metadata\_target}), then
 * normal data, then pending retries.
 *
 * The \texttt{bcachefs reconcile status} command shows current progress, and
 * \texttt{bcachefs reconcile wait} blocks until specified work types complete.
 *
 * \paragraph{Consistency and self-healing}
 *
 * Reconcile is inherently self-healing: its entire purpose is to detect and fix
 * mismatches between desired and actual data placement. Beyond normal background
 * operation, the \texttt{check\_reconcile\_work} recovery pass validates the
 * work btrees against actual extent state, removing stale entries and correcting
 * incorrectly-categorized work items (e.g.\ normal-priority work that should be
 * high-priority). This pass can run online.
 *
 * Extent triggers automatically mark data for reconcile whenever a mismatch is
 * detected --- including degraded writes where the desired replica count could
 * not be satisfied. When a failed device is replaced or a new device is added,
 * all pending work in \texttt{reconcile\_pending} is automatically re-evaluated.
 *
 * \subsubsection{Copygc}
 *
 * \bchdoc{copygc}
 *
 * Copygc relies on backpointers to find live data in fragmented buckets. If
 * missing or inconsistent backpointers are detected during copygc, the
 * backpointer recovery pass is automatically scheduled and run (see
 * \hyperref[sec:backpointers]{Backpointers}).
 *
 * \subsubsection{Scrub}
 *
 * Scrub reads all data on a running filesystem and verifies checksums, detecting
 * silent data corruption (bitrot). When a checksum mismatch is found and a valid
 * redundant copy exists (from replication or erasure coding), the corrupted copy
 * is automatically repaired --- the same self-healing mechanism as the normal read
 * path, but applied proactively to all data rather than waiting for application
 * reads to discover corruption.
 *
 * Scrub walks data in physical (LBA) order using backpointers, which is efficient
 * for rotational devices and avoids the random access pattern that would result
 * from walking the logical extent tree. It can be run on a specific device or on
 * all devices. Progress is reported via sysfs and can be monitored with
 * \texttt{bcachefs data scrub}. Nocow data cannot be scrubbed (no checksums).
 */

use crate::alloc::foreground::{AllocWait, Blocked};
use crate::btree::bkey::prelude::*;
use crate::btree::bkey::{bkey_ge, BkeyS, BkeySC};
use crate::btree::iter::{lockrestart_do, BtreeIter, BtreeIterFlags};
use crate::c;
use crate::data::inline_data::{InlineDataMut, DATA};
use crate::data::keylist::Keylist;
use crate::data::ptrs::{Ptrs, PtrsMut};
use crate::data::write_extent::{write_extent, Extent};
use crate::data::write_nocow::nocow_write;
use crate::data::write_replicas::{InFlight, Refs, ReplicaErrs};
use crate::errcode::{Errcode, ret_to_c, BchError};
use crate::format::{Bpos, DataType, BkeyType, MetadataVersion, SbFeature};
use crate::locking::MemallocFlags;
use crate::debug::trace::events;
use crate::sb::members::DevIoRef;
use crate::snapshots::subvolume;
use crate::os::bio::{Bio, BioBox, BioOwns, BioRange, BioVecs, Buffer, Pages, Typed};
use crate::os::kernel::{local_clock, unlikely, WRITE};
use crate::util::log::LogMsg;
use crate::util::ratelimit::Ratelimit;
use crate::util::refcount::TryRefcount;
use crate::util::task::{requeue, waiter, Resumable, Resume, RunQueue};
use crate::os::workqueue::Queue;
use crate::util::{Nsecs, Printbuf, ToText};
use crate::{btree_trans, log_msg_ratelimited, prt};
use c::WriteFlags as w;
use crate::journal::flush_wait::{Flush, FlushWait};
use core::fmt;
use core::pin::Pin;

#[cfg(CONFIG_BCACHEFS_DEBUG)]
crate::module_param! {
    pub(crate) static write_corrupt_ratio: u32 = 0;
    perm = 0o644, desc = "";
}

type Op = c::bch_write_op;

/// A write bio's io ref on its device.
pub(crate) type DevWriteRef = DevIoRef<c::DevWriteRefKind>;

impl c::bch_write_op {
    pub(crate) fn flags(&self) -> w {
        w::from_bits_retain(self.flags.into())
    }

    pub(crate) fn has(&self, f: w) -> bool {
        u32::from(self.flags) & f.bits() != 0
    }

    pub fn set(&mut self, f: w) {
        self.flags |= f.bits() as u16;
    }

    pub(crate) fn clear(&mut self, f: w) {
        self.flags &= !(f.bits() as u16);
    }

    pub(crate) fn set_error(&mut self, e: BchError) {
        self.error = -e.raw() as i16;
    }

    /// insert_keys' heap buffer freed, if it has one: an op isn't dropped.
    fn keys_free(&mut self) {
        self.insert_keys = Keylist::new();
    }
}

/// Not inlined: its attempt's key and iterator aren't needed in the frame a
/// data update's index update runs under.
#[inline(never)]
fn write_index_default(op: &mut Op, mut flush: Option<&mut Option<Flush<'_>>>) -> Result<(), BchError> {
    let fs = op.fs();
    let subvol = op.subvol;
    let inum = c::subvol_inum {
        subvol: subvol.into(),
        inum:   op.insert_keys.front().expect("an index update with no keys").k().p.inode,
    };

    assert!(subvol != 0, "a write with no subvolume");

    let trans = btree_trans!(fs);
    let check_enospc = op.has(w::check_enospc);

    loop {
        let next = lockrestart_do(&trans, |t| {
            let k = op.insert_keys.front().expect("an index update with no keys");

            let is_last = usize::from(k.k().u64s) == op.insert_keys.u64s();
            let flush = flush.as_deref_mut().filter(|_| is_last);

            /*
             * If we did a degraded write, bch2_bkey_set_needs_reconcile() will add
             * pointers to BCH_SB_MEMBER_INVALID so the extent is accounted as
             * degraded
             */
            let room = (usize::from(k.k().u64s) + 1 + c::BCH_REPLICAS_MAX as usize) * size_of::<u64>();
            let mut sk = t.bkey_reassemble_room(BkeySC::from(k), room)?;

            sk.k_mut().p.snapshot = subvolume::get_snapshot(t, subvol)?;

            let mut iter = BtreeIter::new(t, crate::format::Btree::extents, sk.k().start_pos(),
                                          BtreeIterFlags::SLOTS | BtreeIterFlags::INTENT);

            t.extent_update(inum, &mut iter, &mut sk, &op.res, op.new_i_size, Some(&mut op.i_sectors_delta),
                            check_enospc, op.opts.change_cookie, flush)?;
            Ok(iter.pos())
        })?;

        let k = op.insert_keys.front().expect("an index update with no keys");
        if bkey_ge(next, k.k().p) {
            op.insert_keys.pop_front();
        } else {
            // in place: a direct data key's cut leaves its u64s, so the keys
            // after it stay where they are
            let k = op.insert_keys.front_mut().expect("an index update with no keys");
            crate::data::extents::bch2_cut_front_s(fs, next, BkeyS::from(k));
        }

        if op.insert_keys.is_empty() {
            return Ok(());
        }
    }
}

pub(crate) fn index_update_wq(op: &Op) -> &'static Queue {
    let fs = op.fs();
    if op.bits.watermark() == c::Watermark::copygc as u8 {
        &fs.copygc.wq
    } else {
        &fs.btree_update_wq
    }
}

impl c::write_point {
    pub(crate) fn index_updates(&self) -> Pin<&RunQueue> {
        self.index_updates.as_pin()
    }
}

/// An extent's data, which its replicas are clones of.
pub(crate) enum ExtentData<'a> {
    /// A range of the op's, written in place
    InPlace(BioRange<'a>),
    Own(ExtentBio),
}

/// A bio of an extent's own, its data in it - freed, its buffers with it, as
/// it's dropped, once its replicas are done.
pub(crate) type ExtentBio = BioBox<'static, Bio<ExtentBuf>>;

pub(crate) enum ExtentBuf {
    /// Pages of its own, the extent's data bounced into them: to compress,
    /// checksum or encrypt
    Bounce(Pages<'static>),
    /// The write point's EC stripe buffer, mapped
    Stripe(Buffer<&'static mut [u8]>),
}

impl BioOwns for ExtentBuf {
    type Bvec = c::bio_vec;

    fn release(&mut self, vecs: &mut BioVecs) {
        match self {
            ExtentBuf::Bounce(p) => p.release(vecs),
            ExtentBuf::Stripe(b) => b.release(vecs),
        }
    }

    fn allows_op(op: u32) -> bool {
        Pages::allows_op(op) && Buffer::<&mut [u8]>::allows_op(op)
    }
}

impl Typed for ExtentBuf {}

/// A pass's extents of their own bios: their replicas, in flight, borrow them
/// till the pass is waited on.
pub(crate) type Owned = [Option<ExtentBio>; c::BCH_BKEY_PTRS_MAX as usize];

impl ReplicaErrs {
    /// On @op: its failures - io_error, and
    /// each device's in @op.failed - and its devices to flush, the inode's.
    pub(crate) fn record(&self, op: &mut Op, of: WriteFor<'_>) {
        let fs = op.fs();
        for (dev, status) in self.failed() {
            op.failed.set_blk_status(fs, dev.into(), status);
            op.io_error = 1;
        }
        if let WriteFor::Inode(devs_need_flush) = of {
            for dev in self.unflushed.d.iter() {
                devs_need_flush.set(dev);
            }
        }
    }
}

fn keylist_sectors<const N: usize>(keys: &Keylist<N>) -> u64 {
    keys.iter().map(|k| u64::from(k.k().size)).sum()
}

/* Writes */

fn log_write_error_start(out: &mut Printbuf, op: &Op, update: Option<&c::data_update_index>, offset: u64) {
    prt!(out, "error writing data at ");

    let pos = Bpos { offset, ..op.pos };

    let trans = btree_trans!(op.fs());
    let _ = lockrestart_do(&trans, |t| crate::init::error::inum_offset_err_msg(t, out, op.subvol, pos));
    out.newline();

    write_op_to_text(out, op, update);
}

pub(crate) fn write_op_error(op: &Op, update: Option<&c::data_update_index>, offset: u64, msg: fmt::Arguments<'_>) {
    let mut m = log_msg_ratelimited!(op.fs());

    log_write_error_start(&mut m, op, update, offset);
    prt!(&mut *m, "{msg}");
}

fn __write_done(op: &mut Op) {
    let fs = op.fs();

    debug_assert!(op.open_buckets.nr == 0, "write done with open buckets");

    if op.error == 0 {
        op.error = ret_to_c(fs.journal.error()) as i16;
    }

    fs.times.get(c::TimeStat::data_write as usize).update(op.start_time);
    op.res.put();

    drop(op.write_ref.take());
    op.keys_free();
}

/// The journal flush its last index update asked for done - in process
/// context, if it had to wait - and then the op. One no commit attached -
/// the commit had nothing to commit, or the update failed first - is done
/// already.
async fn write_done(op: &mut Op, flush: Option<Pin<&mut FlushWait<'_>>>) {
    if let Some(f) = flush {
        f.completion().resume_on(Resume::queue(index_update_wq(op))).await;
    }
    __write_done(op);
}

#[inline(never)]
fn write_drop_io_error_ptrs(op: &mut Op) -> Result<(), BchError> {
    let fs = op.fs();
    let failed = &op.failed;

    op.insert_keys.try_for_each_shrink(|k| {
        if k.k().extent_is_direct_data() {
            if let Some(mut p) = BkeyS::from(&mut *k).arm::<PtrsMut>() {
                p.drop_ptrs_noerror(fs, |p| failed.dev(p.p.ptr.dev().into()).is_some());
            }

            if BkeySC::from(&*k).durability_safe(fs).nr_ptrs == 0 {
                return Err(fs.err(Errcode::data_write_io));
            }
        }
        Ok(())
    })
}

/// The failed devices' pointers dropped from the keys of a write that got IO
/// errors - an error if a key's left with none - and logged. Not inlined: its
/// message isn't needed in the frame the index update runs under.
#[cold]
#[inline(never)]
fn write_io_error_keys(op: &mut Op, update: Option<&c::data_update_index>) -> Result<(), BchError> {
    let fs = op.fs();
    let ret = write_drop_io_error_ptrs(op);

    let mut msg = LogMsg::new(fs);

    /*
     * Writing degraded because a device was removed is not news:
     * the removal was reported when it happened, and reconcile
     * restores the replicas. Decided before bch2_ratelimit(),
     * which spends this call site's burst budget - the noise must
     * not crowd out a real error arriving in the same window.
     */
    if ret.is_ok() && op.failed.all_dev_removed() {
        msg.set_suppressed(true);
    } else {
        /* Separate ratelimit_states for hard and soft errors */
        static RATELIMIT: [Ratelimit; 2] = [const { Ratelimit::new() }; 2];
        msg.set_suppressed(fs.ratelimit_suppress(&RATELIMIT[ret.is_err() as usize], crate::here!()));
    }

    let k = op.insert_keys.front().expect("an index update with no keys");
    log_write_error_start(&mut msg, op, update, k.k().start_offset());
    op.failed.to_text(&mut msg, fs);

    match &ret {
        Ok(()) => {
            prt!(&mut *msg, "wrote degraded to ");
            let d = BkeySC::from(k).arm::<Ptrs>().map(|k| k.devs(fs)).unwrap_or_default();
            fs.devs_list_to_text(&mut msg, &d);
            msg.newline();
        }
        Err(e) => prt!(&mut *msg, "error {e}\n"),
    }

    ret
}

/// After a write, update index to point to new data - with w::flush, the
/// write's last update asking for its journal flush.
fn __write_index(op: &mut Op, update: Option<&c::data_update_index>, flush: Option<&mut Option<Flush<'_>>>) {
    let fs = op.fs();
    let flush = flush.filter(|_| op.has(w::submitted));

    /*
     * the btree code sets PF_MEMALLOC_NOIO when it has btree locks held,
     * but will drop locks and do GFP_KERNEL allocations
     *
     * But here, our btree updates really are required for memory reclaim:
     */
    let _noio = MemallocFlags::noio();

    let ret = (|| -> Result<(), BchError> {
        // io_error stays set for the op's later passes: one that wrote nothing
        // has no keys to drop pointers from, and its error was reported
        if unlikely(op.io_error != 0) && !op.insert_keys.is_empty() {
            write_io_error_keys(op, update)?;
        }

        if !op.insert_keys.is_empty() {
            let sectors_start = keylist_sectors(&op.insert_keys);

            let ret = match update {
                None    => write_index_default(op, flush),
                Some(u) => crate::data::update_index::index_update(&btree_trans!(fs), op, u),
            };

            assert!(!ret.as_ref().is_err_and(crate::btree::iter::is_restart),
                    "a write's index update returned a transaction restart");
            assert!(keylist_sectors(&op.insert_keys) == 0 || ret.is_err(),
                    "a write's index update left keys, without an error");

            op.written += (sectors_start - keylist_sectors(&op.insert_keys)) as u32;

            if let Err(e) = &ret {
                if !e.matches(c::EROFS) {
                    let insert = op.insert_keys.front().expect("keys left by an index update's error");
                    write_op_error(op, update, insert.k().start_offset(), format_args!("btree update error: {e}"));
                }
            }

            ret?;
        }
        Ok(())
    })();

    if let Err(e) = ret {
        op.insert_keys.clear();
        op.set_error(e);
        op.set(w::submitted);
    }

    /* If some a bucket wasn't written, we can't erasure code it: */
    op.open_buckets.write_errors(fs, &op.failed);

    op.open_buckets.drain(fs).for_each(drop);
}

/// How a pass ended: the write point it last allocated from, if it found one,
/// and what the allocator blocked on, if it did.
struct Pass {
    wp:      Option<&'static c::write_point>,
    blocked: Option<(Blocked, u32)>,
}

enum Next<'a> {
    Extent(Extent<'a>),
    /// Fresh buckets for an extent that wouldn't decompress: go again
    Again,
    /// No more this pass: IO in flight already, the allocator blocked, or
    /// the write failed
    Stop,
}

/// The pass's next extent, its sectors allocated and its data written out of
/// @data. Out of line and not async: its transaction and memalloc scope go
/// before its replicas are waited on.
#[allow(clippy::too_many_arguments)]
fn next_extent<'s>(op: &mut Op, data: &mut BioRange<'s>, opf: u32, update: Option<&c::data_update_index>,
                   in_caller: bool, stuck: bool, retried_for_encoded: &mut bool, pass: &mut Pass) -> Next<'s> {
    let fs = op.fs();
    let _noio = MemallocFlags::noio();
    let trans = btree_trans!(fs);
    let io_in_flight = op.open_buckets.nr != 0;
    let failed = |op: &mut Op, e: BchError| {
        op.set(w::submitted);
        op.set_error(e);

        /* Extra info on errors from the allocator: */
        if update.is_none() {
            write_op_error(op, None, op.pos.offset, format_args!("__bch2_write(): {e}"));
        }
        Next::Stop
    };

    let ret = lockrestart_do(&trans, |t| {
        let wait = match (io_in_flight, in_caller) {
            (true, _)      => AllocWait::Never,
            (false, true)  => AllocWait::Sync(None),
            (false, false) => AllocWait::Async(None),
        };
        let mut req = t.alloc_request(op.target.into(),
                                      op.opts.erasure_code != 0 && !op.has(w::cached),
                                      Some(&op.devs_have),
                                      op.bits.nr_replicas().into(),
                                      op.opts.data_replicas.into(),
                                      c::Watermark::from_bits(op.bits.watermark().into()),
                                      op.flags(),
                                      wait)?;
        req.ec_max_data_blocks = fs.opts().ec_max_data_blocks.get() as _;

        let ret = t.alloc_sectors(&mut req, op.write_point);
        // the write point last found, whatever came of allocating from it -
        // kept if this attempt found none, as an extent before it may be
        // in flight
        pass.wp = req.wp.or(pass.wp);

        match ret {
            Err(e) if e.matches(Errcode::operation_blocked) && in_caller => {
                t.wait_on_allocator(&mut req, e)?;
                Err(t.restart(Errcode::transaction_restart_nested))
            }
            Err(e) if e.matches(Errcode::operation_blocked) => {
                if let AllocWait::Async(b) = &req.wait {
                    pass.blocked = *b;
                }
                if stuck {
                    fs.allocator_stuck(&req, e);
                }
                Err(e)
            }
            r => r,
        }
    });
    trans.unlock_long();

    let mut l = match ret {
        Ok(l) => l,
        Err(_) if io_in_flight => return Next::Stop,
        Err(e) if e.matches(Errcode::operation_blocked) => return Next::Stop,
        Err(e) => return failed(op, e),
    };

    l.open_buckets_get(&mut op.open_buckets);
    let ret = write_extent(op, data, opf, update, &mut l);

    /*
     * The extent wouldn't decompress, so it has to go out as it came
     * in - and it can't be split, so it needs a write point with room
     * for the whole compressed extent. Retire the buckets that came up
     * short and allocate fresh, the way __bch2_btree_node_alloc() does
     * for a btree node.
     *
     * Once only: nothing constrains encoded_extent_max against bucket
     * size, so fresh buckets aren't guaranteed to be big enough either
     * and retrying on that would never end. Giving up means the write
     * fails, which is what happened before any of this.
     */
    if unlikely(ret.as_ref().is_err_and(|e| e.matches(Errcode::data_write_need_fresh_buckets))) &&
       !*retried_for_encoded {
        *retried_for_encoded = true;
        l.retire_short(op.crc.compressed_size);
        return Next::Again;
    }

    pass.wp = Some(l.done());
    match ret {
        Ok(e)  => Next::Extent(e),
        Err(e) => failed(op, e),
    }
}

/// A pass's extents - as many as its write point and open buckets take -
/// allocated and written out from @data, a position in the write's bio, of
/// @opf, advanced past them: each one's replicas submitted into @io, its own
/// bio, if it has one, kept in @owned till they're done. @in_caller: in its
/// caller's context, where it waits on the allocator. @stuck: its last wait
/// on the allocator ran out allocator_stuck_timeout - blocked again, it's
/// reported.
#[allow(clippy::too_many_arguments)]
async fn write_pass<'s>(op: &mut Op, data: &mut BioRange<'s>, opf: u32, update: Option<&c::data_update_index>,
                        in_caller: bool, stuck: bool, retried_for_encoded: &mut bool,
                        owned: &mut impl Iterator<Item = &'s mut Option<ExtentBio>>,
                        io: &mut InFlight<'s>, at: Resume<'_>) -> Pass {
    let mut pass = Pass { wp: None, blocked: None };
    let opf = opf | write_opf(op);

    loop {
        let key_to_write_offset = op.insert_keys.u64s();

        /* +1 for possible cache device: */
        if usize::from(op.open_buckets.nr) + usize::from(op.bits.nr_replicas()) + 1 > op.open_buckets.v.len() {
            return pass;
        }

        if op.insert_keys.reserve(c::BKEY_EXTENT_U64s_MAX as usize).is_err() {
            return pass;
        }

        let next = next_extent(op, data, opf, update, in_caller, stuck, retried_for_encoded, &mut pass);
        let Extent { data: written, more, reserved } = match next {
            Next::Extent(e) => e,
            Next::Again => continue,
            Next::Stop => return pass,
        };

        if !more {
            op.set(w::submitted);
        }

        let range = match written {
            ExtentData::InPlace(r) => r,
            ExtentData::Own(b)     => owned.next().expect("more extents in a pass than it holds").insert(b).range(),
        };
        let k = op.insert_keys.iter_from(key_to_write_offset).next().expect("a write with no key");
        io.submit::<c::DevWriteRefKind>(range, opf, DataType::user, k, Refs::Get, at).await;

        /* a reserve's memory is the next extent's only once this one's done */
        if !more || reserved {
            return pass;
        }
    }
}

/// What an op's replicas are written with, besides its bio's.
fn write_opf(op: &Op) -> u32 {
    let fs = op.fs();
    let mut opf = c::req_op_REQ_OP_WRITE;

    if op.has(w::update) {
        opf |= c::REQ_SYNC | c::REQ_IDLE;
    }

    /*
     * Internal moves can be issued FUA, making the journal's cache
     * flush a no-op for them. Off by default: the per-write FUA
     * regresses background-move throughput, and the journal flushes
     * every device on commit regardless, so durability is unchanged.
     */
    if op.has(w::update) && fs.opts().move_writes_fua.get() != 0 {
        opf |= c::REQ_FUA;
    }
    opf
}

/// The op's data written, and its index updated, a pass at a time: what one
/// couldn't write - its write point full, its open buckets used up, the
/// allocator blocked - the next does, back @at.
async fn __write(op: &mut Op, wbio: &mut WriteBio<'_>, of: WriteFor<'_>, at: Resume<'_>,
                 mut flush: Option<&mut Option<Flush<'_>>>) {
    let fs = op.fs();
    let update = of.update();
    let mut retried_for_encoded = false;

    /*
     * Sync or no?
     *
     * If we're running asynchronously, wne may still want to block
     * synchronously here if we weren't able to submit all of the IO at
     * once, as that signals backpressure to the caller.
     */
    /* in its caller's context, where it may block, till it first waits for IO */
    let mut in_caller = true;
    let mut stuck = false;

    if unlikely(op.opts.nocow != 0 && fs.opts().nocow_enabled.get() != 0) &&
       nocow_write(op, wbio.bio_mut(), of, flush.as_deref_mut(), &mut in_caller).await {
        return;
    }

    loop {
        op.failed.reset_devs();

        let (pass, pos) = {
            let mut owned: Owned = [const { None }; c::BCH_BKEY_PTRS_MAX as usize];
            let bio = wbio.bio_mut();
            let (mut data, opf) = (bio.range(), bio.opf());
            let mut io = InFlight::new(fs);
            let pass = write_pass(op, &mut data, opf, update, in_caller, stuck, &mut retried_for_encoded,
                                  &mut owned.iter_mut(), &mut io, at).await;
            /* failed before finding a write point: nothing submitted, nothing to wait for */
            if let Some(wp) = pass.wp {
                in_caller = false;
                io.wait(Resume::run_queue(wp.index_updates())).await.record(op, of);
            }
            (pass, data.pos())
        };
        wbio.bio_mut().set_iter(pos);

        if let (true, WriteBio::Update(pages)) = (op.has(w::submitted), &mut *wbio) {
            pages.free_pages();
        }

        __write_index(op, update, flush.as_deref_mut());

        if op.has(w::submitted) {
            return;
        }

        stuck = match pass.blocked {
            Some((on, seen)) => match on.wait_async(fs, seen, at).await {
                Ok(stuck) => stuck,
                Err(e) => {
                    op.set_error(e);
                    return;
                }
            },
            None => {
                requeue(at).await;
                false
            }
        };
    }
}

fn write_data_inline(op: &mut Op, wbio: &mut Bio, update: Option<&c::data_update_index>, data_len: u32,
                     flush: Option<&mut Option<Flush<'_>>>) {
    let fs = op.fs();

    op.failed.reset_devs();

    op.set(w::wrote_data_inline);
    op.set(w::submitted);

    fs.check_set_feature(SbFeature::inline_data);

    // with a checksum of its own, if the filesystem may have them
    let ty = if fs.request_incompat_feature(
        MetadataVersion::inline_data_csum).is_ok() {
        BkeyType::inline_data_v2
    } else {
        BkeyType::inline_data
    };

    let sectors = wbio.range().len() >> 9;
    let p = Bpos { offset: op.pos.offset + u64::from(sectors), ..op.pos };
    let k = c::bkey { type_: ty.0 as u8, p, size: sectors, bversion: op.version, ..c::bkey::new() };

    let len = data_len as usize;
    let val_u64s = (DATA.of(&k).expect("an inline data type") + len).div_ceil(8);
    let room = crate::btree::bkey::BKEY_U64S + val_u64s;
    if let Err(e) = op.insert_keys.reserve(room) {
        op.set_error(e.into());
        return;
    }
    op.pos = p;

    let o = &mut *op;
    o.insert_keys.push_with(room, |id| {
        id.k = k;
        id.set_val_u64s(val_u64s).expect("reserved");

        let d = InlineDataMut::of(BkeyS::from(&mut **id)).expect("an inline data key");
        wbio.range().copy_to(&mut d.data[..len]);
        if let Some(h) = d.csum {
            *h = c::InlineDataCsum::new(fs, &o.opts, d.data);
        }
    });

    __write_index(op, update, flush);
}

#[cold]
#[inline(never)]
fn data_write_trace(out: &mut Printbuf, op: &Op, update: Option<&c::data_update_index>) {
    write_op_to_text(out, op, update)
}

/// Whose a write is: what it tells, besides its op.
#[derive(Clone, Copy)]
pub enum WriteFor<'a> {
    /// A file's, with its bitmask of devices that have had nocow writes
    /// issued to them since last flush
    Inode(&'a c::AtomicDevsMask),
    /// A data update's, which its index update rewrites the extent of
    Update(&'a c::data_update_index),
    /// The tools': no fsync to tell
    Tools,
}

impl<'a> WriteFor<'a> {
    pub(crate) fn update(self) -> Option<&'a c::data_update_index> {
        match self {
            Self::Update(u) => Some(u),
            _ => None,
        }
    }
}

/// A write's bio: its caller's, or a data update's, which owns the pages its
/// data is in - given back once it's written, before the index update.
pub enum WriteBio<'a> {
    Bio(&'a mut Bio),
    Update(&'a mut Bio<Pages<'static>>),
}

impl WriteBio<'_> {
    fn bio_mut(&mut self) -> &mut Bio {
        match self {
            WriteBio::Bio(b) => b,
            WriteBio::Update(b) => b,
        }
    }
}

/**
 * write() - handle a write to a cache device or flash only volume
 *
 * This is the starting point for any data to end up in a cache device; it could
 * be from a normal write, or a writeback write, or a write to a flash only
 * volume - it's also used by the moving garbage collector to compact data in
 * mostly empty buckets.
 *
 * It first writes the data to the cache, creating a list of keys to be inserted
 * (if the data won't fit in a single open bucket, there will be multiple keys);
 * after the data is written it calls bch_journal, and after the keys have been
 * added to the next journal write they're inserted into the btree.
 */
/// @wbio's data written, @wbio its caller's. It's allocated @at, its caller's
/// context, where a blocked allocation waits - backpressure; each pass's IO
/// and index update complete on its write point's index updates, where its
/// buckets are put, so no allocation waits on the completions queued behind
/// it. Done, its task resumed @at - not left where its last wait was.
pub async fn write(op: &mut Op, mut wbio: WriteBio<'_>, of: WriteFor<'_>, at: Resume<'_>) {
    let fs = op.fs();
    let update = of.update();

    assert!(update.is_some() == op.has(w::update), "a write's update and its w::update flag disagree");
    assert!(update.is_some() == matches!(wbio, WriteBio::Update(_)), "a write's update and its bio disagree");

    let len = wbio.bio_mut().range().len();
    let sectors = u64::from(len >> 9);

    if update.is_none() {
        fs.event_add_trace(events::data_write, sectors, |out| data_write_trace(out, op, update));
    } else {
        fs.event_add_trace(events::data_update_write, sectors, |out| data_write_trace(out, op, update));
    }

    assert!(op.bits.nr_replicas() != 0, "a write of no replicas");
    assert!(op.write_point.v != 0, "a write with no write point");
    assert!(op.pos != crate::POS_MAX, "a write at POS_MAX");

    if op.has(w::only_specified_devs) {
        op.set(w::alloc_nowait);
    }

    op.start_time = local_clock();

    let ret = (|| -> Result<(), BchError> {
        if unlikely(len & (fs.opts().block_size.get() as u32 - 1) != 0) {
            write_op_error(op, update, op.pos.offset, format_args!("misaligned write"));
            crate::warn_on!(true);
            return Err(fs.err(Errcode::data_write_misaligned));
        }

        if fs.opts().nochanges.get() != 0 {
            return Err(fs.err(Errcode::erofs_no_writes));
        }

        if !op.has(w::update) {
            op.write_ref = Some(fs.writes.tryget().ok_or_else(|| fs.err(Errcode::erofs_no_writes))?);
        }
        Ok(())
    })();

    if let Err(e) = ret {
        op.set_error(e);
        op.res.put();
        return;
    }

    fs.io_clock[WRITE].increment(sectors);

    let mut slot = core::pin::pin!(None);
    let mut flush = match op.has(w::flush) {
        true  => Some(FlushWait::new(slot.as_mut(), waiter::Async::current().await)),
        false => None,
    };

    let data_len = u64::from(len).min(op.new_i_size.wrapping_sub(op.pos.offset << 9));

    if fs.opts().inline_data.get() != 0 && data_len <= u64::from((fs.block_bytes() / 2).min(1024)) {
        write_data_inline(op, wbio.bio_mut(), update, data_len as u32, Some(&mut flush));
    } else {
        __write(op, &mut wbio, of, at, Some(&mut flush)).await;
    }

    // if no commit took it, none had anything to flush: there's nothing to wait for
    drop(flush);
    write_done(op, slot.as_mut().as_pin_mut()).await;
    requeue(at).await
}

#[cold]
pub fn __write_op_to_text(out: &mut Printbuf, op: &Op) {
    let fs = op.fs();

    if out.nr_tabstops() == 0 {
        out.tabstop_push(32);
    }

    prt!(out, "pos:\t{}\n", op.pos);
    let mut out = out.indent(2);

    prt!(out, "started:\t{}\n", Nsecs(local_clock().wrapping_sub(op.start_time)));

    prt!(out, "flags:\t");
    op.flags().to_text(&mut out);
    out.newline();

    prt!(out, "watermark:\t{}\n", c::Watermark::from_bits(op.bits.watermark().into()).name());

    prt!(out, "nr_replicas:\t{}\n", op.bits.nr_replicas());
    prt!(out, "devs_have:\t");
    fs.devs_list_to_text(&mut out, &op.devs_have);
    out.newline();

    prt!(out, "opts:\t");
    crate::inode_opts::opts_to_text(&mut out, fs, &op.opts);
    out.newline();

    prt!(out, "open_buckets:\t{}", op.open_buckets.nr);
    for ob in &op.open_buckets.v[..usize::from(op.open_buckets.nr)] {
        prt!(out, " {}", ob.map_or(0, |ob| ob.get()));
    }
    out.newline();

    prt!(out, "ret\t{}\n", BchError::from_raw(op.error.into()));
}

#[cold]
pub fn write_op_to_text(out: &mut Printbuf, op: &Op, update: Option<&c::data_update_index>) {
    __write_op_to_text(out, op);

    if let Some(u) = update {
        let fs = op.fs();
        let mut out = out.indent(2);
        prt!(out, "update:\n");
        let mut out = out.indent(2);
        fs.data_update_opts_to_text(&mut out, &op.opts, &u.opts);
        out.newline();

        prt!(out, "old key:\t{}\n", BkeySC::from(u.k.as_ref()).to_text(fs));
    }
}
