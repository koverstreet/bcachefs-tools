// SPDX-License-Identifier: GPL-2.0

/*
 * What's left of dirents in C: emitting an entry to readdir's dir_context,
 * which is VFS glue - the getdents64 fast path writes to userspace with the
 * unsafe_put_user() goto-label macros, through a private fs/readdir.c struct
 * whose layout is checked at build time, and finds filldir64 with a kprobe.
 * The rest is fs/dirent.rs.
 */

#include "bcachefs.h"

#include "btree/bkey_buf.h"
#include "btree/iter.h"

#include "fs/dirent.h"

#include <linux/dcache.h>

static noinline int bch2_dir_emit_slow(struct btree_trans *trans,
				       struct dir_context *ctx,
				       struct bkey_s_c_dirent d, subvol_inum target)
{
	struct bkey_buf sk __cleanup(bch2_bkey_buf_exit);
	bch2_bkey_buf_init(&sk);

	bch2_bkey_buf_reassemble(&sk, d.s_c);
	d = bkey_i_to_s_c_dirent(sk.k);

	struct qstr name = bch2_dirent_get_name(d);

	/*
	 * dir_emit() copies to userspace and can fault, so it can't run with
	 * btree locks held — drop them, then relock for the next iteration.
	 */
	bch2_trans_unlock(trans);
	int ret = dir_emit(ctx, name.name, name.len, target.inum, vfs_d_type(d.v->d_type));
	if (!ret)
		return 1;
	ctx->pos = d.k->p.offset + 1;
	/*
	 * Don't relock here: a restart return would cause for_each_btree_key_*
	 * to retry the current key without advancing the iter, which re-emits
	 * the just-emitted dirent. Let the next peek relock transparently.
	 */
	return 0;
}

#ifdef __KERNEL__
#include <linux/fs.h>
#include <linux/pagemap.h>

#include "bch2_getdents_layout.h"

struct linux_dirent64 {
	u64		d_ino;
	s64		d_off;
	unsigned short	d_reclen;
	unsigned char	d_type;
	char		d_name[];
};

struct getdents_callback64 {
	struct dir_context ctx;
	struct linux_dirent64 __user * current_dir;
	int prev_reclen;
	int error;
};

/*
 * The struct above is a copy of a private struct in fs/readdir.c: external
 * builds verify it against the target kernel's actual layout, extracted
 * from vmlinux at build time (fs/scripts/getdents-layout.sh). If the
 * layout couldn't be extracted, bch2_dirent_init() leaves the fastpath
 * disabled instead of gambling.
 */
#ifdef BCH_GETDENTS_LAYOUT_VERIFIED
static_assert(sizeof(struct getdents_callback64)		== BCH_GETDENTS_SIZE);
static_assert(offsetof(struct getdents_callback64, ctx)		== BCH_GETDENTS_OFF_ctx);
static_assert(offsetof(struct getdents_callback64, current_dir)	== BCH_GETDENTS_OFF_current_dir);
static_assert(offsetof(struct getdents_callback64, prev_reclen)	== BCH_GETDENTS_OFF_prev_reclen);
static_assert(offsetof(struct getdents_callback64, error)	== BCH_GETDENTS_OFF_error);
#endif

static __always_inline bool bch2_filldir64(struct dir_context *ctx, const char *name, int namlen,
					   u64 ino, unsigned int d_type)
{
#define dirent_size(dirent, len) offsetof(typeof(*(dirent)), d_name[len])

#define unsafe_copy_dirent_name(_dst, _src, _len, label) do {	\
        char __user *dst = (_dst);				\
        const char *src = (_src);				\
        size_t len = (_len);					\
        unsafe_put_user(0, dst+len, label);			\
        unsafe_copy_to_user(dst, src, len, label);		\
} while (0)

	struct linux_dirent64 __user *dirent, *prev;
	struct getdents_callback64 *buf =
		container_of(ctx, struct getdents_callback64, ctx);
	int reclen = ALIGN(dirent_size(dirent, namlen + 1), sizeof(u64));

	/*
	 * Unlike fs/readdir.c's filldir64, no verify_dirent_name() here -
	 * intentional: names handed to dir_emit come from dirents that
	 * passed bch2_dirent_validate() (nonzero length, no '/', no NUL),
	 * so the check would be redundant with our on-disk validation.
	 */

	buf->error = -EINVAL;	/* only used if we fail.. */
	if (reclen > ctx->count)
		return false;
	int prev_reclen = buf->prev_reclen;
	if (prev_reclen && signal_pending(current))
		return false;
	dirent = buf->current_dir;
	prev = (void __user *)dirent - prev_reclen;
	if (!user_write_access_begin(prev, reclen + prev_reclen))
		goto efault;

	/* This might be 'dirent->d_off', but if so it will get overwritten */
	unsafe_put_user(ctx->pos, &prev->d_off, efault_end);
	unsafe_put_user(ino, &dirent->d_ino, efault_end);
	unsafe_put_user(reclen, &dirent->d_reclen, efault_end);
	unsafe_put_user(d_type, &dirent->d_type, efault_end);
	unsafe_copy_dirent_name(dirent->d_name, name, namlen, efault_end);
	user_write_access_end();

	buf->prev_reclen = reclen;
	buf->current_dir = (void __user *)dirent + reclen;
	ctx->count -= reclen;
	return true;

efault_end:
	user_write_access_end();
efault:
	buf->error = -EFAULT;
	return false;
}

/*
 * filldir64 — the actor used by getdents64(2) — writes to userspace via
 * unsafe_put_user() inside a user_write_access_begin()/_end() block, which
 * returns -EFAULT cleanly under pagefault_disable() instead of entering the
 * fault handler. That lets us emit dirents while still holding btree_trans
 * locks in the common case where the user buffer is already faulted in,
 * avoiding an unlock/relock round-trip per dirent.
 *
 * filldir64 is static in fs/readdir.c, so we look up its address at module
 * init via the kprobe-based kallsyms trick. If that fails (lockdown,
 * !CONFIG_KALLSYMS_ALL, symbol renamed), the pointer stays NULL and we fall
 * back to the unconditional-unlock path silently.
 */
static filldir_t filldir64_sym __read_mostly;

void bch2_readdir_fault_in(struct dir_context *ctx)
{
	/*
	 * If this is getdents64, fault in the user buffer up front so the
	 * lock-holding fast path in bch2_dir_emit() doesn't fault - and fall
	 * back to the unlock/relock slow path - the first time it writes into
	 * each page. Best-effort: any residual is still handled by the fallback
	 * in bch2_dir_emit().
	 */
	if (ctx->actor == filldir64_sym) {
		struct getdents_callback64 *buf =
			container_of(ctx, struct getdents_callback64, ctx);
		fault_in_writeable((char __user *) buf->current_dir, ctx->count);
	}
}

int bch2_dir_emit(struct btree_trans *trans, struct dir_context *ctx,
		  struct bkey_s_c_dirent d, subvol_inum target)
{
	struct qstr name = bch2_dirent_get_name(d);
	/*
	 * Although not required by the kernel code, updating ctx->pos is needed
	 * for the bcachefs FUSE driver. Without this update, the FUSE
	 * implementation will be stuck in an infinite loop when reading
	 * directories (via the bcachefs_fuse_readdir callback).
	 * In kernel space, ctx->pos is updated by the VFS code.
	 */
	ctx->pos = d.k->p.offset;

	if (ctx->actor == filldir64_sym) {
		pagefault_disable();
		bool ret = bch2_filldir64(ctx, name.name, name.len,
					  target.inum, vfs_d_type(d.v->d_type));
		pagefault_enable();
		if (likely(ret)) {
			ctx->pos = d.k->p.offset + 1;
			return 0;
		}
		/*
		 * Either the user page wasn't present or the buffer is full.
		 * Drop trans locks (so a real fault can recurse into the fs)
		 * and retry. If it was just "buffer full", this returns false
		 * again and we stop iteration — same outcome as the slow path.
		 */
	}

	return bch2_dir_emit_slow(trans, ctx, d, target);
}
#else
void bch2_readdir_fault_in(struct dir_context *ctx)
{
}

int bch2_dir_emit(struct btree_trans *trans, struct dir_context *ctx,
		  struct bkey_s_c_dirent d, subvol_inum target)
{
	/*
	 * The FUSE readdir actor computes each entry's resume-after cookie as
	 * its pos argument + 1: only correct if ctx->pos is the entry's own
	 * offset when it's emitted, same as the kernel path above.
	 */
	ctx->pos = d.k->p.offset;

	return bch2_dir_emit_slow(trans, ctx, d, target);
}
#endif

#ifdef __KERNEL__
#include <linux/kprobes.h>
#include <linux/uaccess.h>
#ifdef CONFIG_X86_KERNEL_IBT
#include <asm/ibt.h>
#endif

void bch2_dirent_init(void)
{
#ifdef BCH_GETDENTS_LAYOUT_UNVERIFIED
	/*
	 * Couldn't extract getdents_callback64's layout from the target
	 * kernel at build time (no vmlinux/pahole): leave filldir64_sym
	 * NULL, the fastpath never engages, readdir uses the unlock path.
	 */
	pr_info("bcachefs: filldir64 fastpath disabled: struct layout unverified for this kernel\n");
	return;
#endif
	struct kprobe kp = { .symbol_name = "filldir64" };

	int ret = register_kprobe(&kp);
	if (!ret) {
		unsigned long addr = (unsigned long)kp.addr;

#ifdef CONFIG_X86_KERNEL_IBT
		/*
		 * On x86 with IBT, arch_adjust_kprobe_addr() snaps the
		 * probe address forward past the endbr64 prefix; ctx->actor
		 * still points at the symbol entry (i.e. the endbr64), so
		 * step kp.addr back to match.
		 */
		if (__is_endbr(*(u32 *)(addr - 4)))
			addr -= 4;
#endif

		filldir64_sym = (filldir_t)addr;
		unregister_kprobe(&kp);
	}
}

void bch2_filldir64_specialization_to_text(struct printbuf *out)
{
	prt_printf(out, "filldir64 fast path:\t");
	/*
	 * Real address (not hashed) so it can be compared against the
	 * filldir64 symbol in /proc/kallsyms: a mismatch means the kprobe
	 * resolved a different entry than ctx->actor points at (e.g. an
	 * IBT/CFI prologue adjustment that's off by a few bytes); a NULL
	 * means register_kprobe() failed (lockdown, !CONFIG_KPROBES,
	 * blacklist).
	 */
	if (!IS_ERR_OR_NULL(filldir64_sym))
		prt_printf(out, "%px", filldir64_sym);
	else if (IS_ERR(filldir64_sym))
		prt_printf(out, "%s", bch2_err_str(PTR_ERR(filldir64_sym)));
	else
		prt_str(out, "unavailable");
	prt_newline(out);

}
#else
void bch2_dirent_init(void)
{
}

void bch2_filldir64_specialization_to_text(struct printbuf *out)
{
	prt_str(out, "filldir64 fast path:\tunavailable (userspace)\n");
}
#endif
