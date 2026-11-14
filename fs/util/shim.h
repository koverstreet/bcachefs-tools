/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_UTIL_SHIM_H
#define _BCACHEFS_UTIL_SHIM_H

#include <linux/gfp.h>
#include <linux/types.h>
#include <linux/workqueue.h>

/*
 * util.c's: the kernel's macros and inlines, and what differs between the
 * kernel and userspace, for Rust
 */

void *rust_kzalloc_nofail(size_t size);
void *rust_kvzalloc_nofail(size_t size);
void rust_kfree(const void *p);
void rust_kvfree(void *p);
void rust_kvfree_rcu(void *p, struct rcu_head *head);
void *rust_bio_pages_alloc(size_t size, gfp_t gfp);
void rust_bio_pages_free(void *p, size_t size);
void rust_kfree_rcu_mightsleep(void *p);


#endif /* _BCACHEFS_UTIL_SHIM_H */
