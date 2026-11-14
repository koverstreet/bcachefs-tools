// SPDX-License-Identifier: GPL-2.0
/*
 * random utility code, for bcache but in theory not specific to bcache
 *
 * Copyright 2010, 2011 Kent Overstreet <kent.overstreet@gmail.com>
 * Copyright 2012 Google, Inc.
 */

#include <linux/bio.h>
#include <linux/blkdev.h>
#include <linux/rcupdate.h>
#include <linux/string.h>
#include <linux/types.h>
#include <linux/sched/debug.h>
#include <linux/static_key.h>
#include <linux/version.h>

#include "util.h"

/*
 * static_branch_unlikely(), for Rust where it has no asm of its own
 * (util/jump_label.rs): out of line, @key isn't a constant the asm can name -
 * the key's count, as static_branch_unlikely() reads it without
 * CONFIG_JUMP_LABEL.
 */
bool bch2_static_branch_unlikely(struct static_key_false *key)
{
	return static_key_enabled(&key->key);
}

/*
 * For Rust: the kernel's Rust allocator API has no __GFP_NOFAIL. Out of line,
 * as locking/locking.c's shims are, for the same reason.
 */
void *rust_kzalloc_nofail(size_t size)
{
	return kzalloc(size, GFP_KERNEL|__GFP_NOFAIL);
}

void *rust_kvzalloc_nofail(size_t size)
{
	return kvzalloc(size, GFP_KERNEL|__GFP_NOFAIL);
}

void rust_kfree(const void *p)
{
	kfree(p);
}

/* For util/rcu.rs: kvfree_rcu() is a macro, of a struct's rcu_head field */
void rust_kvfree(void *p)
{
	kvfree(p);
}

/*
 * kvfree_rcu(p, field), for a head the macro can't name: anywhere in @p. As a
 * void *, as the macro casts it: newer kernels take a struct kvfree_rcu_head,
 * no bigger than an rcu_head.
 */
void rust_kvfree_rcu(void *p, struct rcu_head *head)
{
#ifdef __KERNEL__
	kvfree_call_rcu((void *) head, p);
#else
	kvfree_rcu(p, head);
#endif
}

/* For util/bio/pages.rs: __get_free_pages() is a macro */
void *rust_bio_pages_alloc(size_t size, gfp_t gfp)
{
#ifdef __KERNEL__
	/* page aligned: we don't know the device's dma alignment */
	return (void *) __get_free_pages(gfp, get_order(size));
#else
	return kmalloc(size, gfp);
#endif
}

void rust_bio_pages_free(void *p, size_t size)
{
	free_pages((unsigned long) p, get_order(size));
}

void rust_kfree_rcu_mightsleep(void *p)
{
	kfree_rcu_mightsleep(p);
}

/* For util/sleep.rs: set_current_state() is a macro */
void bch2_set_current_state(unsigned int state)
{
	set_current_state(state);
}

void bch2_set_current_state_running(void)
{
	__set_current_state(TASK_RUNNING);
}
