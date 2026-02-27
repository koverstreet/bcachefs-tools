
Journal
-------

The journal has a number of tunables that affect filesystem performance.
Journal commits are fairly expensive operations as they require issuing
FLUSH and FUA operations to the underlying devices. By default, we issue
a journal flush one second after a filesystem update has been done; this
is controlled with the ``journal_flush_delay`` option, which takes a
parameter in milliseconds.

Filesystem sync and fsync operations issue journal flushes; this can be
disabled with the ``journal_flush_disabled`` option - the
``journal_flush_delay`` option will still apply, and in the event of a
system crash we will never lose more than (by default) one second of
work. This option may be useful on a personal workstation or laptop, and
perhaps less appropriate on a server.

The journal reclaim thread runs in the background, kicking off btree
node writes and btree key cache flushes to free up space in the journal.
Even in the absence of space pressure it will run slowly in the
background: this is controlled by the ``journal_reclaim_delay``
parameter, with a default of 100 milliseconds.

The journal should be sized sufficiently that bursts of activity do not
fill up the journal too quickly; also, a larger journal mean that we can
queue up larger btree writes. The ``bcachefs device resize-journal`` can
be used for resizing the journal on disk on a particular device - it can
be used on a mounted or unmounted filesystem.

In the future, we should implement a method to see how much space is
currently utilized in the journal.
