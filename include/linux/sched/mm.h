#ifndef _LINUX_SCHED_MM_H
#define _LINUX_SCHED_MM_H

#include <linux/mm.h>

#define PF_MEMALLOC		0x00000800	/* Allocating memory */
#define PF_MEMALLOC_NOFS	0x00040000	/* All allocation requests will inherit GFP_NOFS */

/**
 * memalloc_flags_save - Add a PF_* flag to current->flags, save old value
 *
 * This allows PF_* flags to be conveniently added, irrespective of current
 * value, and then the old version restored with memalloc_flags_restore().
 */
static inline unsigned memalloc_flags_save(unsigned flags)
{
	unsigned oldflags = ~current->flags & flags;
	current->flags |= flags;
	return oldflags;
}

static inline void memalloc_flags_restore(unsigned flags)
{
	current->flags &= ~flags;
}

/**
 * memalloc_noio_save - Marks implicit GFP_NOIO allocation scope.
 *
 * This functions marks the beginning of the GFP_NOIO allocation scope.
 * All further allocations will implicitly drop __GFP_IO flag and so
 * they are safe for the IO critical section from the allocation recursion
 * point of view. Use memalloc_noio_restore to end the scope with flags
 * returned by this function.
 *
 * Context: This function is safe to be used from any context.
 * Return: The saved flags to be passed to memalloc_noio_restore.
 */
static inline unsigned int memalloc_noio_save(void)
{
	return memalloc_flags_save(PF_MEMALLOC_NOIO);
}

/**
 * memalloc_noio_restore - Ends the implicit GFP_NOIO scope.
 * @flags: Flags to restore.
 *
 * Ends the implicit GFP_NOIO scope started by memalloc_noio_save function.
 * Always make sure that the given flags is the return value from the
 * pairing memalloc_noio_save call.
 */
static inline void memalloc_noio_restore(unsigned int flags)
{
	memalloc_flags_restore(flags);
}

/**
 * memalloc_nofs_save - Marks implicit GFP_NOFS allocation scope.
 *
 * This functions marks the beginning of the GFP_NOFS allocation scope.
 * All further allocations will implicitly drop __GFP_FS flag and so
 * they are safe for the FS critical section from the allocation recursion
 * point of view. Use memalloc_nofs_restore to end the scope with flags
 * returned by this function.
 *
 * Context: This function is safe to be used from any context.
 * Return: The saved flags to be passed to memalloc_nofs_restore.
 */
static inline unsigned int memalloc_nofs_save(void)
{
	return memalloc_flags_save(PF_MEMALLOC_NOFS);
}

/**
 * memalloc_nofs_restore - Ends the implicit GFP_NOFS scope.
 * @flags: Flags to restore.
 *
 * Ends the implicit GFP_NOFS scope started by memalloc_nofs_save function.
 * Always make sure that the given flags is the return value from the
 * pairing memalloc_nofs_save call.
 */
static inline void memalloc_nofs_restore(unsigned int flags)
{
	memalloc_flags_restore(flags);
}

/**
 * memalloc_noreclaim_save - Marks implicit __GFP_MEMALLOC scope.
 *
 * This function marks the beginning of the __GFP_MEMALLOC allocation scope.
 * All further allocations will implicitly add the __GFP_MEMALLOC flag, which
 * prevents entering reclaim and allows access to all memory reserves. This
 * should only be used when the caller guarantees the allocation will allow more
 * memory to be freed very shortly, i.e. it needs to allocate some memory in
 * the process of freeing memory, and cannot reclaim due to potential recursion.
 *
 * Users of this scope have to be extremely careful to not deplete the reserves
 * completely and implement a throttling mechanism which controls the
 * consumption of the reserve based on the amount of freed memory. Usage of a
 * pre-allocated pool (e.g. mempool) should be always considered before using
 * this scope.
 *
 * Individual allocations under the scope can opt out using __GFP_NOMEMALLOC
 *
 * Context: This function should not be used in an interrupt context as that one
 *          does not give PF_MEMALLOC access to reserves.
 *          See __gfp_pfmemalloc_flags().
 * Return: The saved flags to be passed to memalloc_noreclaim_restore.
 */
static inline unsigned int memalloc_noreclaim_save(void)
{
	return memalloc_flags_save(PF_MEMALLOC);
}

/**
 * memalloc_noreclaim_restore - Ends the implicit __GFP_MEMALLOC scope.
 * @flags: Flags to restore.
 *
 * Ends the implicit __GFP_MEMALLOC scope started by memalloc_noreclaim_save
 * function. Always make sure that the given flags is the return value from the
 * pairing memalloc_noreclaim_save call.
 */
static inline void memalloc_noreclaim_restore(unsigned int flags)
{
	memalloc_flags_restore(flags);
}

#endif /* _LINUX_SCHED_MM_H */
