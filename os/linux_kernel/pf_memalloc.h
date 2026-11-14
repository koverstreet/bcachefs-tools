/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_OS_LINUX_KERNEL_PF_MEMALLOC_H
#define _BCACHEFS_OS_LINUX_KERNEL_PF_MEMALLOC_H

#include <linux/sched.h>

/*
 * Upstream's, until the revert gave their bits back to sched.h's holes - so
 * the page allocator doesn't see them: our Rust allocation entry points apply
 * them (util/alloc.rs, current_gfp_context()), and nothing else does.
 *
 * Whichever holes this kernel still has: upstream fills them as it goes (7.4's
 * PF_NO_NOTIFY_SIGNAL took 0x00800000), and distro kernels sooner. Rust has
 * them from here too, through codegen.h, so the two agree.
 */
#ifndef PF_MEMALLOC_NORECLAIM

#if   defined(PF__HOLE__01000000)
#define PF_MEMALLOC_NORECLAIM	PF__HOLE__01000000
#elif defined(PF__HOLE__02000000)
#define PF_MEMALLOC_NORECLAIM	PF__HOLE__02000000
#elif defined(PF__HOLE__40000000)
#define PF_MEMALLOC_NORECLAIM	PF__HOLE__40000000
#elif defined(PF__HOLE__00800000)
#define PF_MEMALLOC_NORECLAIM	PF__HOLE__00800000
#elif defined(PF__HOLE__00010000)
#define PF_MEMALLOC_NORECLAIM	PF__HOLE__00010000
#elif defined(PF__HOLE__00020000)
#define PF_MEMALLOC_NORECLAIM	PF__HOLE__00020000
#else
#error "PF_MEMALLOC_NORECLAIM: none of sched.h's PF__HOLE__* we know of is free - find one that is"
#endif

#if   defined(PF__HOLE__02000000) && PF__HOLE__02000000 != PF_MEMALLOC_NORECLAIM
#define PF_MEMALLOC_NOWARN	PF__HOLE__02000000
#elif defined(PF__HOLE__40000000) && PF__HOLE__40000000 != PF_MEMALLOC_NORECLAIM
#define PF_MEMALLOC_NOWARN	PF__HOLE__40000000
#elif defined(PF__HOLE__00800000) && PF__HOLE__00800000 != PF_MEMALLOC_NORECLAIM
#define PF_MEMALLOC_NOWARN	PF__HOLE__00800000
#elif defined(PF__HOLE__00010000) && PF__HOLE__00010000 != PF_MEMALLOC_NORECLAIM
#define PF_MEMALLOC_NOWARN	PF__HOLE__00010000
#elif defined(PF__HOLE__00020000) && PF__HOLE__00020000 != PF_MEMALLOC_NORECLAIM
#define PF_MEMALLOC_NOWARN	PF__HOLE__00020000
#else
#error "PF_MEMALLOC_NOWARN: no second of sched.h's PF__HOLE__* we know of is free - find one that is"
#endif

#endif /* PF_MEMALLOC_NORECLAIM */

#endif /* _BCACHEFS_OS_LINUX_KERNEL_PF_MEMALLOC_H */
