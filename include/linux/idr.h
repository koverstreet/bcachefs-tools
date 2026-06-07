/*
 * include/linux/idr.h
 *
 * 2002-10-18  written by Jim Houston jim.houston@ccur.com
 *	Copyright (C) 2002 by Concurrent Computer Corporation
 *	Distributed under the GNU GPL license version 2.
 *
 * Small id to pointer translation service avoiding fixed sized
 * tables.
 */

#ifndef __IDR_H__
#define __IDR_H__

struct idr {
};

#define DEFINE_IDR(name)	struct idr name = {}

static inline int idr_alloc(struct idr *idp, void *ptr, int start, int end, gfp_t gfp_mask)
{
	return 0;
}

static inline void idr_remove(struct idr *idp, int id) {}
/*
 * IDA - ID allocator.
 *
 * Userspace bcachefs-tools implementation: a d-ary bitmap tree in a flat
 * array, d == BITS_PER_LONG, eytzinger layout. Each node is a machine word;
 * set bit = corresponding child subtree has at least one free id.
 *
 * Not a mirror of the kernel's xarray-backed ida - we don't need ID -> ptr
 * translation, only alloc/free of unused integers.
 */

struct ida {
	struct mutex		lock;
	unsigned		depth;      /* 0 = uninitialized tree */
	unsigned long		*nodes;     /* (BITS_PER_LONG^depth - 1) / (BITS_PER_LONG - 1) words */
};

#define IDA_INIT(name)		{ .lock.lock = PTHREAD_MUTEX_INITIALIZER }
#define DEFINE_IDA(name)	struct ida name = IDA_INIT(name)

void ida_init(struct ida *);
void ida_destroy(struct ida *);

int ida_alloc_range(struct ida *, unsigned min, unsigned max, gfp_t);
void ida_free(struct ida *, unsigned id);

int ida_alloc_batch(struct ida *, unsigned min, unsigned max, gfp_t,
		    unsigned *ids, unsigned nr);

int ida_find_first(struct ida *);

static inline int ida_alloc(struct ida *ida, gfp_t gfp)
{
	return ida_alloc_range(ida, 0, ~0U, gfp);
}

static inline int ida_alloc_min(struct ida *ida, unsigned min, gfp_t gfp)
{
	return ida_alloc_range(ida, min, ~0U, gfp);
}

static inline int ida_alloc_max(struct ida *ida, unsigned max, gfp_t gfp)
{
	return ida_alloc_range(ida, 0, max, gfp);
}

#endif /* __IDR_H__ */
