/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _LINUX_VERSION_H
#define _LINUX_VERSION_H

/*
 * No kernel version in userspace: a check of one picks between kernel APIs,
 * and here the API is these headers', whatever kernel the host has. Each
 * check says which branch userspace takes, by __KERNEL__ - one that doesn't
 * is a build error, a division by zero in #if, naming this.
 */
#define LINUX_VERSION_CODE	(1 / LINUX_VERSION_CODE_is_the_kernels_not_userspaces)

#define KERNEL_VERSION(a, b, c) (((a) << 16) + ((b) << 8) + ((c) > 255 ? 255 : (c)))

#endif /* _LINUX_VERSION_H */
