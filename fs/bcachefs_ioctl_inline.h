/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_BCACHEFS_IOCTL_INLINE_H
#define _BCACHEFS_BCACHEFS_IOCTL_INLINE_H

/* Included at the end of bcachefs_ioctl.h. */

static inline unsigned replicas_usage_bytes(struct bch_replicas_usage *u)
{
	return offsetof(struct bch_replicas_usage, r) + replicas_entry_bytes(&u->r);
}

static inline struct bch_replicas_usage *
replicas_usage_next(struct bch_replicas_usage *u)
{
	return (void *) u + replicas_usage_bytes(u);
}

/*
 * The path is NUL-terminated, but reclen is 8-byte aligned so there may
 * be extra NUL padding beyond the terminator.
 */
static inline __u32 bch_ioctl_subvol_dirent_path_len(struct bch_ioctl_subvol_dirent *d)
{
	return strnlen(d->path,
		       d->reclen - offsetof(struct bch_ioctl_subvol_dirent, path));
}

#endif /* _BCACHEFS_BCACHEFS_IOCTL_INLINE_H */
