// SPDX-License-Identifier: GPL-2.0

/*
 * Xattrs are fs/xattr.rs. What's here is their VFS end: the xattr_handler
 * tables the VFS dispatches through, their handlers being Rust's, and what
 * listxattr needs to know of them.
 */

#ifndef NO_BCACHEFS_FS

#include "bcachefs.h"

#include "fs/xattr.h"

#include "vfs/rust.h"

#include <linux/dcache.h>
#include <linux/posix_acl_xattr.h>
#include <linux/xattr.h>

static bool bch2_xattr_trusted_list(struct dentry *dentry)
{
	return capable(CAP_SYS_ADMIN);
}

static const struct xattr_handler bch_xattr_user_handler = {
	.prefix	= XATTR_USER_PREFIX,
	.get	= bch2_xattr_get_handler,
	.set	= bch2_xattr_set_handler,
	.flags	= KEY_TYPE_XATTR_INDEX_USER,
};

static const struct xattr_handler bch_xattr_trusted_handler = {
	.prefix	= XATTR_TRUSTED_PREFIX,
	.list	= bch2_xattr_trusted_list,
	.get	= bch2_xattr_get_handler,
	.set	= bch2_xattr_set_handler,
	.flags	= KEY_TYPE_XATTR_INDEX_TRUSTED,
};

static const struct xattr_handler bch_xattr_security_handler = {
	.prefix	= XATTR_SECURITY_PREFIX,
	.get	= bch2_xattr_get_handler,
	.set	= bch2_xattr_set_handler,
	.flags	= KEY_TYPE_XATTR_INDEX_SECURITY,
};

static const struct xattr_handler bch_xattr_bcachefs_handler = {
	.prefix	= "bcachefs.",
	.get	= bch2_xattr_bcachefs_get,
	.set	= bch2_xattr_bcachefs_set,
};

static const struct xattr_handler bch_xattr_bcachefs_effective_handler = {
	.prefix	= "bcachefs_effective.",
	.get	= bch2_xattr_bcachefs_get_effective,
	.set	= bch2_xattr_bcachefs_set_effective,
};

const struct xattr_handler * const bch2_xattr_handlers[] = {
	&bch_xattr_user_handler,
	&bch_xattr_trusted_handler,
	&bch_xattr_security_handler,
	&bch_xattr_bcachefs_handler,
	&bch_xattr_bcachefs_effective_handler,
	NULL
};

static const struct xattr_handler *bch_xattr_handler_map[] = {
	[KEY_TYPE_XATTR_INDEX_USER]			= &bch_xattr_user_handler,
	[KEY_TYPE_XATTR_INDEX_POSIX_ACL_ACCESS]	=
		&nop_posix_acl_access,
	[KEY_TYPE_XATTR_INDEX_POSIX_ACL_DEFAULT]	=
		&nop_posix_acl_default,
	[KEY_TYPE_XATTR_INDEX_TRUSTED]		= &bch_xattr_trusted_handler,
	[KEY_TYPE_XATTR_INDEX_SECURITY]		= &bch_xattr_security_handler,
};

/*
 * What an xattr of @type lists under for @dentry's caller - NULL if it
 * doesn't: the handler's prefix, or its name for a POSIX ACL's.
 */
const char *bch2_xattr_list_prefix(unsigned type, struct dentry *dentry)
{
	const struct xattr_handler *handler = type < ARRAY_SIZE(bch_xattr_handler_map)
		? bch_xattr_handler_map[type]
		: NULL;

	if (!xattr_handler_can_list(handler, dentry))
		return NULL;

	return xattr_prefix(handler);
}

#endif /* NO_BCACHEFS_FS */
