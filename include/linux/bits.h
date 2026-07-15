/* SPDX-License-Identifier: GPL-2.0 */
#ifndef __LINUX_BITS_H
#define __LINUX_BITS_H

//#include <vdso/bits.h>
//#include <uapi/linux/bits.h>

#define BIT_WORD(nr)		((nr) / BITS_PER_LONG)
#define BIT_ULL_MASK(nr)	(ULL(1) << ((nr) % BITS_PER_LONG_LONG))
#define BIT_ULL_WORD(nr)	((nr) / BITS_PER_LONG_LONG)
#define BITS_PER_BYTE		8
#define BITS_PER_TYPE(type)	(sizeof(type) * BITS_PER_BYTE)
#define __GENMASK(h, l) (((~_UL(0)) << (l)) & (~_UL(0) >> (__BITS_PER_LONG - 1 - (h))))

/*
 * Create a contiguous bitmask starting at bit position @l and ending at
 * position @h. For example
 * GENMASK_ULL(39, 21) gives us the 64bit vector 0x000000ffffe00000.
 */
#if !defined(__ASSEMBLY__)

/*
 * Missing asm support
 *
 * GENMASK_U*() and BIT_U*() depend on BITS_PER_TYPE() which relies on sizeof(),
 * something not available in asm. Nevertheless, fixed width integers is a C
 * concept. Assembly code can rely on the long and long long versions instead.
 */

#include <linux/compiler.h>
#include <linux/overflow.h>

#define GENMASK_INPUT_CHECK(h, l) BUILD_BUG_ON_ZERO(const_true((l) > (h)))

/*
 * Generate a mask for the specified type @t. Additional checks are made to
 * guarantee the value returned fits in that type, relying on
 * -Wshift-count-overflow compiler check to detect incompatible arguments.
 * For example, all these create build errors or warnings:
 *
 * - GENMASK(15, 20): wrong argument order
 * - GENMASK(72, 15): doesn't fit unsigned long
 * - GENMASK_U32(33, 15): doesn't fit in a u32
 */
#define GENMASK_TYPE(t, h, l)					\
	((t)(GENMASK_INPUT_CHECK(h, l) +			\
	     (type_max(t) << (l) &				\
	      type_max(t) >> (BITS_PER_TYPE(t) - 1 - (h)))))

#define GENMASK(h, l)		GENMASK_TYPE(unsigned long, h, l)
#define GENMASK_ULL(h, l)	GENMASK_TYPE(unsigned long long, h, l)

#define GENMASK_U8(h, l)	GENMASK_TYPE(u8, h, l)
#define GENMASK_U16(h, l)	GENMASK_TYPE(u16, h, l)
#define GENMASK_U32(h, l)	GENMASK_TYPE(u32, h, l)
#define GENMASK_U64(h, l)	GENMASK_TYPE(u64, h, l)
#define GENMASK_U128(h, l)	GENMASK_TYPE(u128, h, l)

#else /* defined(__ASSEMBLY__) */

/*
 * BUILD_BUG_ON_ZERO is not available in h files included from asm files,
 * disable the input check if that is the case.
 */
#define GENMASK(h, l)		__GENMASK(h, l)
#define GENMASK_ULL(h, l)	__GENMASK_ULL(h, l)

#endif /* !defined(__ASSEMBLY__) */

#endif	/* __LINUX_BITS_H */
