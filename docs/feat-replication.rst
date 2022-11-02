
Replication
~~~~~~~~~~~

bcachefs supports standard RAID1/10 style redundancy with the
``data_replicas`` and ``metadata_replicas`` options. Layout is not fixed
as with RAID10: a given extent can be replicated across any set of
devices; the ``bcachefs fs usage`` command shows how data is replicated
within a filesystem.