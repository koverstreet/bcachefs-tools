/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_CODEGEN_H
#define _BCACHEFS_CODEGEN_H

/*
 * Kernel APIs the Rust calls that bindgen can't bind - macros, or macros in
 * some kernels - as static inlines, which codegen.rs binds and wraps
 * (extern.c). Kernel headers only, as codegen binds nothing of bcachefs's.
 */

#include <linux/blkdev.h>
#include <linux/compat.h>
#include <linux/fs.h>
#include <linux/kallsyms.h>
#include <linux/key.h>
#include <linux/lz4.h>
#include <linux/mount.h>
#include <linux/namei.h>
#include <linux/mm.h>
#include <linux/notifier.h>
#include <linux/pagemap.h>
#include <linux/poll.h>
#include <linux/vmalloc.h>
#include <asm/ioctl.h>
#include <keys/user-type.h>

#ifdef __BINDGEN__
/*
 * Macros with casts, which bindgen doesn't evaluate: as consts, which it
 * binds - the kernel's bindings_helper.h has its own the same way, and
 * codegen.rs strips the prefix as rust/Makefile does.
 */
const blk_mode_t RUST_CONST_HELPER_BLK_OPEN_READ	= BLK_OPEN_READ;
const blk_mode_t RUST_CONST_HELPER_BLK_OPEN_WRITE	= BLK_OPEN_WRITE;
const blk_mode_t RUST_CONST_HELPER_BLK_OPEN_EXCL	= BLK_OPEN_EXCL;
const blk_opf_t RUST_CONST_HELPER_REQ_SYNC		= REQ_SYNC;
const blk_opf_t RUST_CONST_HELPER_REQ_META		= REQ_META;
const blk_opf_t RUST_CONST_HELPER_REQ_IDLE		= REQ_IDLE;
const blk_opf_t RUST_CONST_HELPER_REQ_FUA		= REQ_FUA;

/*
 * ioctl numbers' layout, the arch's own: powerpc, mips, sparc, alpha and
 * parisc override asm-generic's (util/ioctl.rs)
 */
const unsigned int RUST_CONST_HELPER_MINORBITS		= MINORBITS;

/* a notifier's answer, as reconcile's power supply notifier gives it */
const int RUST_CONST_HELPER_NOTIFY_OK			= NOTIFY_OK;

/* room for one symbol as %pB prints it, util/backtrace.rs's frames */
const size_t RUST_CONST_HELPER_KSYM_SYMBOL_LEN		= KSYM_SYMBOL_LEN;

/* how a directory was opened: the cookies vfs/dir.rs's readdir hands out */
const fmode_t RUST_CONST_HELPER_FMODE_32BITHASH		= FMODE_32BITHASH;
const fmode_t RUST_CONST_HELPER_FMODE_64BITHASH		= FMODE_64BITHASH;

/* the VFS's own ioctl numbers, which vfs/ioctl.rs answers */
const unsigned int RUST_CONST_HELPER_FS_IOC_GETVERSION	= FS_IOC_GETVERSION;
const unsigned int RUST_CONST_HELPER_FS_IOC_GETFSLABEL	= FS_IOC_GETFSLABEL;
const unsigned int RUST_CONST_HELPER_FS_IOC_SETFSLABEL	= FS_IOC_SETFSLABEL;
const unsigned int RUST_CONST_HELPER_FS_IOC_SETVERSION	= FS_IOC_SETVERSION;
const unsigned int RUST_CONST_HELPER_FS_IOC_GETFLAGS	= FS_IOC_GETFLAGS;
const unsigned int RUST_CONST_HELPER_FS_IOC_SETFLAGS	= FS_IOC_SETFLAGS;
const unsigned int RUST_CONST_HELPER_FS_IOC32_GETFLAGS	= FS_IOC32_GETFLAGS;
const unsigned int RUST_CONST_HELPER_FS_IOC32_SETFLAGS	= FS_IOC32_SETFLAGS;
const unsigned int RUST_CONST_HELPER_FS_IOC32_GETVERSION	= FS_IOC32_GETVERSION;

/* a path lookup's flag, and a superblock's: vfs/ioctl.rs's subvolume create */
const unsigned int RUST_CONST_HELPER_LOOKUP_FOLLOW	= LOOKUP_FOLLOW;
const unsigned long RUST_CONST_HELPER_SB_POSIXACL	= SB_POSIXACL;
const unsigned int RUST_CONST_HELPER__IOC_NONE		= _IOC_NONE;
const unsigned int RUST_CONST_HELPER__IOC_READ		= _IOC_READ;
const unsigned int RUST_CONST_HELPER__IOC_WRITE		= _IOC_WRITE;
const unsigned int RUST_CONST_HELPER__IOC_NRSHIFT	= _IOC_NRSHIFT;
const unsigned int RUST_CONST_HELPER__IOC_TYPESHIFT	= _IOC_TYPESHIFT;
const unsigned int RUST_CONST_HELPER__IOC_SIZESHIFT	= _IOC_SIZESHIFT;
const unsigned int RUST_CONST_HELPER__IOC_DIRSHIFT	= _IOC_DIRSHIFT;
const unsigned int RUST_CONST_HELPER__IOC_NRMASK	= _IOC_NRMASK;
const unsigned int RUST_CONST_HELPER__IOC_TYPEMASK	= _IOC_TYPEMASK;
const unsigned int RUST_CONST_HELPER__IOC_SIZEMASK	= _IOC_SIZEMASK;

const __poll_t RUST_CONST_HELPER_EPOLLIN		= EPOLLIN;
const __poll_t RUST_CONST_HELPER_EPOLLOUT		= EPOLLOUT;
const __poll_t RUST_CONST_HELPER_EPOLLERR		= EPOLLERR;
const __poll_t RUST_CONST_HELPER_EPOLLHUP		= EPOLLHUP;

/* lz4's compression workspace: a sizeof() */
const size_t RUST_CONST_HELPER_LZ4_MEM_COMPRESS		= LZ4_MEM_COMPRESS;
#endif

static inline struct folio *rust_page_folio(struct page *page)
{
	return page_folio(page);
}

/* a macro before 6.18 */
static inline unsigned long rust_folio_page_idx(struct folio *folio, struct page *page)
{
	return folio_page_idx(folio, page);
}

/* a static inline */
static inline bool rust_refcount_inc_not_zero(refcount_t *r)
{
	return refcount_inc_not_zero(r);
}

/* a static inline */
static inline void rust_poll_wait(struct file *file, wait_queue_head_t *q, poll_table *pt)
{
	poll_wait(file, q, pt);
}

/* not in the vendored crate's bindings: pagemap.h isn't */
static inline size_t rust_fault_in_writeable(char __user *uaddr, size_t size)
{
	return fault_in_writeable(uaddr, size);
}

static inline size_t rust_fault_in_readable(const char __user *uaddr, size_t size)
{
	return fault_in_readable(uaddr, size);
}

static inline int rust_filemap_write_and_wait_range(struct address_space *mapping,
						    loff_t start, loff_t end)
{
	return filemap_write_and_wait_range(mapping, start, end);
}

#ifdef CONFIG_COMPAT
/*
 * compat_ptr()'s compat_uptr_t is the kernel crate's only with CONFIG_COMPAT:
 * a binding of it would name a type there isn't
 */
static inline void __user *rust_compat_ptr(u32 uptr)
{
	return compat_ptr(uptr);
}
#endif

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

/*
 * A bio's pages mapped as one buffer. PAGE_KERNEL isn't a constant - on x86
 * it has the memory encryption mask - and vmalloc.h stays blocklisted: it
 * would bind a second struct vm_struct.
 */
static inline void *rust_vmap(struct page **pages, unsigned int count)
{
	return vmap(pages, count, VM_MAP, PAGE_KERNEL);
}

static inline void rust_vunmap(const void *addr)
{
	vunmap(addr);
}

/* request_key() is a static inline, of key_type_user, a var user-type.h declares */
static inline struct key *rust_request_user_key(const char *description)
{
	return request_key(&key_type_user, description, NULL);
}

/* dereference_key_locked() is a macro: a user key's payload, its sem held */
static inline const void *rust_user_key_data(struct key *key, unsigned short *len)
{
	const struct user_key_payload *ukp = dereference_key_locked(key);

	*len = ukp->datalen;
	return ukp->data;
}

#endif /* _BCACHEFS_CODEGEN_H */
