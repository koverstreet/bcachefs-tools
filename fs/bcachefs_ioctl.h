/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_IOCTL_H
#define _BCACHEFS_IOCTL_H

/*
 * The ioctl interface: the types are generated, from bcachefs_ioctl.rs, into
 * bcachefs_ioctl_gen.h. This is what's included for them - by userspace too,
 * without bcachefs.h - so it brings what they need.
 */

#include "enum_kind.h"

#include <linux/uuid.h>
#include <asm/ioctl.h>
#include "bcachefs_format.h"
#include "btree/bkey_gen.h"
#include "btree/bkey_types_inline.h"

#include "bcachefs_ioctl_gen.h"
#include "bcachefs_ioctl_inline.h"

#endif /* _BCACHEFS_IOCTL_H */
