#ifndef __TOOLS_LINUX_SLAB_H
#define __TOOLS_LINUX_SLAB_H

#include <malloc.h>
#include <stdlib.h>
#include <string.h>

#include <linux/kernel.h>
#include <linux/log2.h>
#include <linux/overflow.h>
#include <linux/page.h>
#include <linux/shrinker.h>
#include <linux/types.h>

#include <sys/mman.h>

#define ARCH_KMALLOC_MINALIGN		16
#define KMALLOC_MAX_SIZE		SIZE_MAX

static inline void *kmalloc(size_t size, gfp_t flags)
{
	unsigned i = 0;
	void *p;

	do {
		run_shrinkers(flags, i != 0);

		if (size) {
			size_t alignment = min(rounddown_pow_of_two(size), (size_t)PAGE_SIZE);
			alignment = max(sizeof(void *), alignment);
			if (posix_memalign(&p, alignment, size))
				p = NULL;
		} else {
			p = malloc(0);
		}
		if (p && (flags & __GFP_ZERO))
			memset(p, 0, size);
	} while (!p && i++ < 10);

	return p;
}

static inline void *krealloc(void *old, size_t size, gfp_t flags)
{
	void *new;

	new = kmalloc(size, flags);
	if (!new)
		return NULL;

	if (flags & __GFP_ZERO)
		memset(new, 0, size);

	if (old) {
		memcpy(new, old,
		       min(malloc_usable_size(old),
			   malloc_usable_size(new)));
		free(old);
	}

	return new;
}

static inline void *krealloc_array(void *p, size_t new_n, size_t new_size, gfp_t flags)
{
	size_t bytes;

	if (unlikely(check_mul_overflow(new_n, new_size, &bytes)))
		return NULL;

	return krealloc(p, bytes, flags);
}

#define kzalloc(size, flags)		kmalloc(size, flags|__GFP_ZERO)
#define kmalloc_array(n, size, flags)					\
	((size) != 0 && (n) > SIZE_MAX / (size)				\
	 ? NULL : kmalloc((n) * (size), flags))

#define kvmalloc_array(n, size, flags)					\
	((size) != 0 && (n) > SIZE_MAX / (size)				\
	 ? NULL : kmalloc((n) * (size), flags))

#define kcalloc(n, size, flags)		kmalloc_array(n, size, flags|__GFP_ZERO)

#define kfree(p)			free(p)
#define kzfree(p)			free(p)

#define kvmalloc(size, flags)		kmalloc(size, flags)
#define kvzalloc(size, flags)		kzalloc(size, flags)
#define kvfree(p)			kfree(p)

static inline struct page *alloc_pages(gfp_t flags, unsigned int order)
{
	size_t size = PAGE_SIZE << order;
	unsigned i = 0;
	void *p;

	do {
		run_shrinkers(flags, i != 0);

		p = aligned_alloc(PAGE_SIZE, size);
		if (p && (flags & __GFP_ZERO))
			memset(p, 0, size);
	} while (!p && i++ < 10);

	return p;
}

#define alloc_page(gfp)			alloc_pages(gfp, 0)

#define __get_free_pages(gfp, order)	((unsigned long) alloc_pages(gfp, order))
#define __get_free_page(gfp)		__get_free_pages(gfp, 0)

#define __free_pages(page, order)			\
do {							\
	(void) order;					\
	free(page);					\
} while (0)

#define free_pages(addr, order)				\
do {							\
	(void) order;					\
	free((void *) (addr));				\
} while (0)

#define __free_page(page) __free_pages((page), 0)
#define free_page(addr) free_pages((addr), 0)

#define VM_IOREMAP		0x00000001	/* ioremap() and friends */
#define VM_ALLOC		0x00000002	/* vmalloc() */
#define VM_MAP			0x00000004	/* vmap()ed pages */
#define VM_USERMAP		0x00000008	/* suitable for remap_vmalloc_range */
#define VM_UNINITIALIZED	0x00000020	/* vm_struct is not fully initialized */
#define VM_NO_GUARD		0x00000040      /* don't add guard page */
#define VM_KASAN		0x00000080      /* has allocated kasan shadow memory */

static inline void vunmap(const void *addr) {}

static inline void *vmap(struct page **pages, unsigned int count,
			 unsigned long flags, unsigned prot)
{
	return NULL;
}

#define is_vmalloc_addr(page)		0

#define vmalloc_to_page(addr)		((struct page *) (addr))

static inline void *kmemdup(const void *src, size_t len, gfp_t gfp)
{
	void *p;

	p = kmalloc(len, gfp);
	if (p)
		memcpy(p, src, len);
	return p;
}

struct kmem_cache {
	size_t		    obj_size;
};

static inline void *kmem_cache_alloc(struct kmem_cache *c, gfp_t gfp)
{
	return kmalloc(c->obj_size, gfp);
}

static inline void kmem_cache_free(struct kmem_cache *c, void *p)
{
	kfree(p);
}

static inline void kmem_cache_destroy(struct kmem_cache *p)
{
	kfree(p);
}

static inline struct kmem_cache *kmem_cache_create(size_t obj_size)
{
	struct kmem_cache *p = kmalloc(sizeof(*p), GFP_KERNEL);
	if (!p)
		return NULL;

	p->obj_size = obj_size;
	return p;
}

#define KMEM_CACHE(_struct, _flags)	kmem_cache_create(sizeof(struct _struct))

#define PAGE_KERNEL		0
#define PAGE_KERNEL_EXEC	1

#define vfree(p)		free(p)

static inline void *__vmalloc(unsigned long size, gfp_t gfp_mask)
{
	unsigned i = 0;
	void *p;

	size = round_up(size, PAGE_SIZE);

	do {
		run_shrinkers(gfp_mask, i != 0);

		p = aligned_alloc(PAGE_SIZE, size);
		if (p && gfp_mask & __GFP_ZERO)
			memset(p, 0, size);
	} while (!p && i++ < 10);

	return p;
}

static inline void *vmalloc_exec(unsigned long size, gfp_t gfp_mask)
{
	void *p;

	p = __vmalloc(size, gfp_mask);
	if (!p)
		return NULL;

	if (mprotect(p, size, PROT_READ|PROT_WRITE|PROT_EXEC)) {
		vfree(p);
		return NULL;
	}

	return p;
}

static inline void *vmalloc(unsigned long size)
{
	return __vmalloc(size, GFP_KERNEL);
}

static inline void *vzalloc(unsigned long size)
{
	return __vmalloc(size, GFP_KERNEL|__GFP_ZERO);
}

#endif /* __TOOLS_LINUX_SLAB_H */
