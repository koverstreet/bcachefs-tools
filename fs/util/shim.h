/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_UTIL_SHIM_H
#define _BCACHEFS_UTIL_SHIM_H

#include <linux/gfp.h>
#include <linux/srcu.h>
#include <linux/types.h>
#include <linux/workqueue.h>

struct static_key_false;

/*
 * util.c's: the kernel's macros and inlines, and what differs between the
 * kernel and userspace, for Rust
 */

bool bch2_static_branch_unlikely(struct static_key_false *key);

void *rust_kzalloc_nofail(size_t size);
void *rust_kvzalloc_nofail(size_t size);
void rust_kfree(const void *p);
void rust_kvfree(void *p);
void rust_kvfree_rcu(void *p, struct rcu_head *head);
void *rust_bio_pages_alloc(size_t size, gfp_t gfp);
void rust_bio_pages_free(void *p, size_t size);
void rust_kfree_rcu_mightsleep(void *p);

void bch2_set_current_state(unsigned int state);
void bch2_set_current_state_running(void);

/* rcu.c's: init_srcu_struct() is a macro under lockdep */
int rust_init_srcu_struct(struct srcu_struct *ssp, const char *name, void *key);

#endif /* _BCACHEFS_UTIL_SHIM_H */
