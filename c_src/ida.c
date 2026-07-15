// SPDX-License-Identifier: GPL-2.0
/*
 * Userspace ID allocator for bcachefs-tools.
 *
 * Implementation: a d-ary bitmap tree in a flat array, d == BITS_PER_LONG,
 * eytzinger layout. Each node is a machine word; a set bit means the
 * corresponding child subtree has at least one free id. Leaves represent
 * ids directly (set bit == id free).
 *
 * Growth: when the root has no free bits and max would admit a larger tree,
 * we realloc to a tree one level deeper. The old tree becomes the leftmost
 * subtree of the new root; the rest of the new tree is initialized to all
 * bits set (all ids free).
 *
 * Replaces the xarray-backed kernel ida for userspace tools where we don't
 * need id -> pointer translation.
 */

#include "bcachefs.h"
#include <linux/idr.h>
#include <linux/slab.h>
#include <linux/mutex.h>

#define IDA_B		BITS_PER_LONG
#define IDA_B_BITS	(sizeof(long) == 8 ? 6 : 5)

static inline unsigned long ida_nodes_up_to_level(unsigned L)
{
	/* Total nodes at levels 0..L-1 == (B^L - 1) / (B - 1). */
	unsigned long r = 0, p = 1;
	for (unsigned i = 0; i < L; i++) {
		r += p;
		p <<= IDA_B_BITS;
	}
	return r;
}

static inline unsigned long ida_total_nodes(unsigned depth)
{
	return ida_nodes_up_to_level(depth);
}

static inline unsigned long ida_capacity(unsigned depth)
{
	if (!depth)
		return 0;
	/* Saturate to avoid UB from shift-by >= width. */
	if (depth * IDA_B_BITS >= BITS_PER_LONG)
		return ~0UL;
	return 1UL << (depth * IDA_B_BITS);
}

static inline unsigned long ida_first_leaf(unsigned depth)
{
	return depth ? ida_nodes_up_to_level(depth - 1) : 0;
}

/* span covered by a single bit at level L, in a tree of depth D */
static inline unsigned long ida_bit_span(unsigned L, unsigned D)
{
	return 1UL << ((D - L - 1) * IDA_B_BITS);
}

#ifdef IDA_DEBUG
/*
 * Walk the tree verifying: at every interior node, bit k is set iff the
 * k-th child subtree contains at least one free slot. At leaves, bits are
 * the ground truth (each bit == id free). Expensive; debug builds only.
 */
static bool ida_verify_subtree(struct ida *ida, unsigned long i, unsigned L,
			       unsigned D)
{
	unsigned long word = ida->nodes[i];

	if (L == D - 1)
		return true; /* leaf: self-consistent by definition */

	for (unsigned bit = 0; bit < IDA_B; bit++) {
		unsigned long child = (i << IDA_B_BITS) + 1 + bit;
		bool child_has_free = ida->nodes[child] != 0;
		bool parent_says_free = (word >> bit) & 1;

		if (child_has_free != parent_says_free) {
			fprintf(stderr, "ida: invariant violated at node %lu bit %u: "
				"parent bit=%d, child[%lu] word=0x%lx\n",
				i, bit, parent_says_free, child, ida->nodes[child]);
			return false;
		}
		if (!ida_verify_subtree(ida, child, L + 1, D))
			return false;
	}
	return true;
}

static void ida_verify(struct ida *ida)
{
	if (ida->depth == 0) {
		BUG_ON(ida->nodes != NULL);
		return;
	}
	BUG_ON(ida->nodes == NULL);
	BUG_ON(!ida_verify_subtree(ida, 0, 0, ida->depth));
}
#else
static inline void ida_verify(struct ida *ida) {}
#endif

void ida_init(struct ida *ida)
{
	mutex_init(&ida->lock);
	ida->depth = 0;
	ida->nodes = NULL;
}

void ida_destroy(struct ida *ida)
{
	kfree(ida->nodes);
	ida->nodes = NULL;
	ida->depth = 0;
}

static int ida_grow(struct ida *ida, gfp_t gfp)
{
	unsigned new_depth = ida->depth + 1;

	/*
	 * Prevent UB in the shift math of ida_nodes_up_to_level() and
	 * ida_capacity(): we need (new_depth - 1) * IDA_B_BITS < BITS_PER_LONG
	 * for the internal p <<= IDA_B_BITS loop to stay in-range.
	 */
	if ((unsigned long)new_depth * IDA_B_BITS >= BITS_PER_LONG)
		return -ENOSPC;

	unsigned long new_total = ida_total_nodes(new_depth);
	unsigned long *new_nodes = kmalloc_array(new_total, sizeof(unsigned long), gfp);
	if (!new_nodes)
		return -ENOMEM;

	/* Initialize everything to "all free". */
	memset(new_nodes, 0xff, new_total * sizeof(unsigned long));

	/*
	 * Old tree becomes the leftmost subtree of new tree. For each level L
	 * in the old tree (0..depth-1), its B^L nodes move to new level L+1,
	 * which starts at nodes_up_to_level(L+1). The old level L starts at
	 * nodes_up_to_level(L).
	 */
	for (unsigned L = 0; L < ida->depth; L++) {
		unsigned long count = 1UL << (L * IDA_B_BITS);
		memcpy(&new_nodes[ida_nodes_up_to_level(L + 1)],
		       &ida->nodes[ida_nodes_up_to_level(L)],
		       count * sizeof(unsigned long));
	}

	/*
	 * New root bit 0 reflects the old root: if old root had no free bits,
	 * new root's bit 0 is clear. Bits 1..B-1 correspond to fresh subtrees
	 * that are all free, so they stay set.
	 */
	new_nodes[0] = ida->depth && ida->nodes[0] == 0 ? ~1UL : ~0UL;

	kfree(ida->nodes);
	ida->nodes = new_nodes;
	ida->depth = new_depth;
	return 0;
}

/*
 * Compute the bit range [bit_min, bit_max] within a node at level L covering
 * base..base+span*B-1 that overlaps the id range [min, max].
 * Returns true if at least one bit is in-range.
 */
static bool ida_bit_range(unsigned long base, unsigned long span,
			  unsigned min, unsigned max,
			  unsigned *bit_min, unsigned *bit_max)
{
	if (max < base || min >= base + span * IDA_B)
		return false;

	*bit_min = min > base ? (min - base) / span : 0;

	unsigned long max_offset = max - base;
	*bit_max = max_offset / span < IDA_B
		? max_offset / span
		: IDA_B - 1;

	return *bit_min <= *bit_max;
}

static int ida_alloc_descend(struct ida *ida, unsigned long i, unsigned L,
			     unsigned D, unsigned long base,
			     unsigned min, unsigned max)
{
	unsigned long span = ida_bit_span(L, D);
	unsigned bit_min, bit_max;

	if (!ida_bit_range(base, span, min, max, &bit_min, &bit_max))
		return -ENOSPC;

	unsigned long word = ida->nodes[i];
	unsigned long mask = bit_min == 0 ? ~0UL : ~((1UL << bit_min) - 1);
	if (bit_max < IDA_B - 1)
		mask &= (1UL << (bit_max + 1)) - 1;

	unsigned long usable = word & mask;

	while (usable) {
		unsigned bit = __builtin_ctzl(usable);
		unsigned long new_base = base + (unsigned long)bit * span;

		if (L == D - 1) {
			/* Leaf: bit maps to id. */
			BUG_ON(!(word & (1UL << bit))); /* we picked from usable = word & mask */
			BUG_ON(new_base > (unsigned long)INT_MAX);
			ida->nodes[i] &= ~(1UL << bit);

			/* Propagate "full" upward. */
			unsigned long j = i;
			while (j > 0 && ida->nodes[j] == 0) {
				unsigned long parent = (j - 1) >> IDA_B_BITS;
				unsigned bit_in_parent = (j - 1) & (IDA_B - 1);
				BUG_ON(!(ida->nodes[parent] & (1UL << bit_in_parent)));
				ida->nodes[parent] &= ~(1UL << bit_in_parent);
				j = parent;
			}
			return (int)new_base;
		}

		unsigned long child = (i << IDA_B_BITS) + 1 + bit;
		int r = ida_alloc_descend(ida, child, L + 1, D, new_base, min, max);
		if (r >= 0 || r == -ENOMEM)
			return r;

		/* Child's subtree has free slots but none in [min, max]. Skip. */
		usable &= usable - 1;
	}

	return -ENOSPC;
}

static int ida_alloc_locked(struct ida *ida, unsigned min, unsigned max, gfp_t gfp)
{
	if (min > max)
		return -EINVAL;

	for (;;) {
		if (ida->depth == 0) {
			int ret = ida_grow(ida, gfp);
			if (ret)
				return ret;
		}

		int id = ida_alloc_descend(ida, 0, 0, ida->depth, 0, min, max);
		if (id >= 0)
			return id;
		if (id == -ENOMEM)
			return id;

		/*
		 * -ENOSPC: either current tree is full, or all its free bits
		 * are outside [min, max]. Grow if there's room for ids above
		 * current capacity (and max allows).
		 */
		if (ida_capacity(ida->depth) > max)
			return -ENOSPC;
		if (ida_capacity(ida->depth) >= (unsigned long)INT_MAX + 1)
			return -ENOSPC;

		int ret = ida_grow(ida, gfp);
		if (ret)
			return ret;
	}
}

int ida_alloc_range(struct ida *ida, unsigned min, unsigned max, gfp_t gfp)
{
	guard(mutex)(&ida->lock);
	int ret = ida_alloc_locked(ida, min, max, gfp);
	if (ret >= 0) {
		BUG_ON((unsigned)ret < min || (unsigned)ret > max);
		ida_verify(ida);
	}
	return ret;
}

int ida_alloc_batch(struct ida *ida, unsigned min, unsigned max, gfp_t gfp,
		    unsigned *ids, unsigned nr)
{
	BUG_ON(nr && !ids);

	guard(mutex)(&ida->lock);
	unsigned out = 0;
	while (out < nr) {
		int id = ida_alloc_locked(ida, min, max, gfp);
		if (id < 0) {
			if (out == 0 && id != -ENOSPC)
				return id;
			break;
		}
		ids[out++] = (unsigned)id;
	}
	if (out)
		ida_verify(ida);
	return (int)out;
}

void ida_free(struct ida *ida, unsigned id)
{
	guard(mutex)(&ida->lock);

	BUG_ON(ida->depth == 0);
	BUG_ON(id >= ida_capacity(ida->depth));

	unsigned long leaf_idx = ida_first_leaf(ida->depth) + id / IDA_B;
	unsigned bit = id & (IDA_B - 1);

	/* Double-free: the bit must currently be CLEAR (id allocated). */
	BUG_ON(ida->nodes[leaf_idx] & (1UL << bit));

	bool was_zero = ida->nodes[leaf_idx] == 0;
	ida->nodes[leaf_idx] |= 1UL << bit;

	/* Propagate "has free" upward for nodes that were previously fully allocated. */
	unsigned long j = leaf_idx;
	while (was_zero && j > 0) {
		unsigned long parent = (j - 1) >> IDA_B_BITS;
		unsigned bit_in_parent = (j - 1) & (IDA_B - 1);
		was_zero = ida->nodes[parent] == 0;
		ida->nodes[parent] |= 1UL << bit_in_parent;
		j = parent;
	}

	ida_verify(ida);
}

int ida_find_first(struct ida *ida)
{
	guard(mutex)(&ida->lock);

	if (ida->depth == 0)
		return -1;

	unsigned long first = ida_first_leaf(ida->depth);
	unsigned long n_leaves = ida_capacity(ida->depth) / IDA_B;

	for (unsigned long k = 0; k < n_leaves; k++) {
		unsigned long allocated = ~ida->nodes[first + k];
		if (allocated)
			return (int)(k * IDA_B + __builtin_ctzl(allocated));
	}

	return -1;
}
