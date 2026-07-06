// SPDX-License-Identifier: GPL-2.0

#include <linux/align.h>
#include <linux/slab.h>
#include <linux/version.h>

__rust_helper void *__must_check __realloc_size(2)
rust_helper_krealloc_node_align(const void *objp, size_t new_size, unsigned long align,
				gfp_t flags, int node)
{
#if LINUX_VERSION_CODE >= KERNEL_VERSION(6, 18, 0)
	return krealloc_node_align(objp, new_size, align, flags, node);
#else
	/*
	 * krealloc_node_align() is 6.18+. Pad size to a multiple of align:
	 * with the slab allocators' alignment guarantees that satisfies the
	 * requested alignment - it's what the kernel's own Rust allocator
	 * (Kmalloc::aligned_layout) relied on before this API existed. The
	 * NUMA hint is dropped; krealloc has no node variant here.
	 */
	return krealloc(objp, ALIGN(new_size, align), flags);
#endif
}

__rust_helper void *__must_check __realloc_size(2)
rust_helper_kvrealloc_node_align(const void *p, size_t size, unsigned long align,
				 gfp_t flags, int node)
{
#if LINUX_VERSION_CODE >= KERNEL_VERSION(6, 18, 0)
	return kvrealloc_node_align(p, size, align, flags, node);
#else
	/* As above; the vmalloc fallback is page-aligned regardless. */
	return kvrealloc(p, ALIGN(size, align), flags);
#endif
}
