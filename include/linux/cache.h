#ifndef __TOOLS_LINUX_CACHE_H
#define __TOOLS_LINUX_CACHE_H

#define L1_CACHE_SHIFT		6
#define L1_CACHE_BYTES		(1 << L1_CACHE_SHIFT)
#define SMP_CACHE_BYTES		L1_CACHE_BYTES

#define L1_CACHE_ALIGN(x)	__ALIGN_KERNEL(x, L1_CACHE_BYTES)

#define __read_mostly
#define __ro_after_init

#define ____cacheline_aligned	__attribute__((__aligned__(SMP_CACHE_BYTES)))
#define ____cacheline_aligned_in_smp ____cacheline_aligned

#endif /* __TOOLS_LINUX_CACHE_H */

