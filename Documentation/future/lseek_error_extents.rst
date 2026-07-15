Exposing error extents to userspace
===================================

bcachefs represents a region of a file whose data is known lost - an
unrecoverable read error with no good replica, a checksum failure on the last
copy, etc. - as a ``KEY_TYPE_error`` extent: a key covering a byte range, with
no data pointers, that says "this used to be data, it is gone." Reading it
returns ``-EIO``. This is deliberately different from a hole, which reads as
zeros: a hole means "there was never anything here," an error extent means
"there was, and we lost it."

The problem is that userspace has no way to see that distinction.

POSIX gives two ways to inspect a file's layout: ``lseek(fd, off, SEEK_DATA)``
/ ``SEEK_HOLE``, and ``FIEMAP``. Both classify every region as either data or
hole. ``KEY_TYPE_error`` extents currently land on the hole side of that line
- ``bkey_extent_is_data()`` returns false for them, and ``SEEK_HOLE`` /
``SEEK_DATA``, ``FIEMAP``, and the reflink path all key off that. So:

- ``cp --reflink`` (i.e. ``FICLONERANGE``) of a file with an error extent
  silently produces a destination with a *hole* - reads as zeros - where the
  source had an error extent that reads ``-EIO``. The damage is laundered:
  a region that would have raised an error becomes a region of plausible
  zeros, and the next backup or ``cp -a`` is none the wiser.
- There is no efficient way to ask "where in this file are the damaged
  regions?" You can read the whole file and note where you get ``-EIO``, but
  that conflates a persistent error extent with a transient IO error, and it
  costs a full read.
- There is no efficient way to ask "which files in this filesystem have
  damage?" fsck/scrub knows, but doesn't surface it in a queryable form, so a
  "show me everything that needs my attention" tool has to walk the namespace.

A filesystem that quietly turns errors into holes when you ``cp --reflink`` is
doing the opposite of telling you what's going on. And the longer there's no
real interface, the more likely people are to start depending on the buggy
behaviour - e.g. ``SEEK_HOLE`` past an error extent is, right now, the only
way to skip damaged regions, so tooling will grow to rely on it.

Near-term fixes
---------------

These are distinct from the ioctl below and worth doing first:

- ``FICLONE`` / ``FICLONERANGE`` should not silently drop error extents. The
  cleanest option is to carry them through: clone an error extent as an error
  extent, so the destination is logically identical to the source, damaged
  regions included, and still reads ``-EIO`` there - which is honest. (The
  alternatives - return ``-EIO`` from the clone, or require a flag - are
  fallbacks if carrying-through turns out to be impractical.)

- Converting error extents to holes should be an explicit, auditable
  operation, not a side effect of a copy: a ``bcachefs`` subcommand, or an
  ioctl (in the spirit of ``FALLOC_FL_PUNCH_HOLE``), so "I accept this data is
  gone, give me a hole" is a thing the user deliberately asks for.

- ``SEEK_DATA`` / ``SEEK_HOLE`` should classify error extents as **data**, not
  hole. This is the awkward one, so the reasoning:

  ``SEEK_HOLE``/``SEEK_DATA`` is a two-valued classification - data or hole -
  of a thing that is actually three-valued, so an error extent has to go in
  one bucket or the other, and either choice is a lie. ``error -> hole`` lies
  "this is zeros": a copy tool that walks ``SEEK_DATA``/``SEEK_HOLE`` skips
  the region and the destination ends up with plausible zeros where the source
  had lost-and-``-EIO``. ``error -> data`` lies "this is data": the copy tool
  ``read()``\ s the region, gets ``-EIO``, and fails. One lie corrupts
  quietly; the other fails honestly.

  ``error -> data`` is the right call, because a faithful copy of a damaged
  file is *impossible* - so the correct default behaviour of a copy tool that
  hits one is to fail, not to produce a subtly-wrong copy that then sails
  through the next backup. "Evacuate what's left off this dying disk, holes
  where you must" is a real and important need, but it is a *different
  operation* - best-effort recovery - and it should be an explicit opt-in:
  ``cp --ignore-errors``, the explicit error-extent-to-hole conversion above,
  or a ``ddrescue``-shaped tool. The lossy-but-quiet path should not be the
  default.

  The cost is real: a lot of existing infrastructure does
  ``SEEK_DATA``/``SEEK_HOLE``-based sparse copies (coreutils ``cp`` since 9.x,
  ``rsync --sparse``, qemu image handling, journald rotation, essentially
  every backup tool), and with ``error -> data`` all of them start failing on
  any file containing a single error extent, where today they "succeed." That
  will produce "bcachefs broke my backups" reports. But those backups were
  silently corrupting the data - so the failures are surfacing real damage
  that was being hidden, which is the same trade made everywhere else: a loud
  failure that exposes a latent problem beats a quiet success that perpetuates
  it.

Extended lseek ioctl
--------------------

There are really three jobs: *classifying* a region (data/hole/error - the
``SEEK_*`` question, settled above as ``error -> data``), *reporting* a file's
layout, and *navigating* to the next error. ``FIEMAP`` already iterates
extents-with-flags, so a ``FIEMAP_EXTENT_ERROR`` flag is the natural home for
the reporting half - it's a purely additive change, and consumers that don't
know the flag still see a reported extent rather than a hole, which is the
honest default. What ``FIEMAP`` doesn't give you is a cheap "jump to the next
error" the way ``SEEK_HOLE`` gives you "jump to the next hole" - that's what
the ioctl is for.

``lseek(2)``'s ``whence`` enum (``SEEK_SET/CUR/END/DATA/HOLE``) can't
practically be extended, so the mechanism is an ioctl that takes an offset and
returns the resulting offset:

The recommended shape is a *classify-and-extent* query rather than more
``SEEK_*`` whences: given an offset, return ``{ region_type, region_end }``
where ``region_type`` is one of data / hole / error and ``region_end`` is the
offset of the next transition. One call per region transition walks the whole
file in O(regions), and it generalizes cleanly if more region types ever show
up (unwritten, encrypted-and-unreadable, ...).

A simpler alternative, parallel to ``SEEK_DATA``/``SEEK_HOLE``, is a
``SEEK_ERROR`` / ``SEEK_NONERROR`` pair: ``SEEK_ERROR`` returns the next byte
at or after the given offset that is inside an error extent (``-ENXIO`` if
there is none, mirroring ``SEEK_DATA`` past EOF), and ``SEEK_NONERROR`` finds
the end of the run. Either works; the query form is preferred.

bcachefs-specific or generic VFS? Error extents are a bcachefs concept today,
but the situation isn't unique - other filesystems have "this region is known
bad" states (a bad block under ext4, a csum failure under btrfs nodatacow),
they just mostly surface it only as ``-EIO`` with no persistent type. A
generic ``FS_IOC_*`` would be the better home eventually; prototyping it
bcachefs-specific (``BCH_IOCTL_*``) and proposing promotion if it proves out
is a fine path - plenty of filesystem features have taken it.

Finding files with errors, filesystem-wide
------------------------------------------

Lower priority, and noted here mainly so it isn't forgotten: error extents
live in the extents btree, not in a separate index, so "efficiently list all
files with damage" needs either a secondary index ("inode has error extents")
maintained as error keys come and go, or a scan. Either way the interface
would be a ``BCH_IOCTL_*`` on the filesystem (or a sysfs file) that yields a
stream of ``(subvol, inum)`` for inodes with damage - so a repair/triage tool
doesn't have to crawl the namespace.

Summary
-------

What we'll need:

- ``SEEK_DATA``/``SEEK_HOLE`` classifying error extents as data, not hole - so
  a sparse-copy tool fails loudly on a damaged file rather than producing a
  zero-filled forgery of it.
- A ``FIEMAP_EXTENT_ERROR`` flag, so the layout report names error extents for
  what they are.
- An ioctl on a file fd for the navigation that ``FIEMAP`` doesn't give you:
  classify the region at a given offset (data / hole / error) and report where
  it ends - or, the simpler ``SEEK_ERROR``/``SEEK_NONERROR`` pair.
- ``FICLONE`` carrying error extents through to the destination (or refusing),
  rather than silently turning them into holes.
- An explicit operation for converting error extents to holes, for when the
  user genuinely accepts the loss - so the laundering becomes a deliberate act.
- (Lower priority) a way to enumerate files-with-errors filesystem-wide,
  without walking the namespace.
