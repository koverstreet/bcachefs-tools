
Inline data extents
-------------------

bcachefs supports inline data extents, controlled by the ``inline_data``
option (on by default). When the end of a file is being written and is
smaller than half of the filesystem blocksize, it will be written as an
inline data extent. Inline data extents can also be reflinked (moved to
the reflink btree with a refcount added): as a todo item we also intend
to support compressed inline data extents.
