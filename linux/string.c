/*
 *  linux/lib/string.c
 *
 *  Copyright (C) 1991, 1992  Linus Torvalds
 */

#include <string.h>

#include <linux/compiler.h>
#include <linux/string.h>

void memzero_explicit(void *s, size_t count)
{
	memset(s, 0, count);
	barrier_data(s);
}
