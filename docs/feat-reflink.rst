
Reflink
-------

bcachefs supports reflink, similarly to other filesystems with the same
feature. cp –reflink will create a copy that shares the underlying
storage. Reading from that file will become slightly slower - the extent
pointing to that data is moved to the reflink btree (with a refcount
added) and in the extents btree we leave a key that points to the
indirect extent in the reflink btree, meaning that we now have to do two
btree lookups to read from that data instead of just one.