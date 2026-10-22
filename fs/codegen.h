/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_CODEGEN_H
#define _BCACHEFS_CODEGEN_H

/*
 * Kernel APIs the Rust calls that bindgen can't bind - macros, or macros in
 * some kernels - as static inlines, which codegen.rs binds and wraps
 * (extern.c). Kernel headers only, as codegen binds nothing of bcachefs's.
 */

#include <linux/mm.h>

static inline struct folio *rust_page_folio(struct page *page)
{
	return page_folio(page);
}

/* a macro before 6.18 */
static inline unsigned long rust_folio_page_idx(struct folio *folio, struct page *page)
{
	return folio_page_idx(folio, page);
}

/* a static inline: here rather than all of mm.h unblocklisted for it */
static inline size_t rust_folio_size(struct folio *folio)
{
	return folio_size(folio);
}

static inline bool rust_page_highmem(struct page *page)
{
	return PageHighMem(page);
}

/* a macro, a static inline or exported, by the configuration */
static inline void *rust_page_address(struct page *page)
{
	return page_address(page);
}

#endif /* _BCACHEFS_CODEGEN_H */
