Checksumming
~~~~~~~~~~~~

bcachefs supports both metadata and data checksumming - crc32c by
default, but stronger checksums are available as well. Enabling data
checksumming incurs some performance overhead - besides the checksum
calculation, writes have to be bounced for checksum stability (Linux
generally cannot guarantee that the buffer being written is not modified
in flight), but reads generally do not have to be bounced.

Checksum granularity in bcachefs is at the level of individual extents,
which results in smaller metadata but means we have to read entire
extents in order to verify the checksum. By default, checksummed and
compressed extents are capped at 64k. For most applications and usage
scenarios this is an ideal trade off, but small random ``O_DIRECT``
reads will incur significant overhead. In the future, checksum
granularity will be a per-inode option.
