// SPDX-License-Identifier: GPL-2.0

#include <linux/mm.h>
#include <linux/version.h>
#include <linux/vmalloc.h>

__rust_helper void *__must_check __realloc_size(2)
rust_helper_vrealloc_node_align(const void *p, size_t size, unsigned long align,
				gfp_t flags, int node)
{
#if LINUX_VERSION_CODE >= KERNEL_VERSION(7, 0, 0)
	return vrealloc_node_align(p, size, align, flags, node);
#else
	/*
	 * vrealloc_node_align_noprof gained EXPORT_SYMBOL only in 7.0 (commit
	 * d60769075013); on older kernels the bundled helper can't link it from a
	 * module. bcachefs's Vmalloc, os/linux_kernel/alloc.rs's, calls
	 * __vmalloc_node_noprof() and vfree() there instead, and doesn't come
	 * here - WARN if that ever changes.
	 */
	WARN_ONCE(1, "bcachefs: Vmalloc realloc unsupported on this kernel (vrealloc not exported pre-7.0)\n");
	return NULL;
#endif
}

__rust_helper bool rust_helper_is_vmalloc_addr(const void *x)
{
	return is_vmalloc_addr(x);
}
