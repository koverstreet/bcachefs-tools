/*
  File: linux/posix_acl_xattr.h

  Extended attribute system call representation of Access Control Lists.

  Copyright (C) 2000 by Andreas Gruenbacher <a.gruenbacher@computer.org>
  Copyright (C) 2002 SGI - Silicon Graphics, Inc <linux-xfs@oss.sgi.com>
 */
#ifndef _POSIX_ACL_XATTR_H
#define _POSIX_ACL_XATTR_H

#include <uapi/linux/xattr.h>

/* Supported ACL a_version fields */
#define POSIX_ACL_XATTR_VERSION	0x0002

/* An undefined entry e_id value */
#define ACL_UNDEFINED_ID	(-1)

typedef struct {
	__le16			e_tag;
	__le16			e_perm;
	__le32			e_id;
} posix_acl_xattr_entry;

typedef struct {
	__le32			a_version;
	posix_acl_xattr_entry	a_entries[0];
} posix_acl_xattr_header;

extern const struct xattr_handler nop_posix_acl_access;
extern const struct xattr_handler nop_posix_acl_default;

#endif	/* _POSIX_ACL_XATTR_H */
