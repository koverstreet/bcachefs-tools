Snapshot backup and send/receive design
=======================================

bcachefs already has the local pieces that an administrator expects from a
snapshot-based backup system: subvolumes, read-only snapshots, recursive
snapshot listings, and usage accounting. That is enough to build local
retention policy around ``bcachefs subvolume snapshot`` and
``bcachefs subvolume list-snapshots``, but it is not yet enough for a real
``bcachefs backup`` command.

The missing part is an export/import protocol. A useful backup command should
not be a wrapper around ``cp -a`` or ``rsync`` that happens to create
snapshots first; it needs to preserve filesystem identity, snapshot
relationships, clone sharing, holes, reflinks, xattrs, bcachefs-specific
extent state, and eventually damaged/error extents. Otherwise it becomes a
tree copy helper, not a filesystem backup tool.

Target user model
-----------------

The intended workflow is the same broad shape as btrfs ``send``/``receive``:

- create a read-only snapshot of a source subvolume;
- export a full stream for the first transfer;
- export later incremental streams between two related snapshots;
- import the stream into a target filesystem as a read-only snapshot;
- keep enough machine-readable metadata to choose the next parent snapshot.

The user-facing command can grow around that protocol:

.. code-block:: text

   bcachefs send SNAPSHOT
   bcachefs send --parent OLD_SNAPSHOT NEW_SNAPSHOT
   bcachefs receive TARGET_SUBVOLUME
   bcachefs backup plan SOURCE TARGET
   bcachefs backup sync SOURCE TARGET

``send`` and ``receive`` are the primitive, auditable operations. ``backup`` is
the policy layer: it can decide which snapshots to create, which parent to use
for an incremental send, and which old snapshots to keep or prune. Keeping the
stream primitives separate matters because users will want to move streams over
ssh, store them as files, put them on tape, or feed them through their own
schedulers.

Required stream semantics
-------------------------

The stream format needs to describe more than file bytes:

- subvolume and snapshot identity;
- parent snapshot identity for incremental streams;
- directory entries, file modes, owners, timestamps, xattrs, and symlinks;
- holes and sparse regions;
- reflink or clone sharing where it can be represented;
- deletions and renames in incremental streams;
- filesystem-specific extent state that generic tools cannot round-trip;
- feature flags so older tools can reject streams they cannot safely import.

The format should be self-describing enough that ``receive`` can fail before
partially importing a stream with unsupported required features. Optional
features may be skipped only when doing so is explicitly lossless or explicitly
requested by the user.

Incremental streams
-------------------

An incremental stream is valid only when the sender and receiver agree on the
parent snapshot. The receive side therefore needs durable metadata that records
which received snapshot corresponds to the sender's parent. Path names alone
are not sufficient: users can rename snapshots, move them under different
retention trees, or keep several backup sets in one filesystem.

The design should include a stable snapshot identity in the stream header. That
identity can be checked against metadata stored on received snapshots before
applying an incremental stream. If the parent does not match, ``receive`` must
fail loudly instead of silently creating a divergent backup chain.

Policy layer
------------

Once send/receive exists, ``bcachefs backup`` can stay deliberately small:

- list source snapshots and target snapshots in a machine-readable form;
- choose the newest common parent;
- create a new read-only source snapshot when requested;
- run ``send`` or ``send --parent`` and pipe it to ``receive``;
- report what it will do in a dry-run/plan mode;
- leave retention policy simple and explicit at first.

More elaborate retention rules, timers, remote transports, tape integration,
and service management can be built on top later. They should not be required
for the first correct stream protocol.

Implementation notes
--------------------

The current userspace command surface already has useful building blocks:

- ``subvolume snapshot`` creates the read-only snapshot that should be sent;
- ``subvolume list`` and ``subvolume list-snapshots --json`` provide discovery;
- ``dump``/``undump`` are metadata-image tools, but they are not a snapshot
  send/receive protocol;
- ``format --source`` and ``migrate`` reuse the tree copy engine, but a tree
  copy is not enough for incremental backup semantics.

The first implementation should therefore avoid promising ``backup`` before
the lower-level stream is real. A maintainable sequence is:

1. define the stream header and compatibility rules;
2. implement full ``send``/``receive`` for one read-only snapshot;
3. add tests that receive the stream into another filesystem and compare
   file contents, metadata, holes, xattrs, and snapshot listing output;
4. add incremental ``send --parent`` tests that include creates, deletes,
   renames, sparse files, and reflinks;
5. add the ``backup plan``/``backup sync`` orchestration layer.

Every receive operation should be crash safe: either the received snapshot is
complete and visible, or it is cleaned up as an incomplete import.

Open questions
--------------

- Which kernel ioctls are needed for efficiently walking a snapshot and
  preserving bcachefs-specific extent state?
- What should the stable snapshot identity be, and where should received
  parent metadata live?
- Should the stream format be bcachefs-specific from the start, or should any
  parts be designed for eventual generic VFS reuse?
- How should damaged/error extents be represented once userspace can query
  them?
- Which features are required for the first mergeable version, and which can
  be negotiated as optional stream features?
