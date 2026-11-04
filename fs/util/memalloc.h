/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_UTIL_MEMALLOC_H
#define _BCACHEFS_UTIL_MEMALLOC_H

#include <linux/sched.h>

/*
 * Dynamically select two free PF__HOLE__* bits from sched.h for
 * PF_MEMALLOC_NORECLAIM and PF_MEMALLOC_NOWARN.
 *
 * In older kernels (<= 6.12), 0x00800000 and 0x01000000 are free holes.
 * In 6.13+, 0x00800000 is taken by PF_NO_NOTIFY_SIGNAL, leaving 0x01000000,
 * 0x02000000, and 0x40000000.
 * In 7.4+, PF_KCOMPACTD (0x00010000) and PF_KSWAPD (0x00020000) are also holes.
 *
 * Pick the first two distinct available holes in order of preference.
 */

#if !defined(PF_MEMALLOC_NORECLAIM)
#if defined(PF__HOLE__00800000)
#define PF_MEMALLOC_NORECLAIM		PF__HOLE__00800000
#elif defined(PF__HOLE__01000000)
#define PF_MEMALLOC_NORECLAIM		PF__HOLE__01000000
#elif defined(PF__HOLE__02000000)
#define PF_MEMALLOC_NORECLAIM		PF__HOLE__02000000
#elif defined(PF__HOLE__40000000)
#define PF_MEMALLOC_NORECLAIM		PF__HOLE__40000000
#elif defined(PF__HOLE__00010000)
#define PF_MEMALLOC_NORECLAIM		PF__HOLE__00010000
#elif defined(PF__HOLE__00020000)
#define PF_MEMALLOC_NORECLAIM		PF__HOLE__00020000
#else
#error "PF_MEMALLOC_NORECLAIM: no free PF__HOLE__* bits found in sched.h"
#endif
#endif

#if !defined(PF_MEMALLOC_NOWARN)
#if defined(PF__HOLE__00800000) && (PF_MEMALLOC_NORECLAIM != PF__HOLE__00800000)
#define PF_MEMALLOC_NOWARN		PF__HOLE__00800000
#elif defined(PF__HOLE__01000000) && (PF_MEMALLOC_NORECLAIM != PF__HOLE__01000000)
#define PF_MEMALLOC_NOWARN		PF__HOLE__01000000
#elif defined(PF__HOLE__02000000) && (PF_MEMALLOC_NORECLAIM != PF__HOLE__02000000)
#define PF_MEMALLOC_NOWARN		PF__HOLE__02000000
#elif defined(PF__HOLE__40000000) && (PF_MEMALLOC_NORECLAIM != PF__HOLE__40000000)
#define PF_MEMALLOC_NOWARN		PF__HOLE__40000000
#elif defined(PF__HOLE__00010000) && (PF_MEMALLOC_NORECLAIM != PF__HOLE__00010000)
#define PF_MEMALLOC_NOWARN		PF__HOLE__00010000
#elif defined(PF__HOLE__00020000) && (PF_MEMALLOC_NORECLAIM != PF__HOLE__00020000)
#define PF_MEMALLOC_NOWARN		PF__HOLE__00020000
#else
#error "PF_MEMALLOC_NOWARN: not enough free PF__HOLE__* bits found in sched.h"
#endif
#endif

#endif /* _BCACHEFS_UTIL_MEMALLOC_H */
