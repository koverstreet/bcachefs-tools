#ifndef _LINUX_SORT_H
#define _LINUX_SORT_H

#include <stdlib.h>
#include <linux/kernel.h>

/**
 * cmp_int - perform a three-way comparison of the arguments
 * @l: the left argument
 * @r: the right argument
 *
 * Return: 1 if the left argument is greater than the right one; 0 if the
 * arguments are equal; -1 if the left argument is less than the right one.
 */
#define cmp_int(l, r) (((l) > (r)) - ((l) < (r)))

static inline void sort(void *base, size_t num, size_t size,
			int (*cmp_func)(const void *, const void *),
			void (*swap_func)(void *, void *, int size))
{
	return qsort(base, num, size, cmp_func);
}

#define sort_nonatomic(...)	sort(__VA_ARGS__)

#endif
