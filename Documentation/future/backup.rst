Snapshot backup and send/receive design
=======================================

bcachefs already has the local pieces an administrator expects from a
snapshot-based backup system: subvolumes, read-only snapshots, recursive
snapshot listings, usage accounting. That's enough to build local retention
scripting around ``bcachefs subvolume snapshot`` and
``bcachefs subvolume list-snapshots``. It is not enough for a real
``bcachefs backup`` command - the missing piece is an export/import
protocol.

A backup command that's just ``cp -a`` with snapshots bolted on isn't worth
building - it has to preserve filesystem identity, snapshot relationships,
clone sharing, holes, reflinks, xattrs, bcachefs-specific extent state, and
eventually damaged/error extents, or it's a tree-copy helper wearing a
backup command's name. The right shape, following btrfs ``send``/``receive``:
``send`` and ``receive`` are the primitive, auditable stream operations.
Deciding which snapshots to create, which parent to diff an incremental send
against, and what to prune is a policy layer on top, kept separate, because
people will want to move streams over ssh, onto tape, or through their own
schedulers without touching the primitives.

What a stream has to contain
-----------------------------

A send stream needs to describe more than file bytes: subvolume and snapshot
identity; parent snapshot identity, for incremental streams; directory
entries, file modes, owners, timestamps, xattrs, and symlinks; holes and
sparse regions; reflink or clone sharing where it can be represented;
deletions and renames, for incremental streams; bcachefs-specific extent
state that generic tools can't round-trip; and feature flags, so an older
``receive`` can refuse a stream it doesn't understand instead of silently
importing it wrong. ``receive`` has to be able to reject a stream before it
partially applies it - skipping a feature is only safe when doing so is
explicitly lossless, or explicitly requested.

An incremental stream is only valid if the sender and receiver agree on the
parent snapshot, and path names aren't good enough to establish that:
snapshots get renamed, moved into different retention trees, or a filesystem
ends up hosting several independent backup sets side by side. The stream
header needs a stable snapshot identity, checked against metadata bcachefs
stores on the received snapshot before an incremental stream is allowed to
apply on top of it. A mismatch has to be a loud failure, not a silently
divergent backup chain - and every receive has to be crash-safe: a received
snapshot is either complete and visible, or cleaned up as an incomplete
import, never left half-applied.

What's missing today
---------------------

The current userspace command surface has some of the building blocks, but
not the protocol: ``subvolume snapshot`` creates the read-only snapshot that
would get sent, ``subvolume list-snapshots --json`` gives discovery,
``dump``/``undump`` are metadata-image tools rather than a send/receive
protocol, and ``format --source``/``migrate`` reuse the tree-copy engine -
the same ``cp -a`` problem above, not incremental backup semantics.

The real gap is lower down: there's no efficient way for userspace to walk a
snapshot and read back the extent-level state - reflink sharing, holes,
eventually error extents - that a stream needs to preserve. That's a missing
ioctl surface, not a CLI problem, and it's why this has to start as a stream
design rather than a ``bcachefs backup`` wrapper around today's ioctls.

Open questions
---------------

- Which kernel ioctls are needed for efficiently walking a snapshot and
  reading back bcachefs-specific extent state?
- What should the stable snapshot identity be, and where should received
  parent metadata live?
- Should the stream format be bcachefs-specific from the start, or should
  parts of it be designed for eventual generic VFS reuse?
- How should damaged/error extents be represented in a stream once
  userspace can query them (see ``future/lseek_error_extents.rst``), and
  which stream features are required for a first mergeable version versus
  negotiated as optional?
