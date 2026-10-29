// SPDX-License-Identifier: GPL-2.0

#include <linux/uaccess.h>
#include <linux/version.h>

__rust_helper unsigned long
rust_helper_copy_from_user(void *to, const void __user *from, unsigned long n)
{
	return copy_from_user(to, from, n);
}

__rust_helper unsigned long
rust_helper_copy_to_user(void __user *to, const void *from, unsigned long n)
{
	return copy_to_user(to, from, n);
}

/*
 * An arch that inlines copy_{from,to}_user() has no out-of-line
 * _copy_{from,to}_user() for Rust to call. It says so with INLINE_COPY_USER
 * - before 7.2, INLINE_COPY_FROM_USER and INLINE_COPY_TO_USER, which this
 * build has to take too: it builds against kernels back to 6.16.
 */
#if defined(INLINE_COPY_USER) || defined(INLINE_COPY_FROM_USER)
__rust_helper
unsigned long rust_helper__copy_from_user(void *to, const void __user *from, unsigned long n)
{
	return _inline_copy_from_user(to, from, n);
}
#endif

#if defined(INLINE_COPY_USER) || defined(INLINE_COPY_TO_USER)
__rust_helper
unsigned long rust_helper__copy_to_user(void __user *to, const void *from, unsigned long n)
{
	return _inline_copy_to_user(to, from, n);
}
#endif

/*
 * Before 6.19 the header declares _copy_{from,to}_user() even on an arch that
 * inlines them, and the kernel defines and exports them there only with
 * CONFIG_RUST - so a pre-6.19 kernel without Rust, which is what this stack
 * is for, has the declaration and no definition. The bindings take the
 * declared function over the helpers above, so supply it, as lib/usercopy.c
 * would.
 */
#if LINUX_VERSION_CODE < KERNEL_VERSION(6, 19, 0) && !defined(CONFIG_RUST)
#ifdef INLINE_COPY_FROM_USER
unsigned long _copy_from_user(void *to, const void __user *from, unsigned long n)
{
	return _inline_copy_from_user(to, from, n);
}
#endif

#ifdef INLINE_COPY_TO_USER
unsigned long _copy_to_user(void __user *to, const void *from, unsigned long n)
{
	return _inline_copy_to_user(to, from, n);
}
#endif
#endif
