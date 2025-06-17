Quotas
------

bcachefs supports conventional user/group/project quotas. Quotas do not
currently apply to snapshot subvolumes, because if a file changes
ownership in the snapshot it would be ambiguous as to what quota data
within that file should be charged to.

When a directory has a project ID set it is inherited automatically by
descendants on creation and rename. When renaming a directory would
cause the project ID to change we return -EXDEV so that the move is done
file by file, so that the project ID is propagated correctly to
descendants - thus, project quotas can be used as subdirectory quotas.