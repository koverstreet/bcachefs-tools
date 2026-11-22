# bcachefs send/recv: design (RFC)

Status: proposal for discussion. No code yet. Checked against commit
`0a4ba50` (2026-11-22).

## Summary

bcachefs can ship read-only snapshots between filesystems ZFS-style without
adding a change-tracking index. The diff between two snapshots is exactly the
keys whose snapshot ID is an ancestor of one snapshot and not the other. The
inodes btree already finds those keys cheaply: snapshot deletion v2 uses the
same trick today.

The stream carries a versioned, logical encoding of inodes, dirents, xattrs
and file data, not raw on-disk keys. The receiver writes it into a hidden
child snapshot of its copy of the base. That snapshot becomes visible only
after the stream ends cleanly.

## Goals and non-goals

Goals:

- Full send of a read-only snapshot subvolume to a byte stream (pipe, file,
  ssh).
- Incremental send between two snapshots of one subvolume lineage. Cost
  scales with the inodes that changed, not with the size of the filesystem.
- Receive on any bcachefs that speaks the stream version, whatever its
  devices, replicas, compression or encryption settings.
- Atomic receive: the received snapshot is either complete and read-only, or
  it does not exist.
- Resumable receive after an interrupted transfer.
- End-to-end integrity: a checksum on every record, with source data checked
  against its stored checksums when read.
- Preserve reflink sharing inside one stream.

Non-goals for v1:

- Raw encrypted send, meaning ciphertext shipped to a host without the key.
- Recursive send of nested subvolumes. Each subvolume is its own stream, and a
  mount point arrives as a stub.
- Interchange with btrfs or ZFS streams.
- Replicating filesystem-wide state: device labels, targets, quotas,
  accounting.
- Sending a writable subvolume. As with btrfs, the sender requires a
  read-only snapshot.

## Background: what the design relies on

File paths are relative to `fs/`.

| Fact | Where | Why send/recv cares |
| --- | --- | --- |
| Keys in snapshotted btrees are positioned `(inode, offset, snapshot)`. Snapshot is the least significant field, so every version of a position sits next to the others. | `format.rs` `Bpos` | One forward pass sees every snapshot's version of a key together |
| Snapshotted btrees: extents, inodes, dirents, xattrs, damage. Reflink is **not** snapshotted. | `format.rs` `Btree` flags | The diff covers 5 btrees; `reflink_p` must be followed into a shared, refcounted btree |
| Snapshot nodes form a tree (`parent`, `children[2]`, `depth`, skiplist). IDs descend, so a child's ID is always lower than its parent's. | `snapshots/format.rs`, `snapshots/snapshot.rs` `create_snapids` | Ancestor sets are cheap to compute from the in-memory snapshot table |
| Taking a snapshot turns the source node into an interior node with 2 new leaves: one for the snapshot, one for the live subvolume. | `snapshots/snapshot.rs` `node_create_children` | Writes after a snapshot always land in snapshot IDs the old snapshot cannot see |
| `Subvolume` holds flags (`ro`, `snap`), `snapshot`, root `inode`, `creation_parent`, `fs_path_parent`, `otime`, `state`. It has **no UUID**. The struct is extensible via `#[grown]`. | `snapshots/format.rs` | Identity across filesystems needs new fields (see On-disk format changes) |
| Snapshot deletion moves keys out of a redundant interior node into its only child. | `snapshots/delete.rs` | The snapshot ID a key lives under can change between sends, so a diff is computed from the live tree and never cached by ID |
| If an extent, dirent or xattr exists in snapshot S, an inode key for that inode also exists in S. Deletion v2 depends on this and falls back to a full scan when accounting shows it missed keys. | `snapshots/delete.rs` `delete_dead_snapshot_keys_v2` | The inodes btree is a ready-made index of what changed |
| Dirent and xattr positions are hashes salted with the directory's `bi_hash_seed`. Probe chains use `hash_whiteout` keys. | `fs/str_hash.rs`, `Btree` holds | Keep the seed and the whiteouts, and the received positions stay valid without rehashing |
| The extent checksum and encryption nonce include the key's `bversion`. | `data/ptrs.rs` `extent_nonce` | Shipping stored bytes verbatim (compressed or encrypted) requires keeping `bversion` or recomputing checksums |
| Existing ioctls: `SUBVOLUME_CREATE/DESTROY/LIST/TO_PATH`, `SNAPSHOT_TREE`, `QUERY_BTREE_KEYS`, `QUERY_UUID`. None stream a subvolume. | `bcachefs_ioctl.rs` | `QUERY_BTREE_KEYS` makes a userspace prototype possible before any kernel work (snapshot filtering for it is in open PR #1223) |

## Architecture

```
 Sender kernel: BCH_IOCTL_SUBVOLUME_SEND
 +---------------------------+   +-----------------------------+   +-----------------------------+
 | Snapshot sets             |   | Index pass                  |   | Emit records                |
 |  pin A, B against delete  |-->|  walk the inodes btree      |-->|  per changed inode, ordered |
 |  D = anc(B) \ anc(A)      |   |  keep inodes keyed in D/R   |   |  compare B's and A's view   |
 |  R = anc(A) \ anc(B)      |   |  cross-check w/ accounting  |   |  INODE, DIRENT, DATA, HOLE  |
 +---------------------------+   +-----------------------------+   +-----------------------------+
                                                                                  |
        byte stream: header, records, END (pipe, ssh or file; the CLI only moves bytes)
                                                                                  |
 Receiver kernel: BCH_IOCTL_SUBVOLUME_RECEIVE                                     v
 +---------------------------+   +-----------------------------+   +=============================+
 | Hidden target             |   | Apply in batches            |   | Finalize                    |
 |  full: new subvolume      |-->|  normal btree updates       |-->|  verify END digest          |
 |  incr: snapshot the base  |   |  triggers fix accounting    |   |  scoped consistency check   |
 |  state = receiving        |   |  checkpoint resume token    |   |  ro + received_uuid, rename |
 +---------------------------+   +-----------------------------+   +=============================+

 Crash before Finalize: resume from the saved token, or destroy with the
 normal subvolume delete path.
```

Both ioctls run in the kernel module. The userspace commands only open
snapshots and move bytes. On the receiver, nothing is visible until Finalize.

## Computing the diff

A key belongs in the stream if and only if its snapshot ID is in D ∪ R. D and
R are two small sets of snapshot IDs taken from the live snapshot table.

Definitions, for base snapshot A and target snapshot B:

- anc(X) = the snapshot IDs on the path from X's leaf to the root of its tree.
- **D = anc(B) \ anc(A)**: nodes holding changes visible in B but not in A.
- **R = anc(A) \ anc(B)**: nodes holding keys visible in A but not in B. For
  a read-only base this is just A's own leaf, and normally it holds no keys.
- Full send is the special case A = ∅, so D = anc(B).

Both sets are bounded by tree depth, typically tens of IDs. Unlike btrfs
(generation numbers) or ZFS (block birth txg), no timestamp is involved:
membership in the snapshot tree is the change record.

Algorithm:

1. **Pin.** Take a send reference on A and B. While it is held, snapshot
   deletion must not delete or merge any node in anc(A) ∪ anc(B). Otherwise
   keys could be renumbered into a different snapshot ID mid-send.
2. **Index pass.** Walk the inodes btree with `ALL_SNAPSHOTS`. Collect every
   inode number that has a key whose snapshot ID is in D ∪ R. This is the same
   loop as `delete_dead_snapshot_keys_v2`.
3. **Accounting cross-check.** Per-snapshot accounting already tells deletion
   when keys exist that the inode index missed. If it reports keys under
   D ∪ R in a btree, rescan that btree in full, as deletion's v1 fallback
   does.
4. **Per changed inode, in inode-number order:**
    1. Inode: resolve the version visible in B and in A. Emit `INODE` if they
       differ, or `INODE_DEL` if only A has one.
    2. Dirents and xattrs: iterate the inode's positions with `ALL_SNAPSHOTS`.
       At each position, take the nearest-ancestor key for B and for A. Emit
       B's key if its snapshot is in D, or if A's visible key came from R.
       Emit a delete if B has nothing and A has something. Keep
       `hash_whiteout` keys so probe chains survive.
    3. Extents: versions in different snapshots can overlap partially.
       Collect the byte ranges covered by any extent key in D ∪ R. For those
       ranges only, read the extent map as B sees it (normal
       snapshot-filtered iteration), then emit `DATA`, `HOLE`, `RESERVATION`,
       `REFLINK_USE` or `ERROR_RANGE`.
5. **Order.** Output is sorted by (inode number, btree, offset). That order
   makes the resume token a single position.

Cost: the index pass reads every key in the inodes btree, which is far
smaller than the extents btree. After that, work is proportional to the
changed inodes and their keys. One weak spot: a changed file is scanned at
file granularity. A 1 TB image with one changed block walks all of that
inode's extent keys (around 8M at 128 KiB extents). That is metadata only,
seconds of CPU, but more than ZFS's block-level pruning. The Alternatives
section covers the fix if it matters.

## Stream format

The stream has its own stable encoding. It never contains raw on-disk bkeys,
because those change with metadata versions (`inode_v2` to `inode_v3`, packed
formats, endianness) and carry device-local pointers.

**Header**, sent once: magic `BCHSEND\0`, stream major/minor version, compat
and incompat feature bits, sender filesystem UUID, target snapshot UUID, base
snapshot UUID (zero for a full send), base `otime`, the sender's metadata
version, block size, string-hash type, and a casefold flag.

**Records:** `type u16 | flags u16 | len u32 | crc32c u32 | payload`. Each
record's CRC covers its type, length and payload.

| Record | Payload | Receiver action |
| --- | --- | --- |
| `INODE` | inode number plus portable fields (mode, uid/gid, times, size, nlink, flags, `bi_hash_seed`, `bi_generation`, project, `bi_dir`, `bi_dir_offset`) | insert the inode key in the new snapshot |
| `INODE_DEL` | inode number | insert an inode whiteout |
| `DIRENT` / `DIRENT_DEL` | dir inode, hash offset, name, target inode or subvolume stub, d_type, whiteout kind | insert at that offset |
| `XATTR` / `XATTR_DEL` | inode, hash offset, type, name, value | insert at that offset |
| `DATA` | inode, offset, length, bytes, the source checksum type | write through the normal write path |
| `HOLE` | inode, offset, length | insert an extent whiteout or delete the range |
| `RESERVATION` | inode, offset, length, replicas | fallocate-style reservation |
| `REFLINK_DEF` / `REFLINK_USE` | stream-local id plus data / inode range plus id and offset | write once, then `reflink_p` to it |
| `ERROR_RANGE` | inode, offset, length | write an `error` extent so the loss stays visible, never zeros |
| `RESUME_POINT` | (inode, btree, offset) | persist as the resume token after the batch commits |
| `END` | record count, byte count, a SHA-256 over all record CRCs | verify, then finalize |

Versioning follows the superblock's compat/incompat model. A receiver rejects
unknown incompat bits and ignores unknown compat bits and unknown record types
flagged skippable.

v1 data is plaintext and uncompressed. The sender reads it through the normal
read path, which verifies stored checksums, and the receiver applies its own
compression, replicas and targets. A later `ENCODED_DATA` record ships
compressed bytes plus checksum so the receiver can skip recompression.
Because checksums are salted with `bversion`, the receiver must either keep
the sender's `bversion` or recompute the checksum.

## Receive path

The receiver applies the stream into a snapshot that nobody can see until
`END` verifies. A crash at any point leaves either a resumable receive or
nothing.

1. **Validate** the header. For an incremental, find the local subvolume
   whose `received_uuid` equals the base UUID. It must be read-only. Clearing
   `ro` on a received subvolume also clears its `received_uuid`, as btrfs
   does, so a base that was ever writable can no longer match.
2. **Create the target.** For a full receive, create a new subvolume in a new
   snapshot tree. For an incremental, snapshot the local base, which is O(1)
   in bcachefs. Either way, set the new state `receiving` and leave it
   unlinked from the namespace.
3. **Apply** records as normal btree updates into the new snapshot ID,
   committing in batches. Triggers run as usual, so accounting, backpointers
   and reflink refcounts stay correct with no special cases.
4. **Rewrite local fields** on the way in: `bi_subvol` and `bi_parent_subvol`
   (new subvolume), `bi_journal_seq` (reset), `bi_sectors` (rebuilt by extent
   triggers, never trusted from the wire). `bi_*_target` options are dropped
   unless `--keep-options` comes with a label map, because targets name the
   sender's disks.
5. **Checkpoint.** After each committed batch, store the last `RESUME_POINT`
   in the subvolume's receive record.
6. **Finalize** once `END` verifies. Run a scoped consistency pass over the
   changed inodes only: nlink, dirent-to-inode, i_size against extents. Then
   set `ro` and `snap`, write `received_uuid`, and link the subvolume into
   place with one rename.
7. **Abort and cleanup.** A subvolume left in `receiving` with no live
   receive is resumable by token. If it is abandoned, the normal subvolume
   destroy path removes it, so no new cleanup code is needed.

**Resume.** `receive --resume-token` prints the stream identity plus the last
committed position. `send --resume <token>` re-pins the same snapshots and
restarts the walk at that position. This works because output is strictly
ordered by position.

## On-disk format changes

One metadata version bump (working name `subvolume_uuids`) adds
cross-filesystem identity and receive state. Nothing changes in extents,
inodes, dirents or btree nodes.

| Change | Where | Purpose |
| --- | --- | --- |
| `uuid` (16 bytes) | `Subvolume`, grown after `pad` | Stable identity across filesystems. A new snapshot gets a new UUID. |
| `parent_uuid` | `Subvolume` | UUID of the subvolume a snapshot was taken from. `creation_parent` is a local ID only. |
| `received_uuid` | `Subvolume` | UUID of the sender's snapshot this one was received from. This is how an incremental finds its base. |
| `receiving` state codeword | `SubvolumeState` | Marks a receive in flight. It must pass the existing compile-time check: at least 14 bit flips from every other state. |
| Receive progress record | `logged_ops` btree, new type `logged_op_receive` | Resume token and batch position. `logged_ops` already exists for crash-resumable operations. |
| UUID index | new non-snapshotted btree `subvolume_uuids`, key = UUID | Base lookup by `received_uuid` without scanning every subvolume. Optional in v1, where a linear scan is fine below ~10k subvolumes. |

Upgrade assigns UUIDs to existing subvolumes in a recovery pass. fsck gains
checks that UUIDs are unique and that a `receiving` subvolume has a matching
progress record. Downgrade drops the new fields, and the grown-struct
mechanism lets old code ignore them.

## Interfaces

Both send and receive run in the kernel module against a mounted filesystem,
and userspace only moves bytes. Pinning snapshots and inserting keys
transactionally both need filesystem-internal locks.

New ioctls:

- `BCH_IOCTL_SUBVOLUME_SEND { target, base, flags, out_fd, resume_token }`:
  the kernel writes the stream to `out_fd`, as `BTRFS_IOC_SEND` does.
- `BCH_IOCTL_SUBVOLUME_RECEIVE { in_fd, dest_dirfd, name, flags, resume }`:
  the kernel reads from `in_fd` and applies.
- `BCH_IOCTL_SUBVOLUME_LIST` output gains `uuid`, `parent_uuid` and
  `received_uuid`.

Commands in bcachefs-tools:

```bash
bcachefs subvolume snapshot -r /mnt/data /mnt/.snap/data@2026-11-22
bcachefs subvolume send /mnt/.snap/data@2026-11-22 | ssh backup bcachefs subvolume receive /backup/data

# incremental
bcachefs subvolume send -p /mnt/.snap/data@2026-11-21 /mnt/.snap/data@2026-11-22 \
  | ssh backup bcachefs subvolume receive /backup/data

# resume
ssh backup bcachefs subvolume receive --resume-token /backup/data
bcachefs subvolume send --resume <token> | ssh backup bcachefs subvolume receive /backup/data

bcachefs subvolume show /mnt/.snap/data@2026-11-22   # uuid, parent_uuid, received_uuid
```

Offline mode comes almost for free. bcachefs-tools runs the same filesystem
code in userspace for unmounted devices, so `send --dev /dev/sdX` and
`receive --dev` can share the implementation.

## Alternatives considered

| Option | How it works | Verdict |
| --- | --- | --- |
| Path-based logical stream (btrfs style) | Sender rebuilds paths and emits mkfile/rename/link/write/clone; receiver replays through the VFS | Rejected for v1. Ordering renames and orphans is the main source of complexity and bugs in btrfs send. It also loses inode numbers and hash seeds, and the receiver is slower. Worth revisiting later as a portable export format. |
| Raw on-disk keys and btree nodes | Ship bkeys exactly as stored | Rejected. It ties the stream to metadata version and endianness, and extent pointers are device-local and need rewriting anyway. |
| Bucket- or block-level replication | Ship changed buckets | Not possible per subvolume: subvolumes share all space in bcachefs. |
| Per-node sequence pruning (ZFS birth-time style) | Store max journal seq in each child pointer and skip subtrees older than A | Deferred. It needs a new `btree_ptr` format and touches every interior-node update. The inode index gets most of the win; revisit only if profiling shows the index pass or large-file scans dominating. |
| Userspace-only, via `QUERY_BTREE_KEYS` | Diff in userspace from raw key reads, data via file reads | Prototype only. It cannot pin snapshots against deletion, and receive still needs in-kernel transactional inserts. |

## Rollout

Five phases. Each one ships only after its gate passes.

1. **Prototype (userspace).** Diff engine over `QUERY_BTREE_KEYS` and the
   userspace filesystem, on unmounted images. Property test: random workloads
   with interleaved snapshots, then send, receive and compare the trees.
   Gate: 10,000 fuzz runs with byte-identical trees, plus the inode-number
   question settled (see Risks).
2. **Format.** Subvolume UUIDs, `received_uuid`, the `receiving` state, the
   receive progress record, the upgrade pass and fsck checks. Gate: fsck is
   clean after upgrade, downgrade and re-upgrade on a populated filesystem.
3. **Full send/receive in kernel.** Plaintext data, no resume. Gate:
   round-trip suite passes, and `kill -9` at random points never leaves a
   visible partial subvolume.
4. **Incremental and resume.** Gate: a chain of 1,000 incrementals
   round-trips, and the index pass is measured on a 100M-inode filesystem.
5. **Efficiency.** `ENCODED_DATA` (compressed passthrough), reflink sharing
   across incrementals, per-node pruning if profiling calls for it. Raw
   encrypted send is evaluated here, not promised.

## Risks and open questions

The first two can change the design. The rest are contained.

- **Inode-number collisions on the receiver.** v1 keeps sender inode numbers,
  so dirent targets, `bi_dir` and hash positions copy over verbatim. The
  allocator (`fs/inode.rs` `create`) avoids any number used in any snapshot,
  but the format itself allows the same number in different snapshot IDs.
  Open: do fsck, `deleted_inodes` and NFS file handles tolerate duplicates
  across unrelated snapshot trees? If not, the receiver needs a persistent
  sender-to-local inode map per received lineage. Settle this in phase 1.
- **Pinning against snapshot deletion.** Deletion moves keys from redundant
  interior nodes into a child. Send needs a way to defer that for one
  snapshot tree while a send holds a reference, without stalling deletion
  filesystem-wide.
- **Reflink across incrementals.** Indirect extents are not snapshotted and
  their indices are local. v1 preserves sharing inside one stream only, so
  sharing that spans two streams is duplicated on the receiver.
- **Casefold and string-hash type.** Dirent offsets depend on both. If the
  receiver cannot match the sender's settings, it must rehash and insert by
  name. That is slower but correct.
- **Damaged inodes.** Decide whether `damage` keys are sent as-is (the
  receiver knows the data is suspect) or whether send refuses without
  `--allow-damaged`. Leaning: send them.
- **Large-file incrementals.** Changes are found at file granularity (see
  Computing the diff). Benchmark a 1 TB VM image with sparse writes before
  deciding on per-node pruning.

## References

- Source files read: `fs/format.rs`, `fs/mod.rs`, `fs/snapshots/format.rs`,
  `fs/snapshots/snapshot.rs`, `fs/snapshots/delete.rs`, `fs/fs/inode.rs`,
  `fs/fs/inode_format.rs`, `fs/fs/str_hash.rs`, `fs/data/ptrs.rs`,
  `fs/bcachefs_ioctl.rs`.
- btrfs send (`BTRFS_IOC_SEND`, `received_uuid`) and ZFS send (birth txg
  pruning, resume tokens, compressed and raw streams) are described from
  general knowledge, not re-checked against their sources for this document.
