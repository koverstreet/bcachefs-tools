/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_OPTS_H
#define _BCACHEFS_OPTS_H

#include <linux/bitops.h>
#include <linux/bug.h>
#include <linux/log2.h>
#include <linux/sizes.h>
#include <linux/string.h>
#include <linux/sysfs.h>
#include "bcachefs_format.h"
#include "util/darray.h"

#include "opts_gen.h"

#define opt_defined(_opts, _name)	test_bit(Opt_##_name, (_opts).defined.d)

#define opt_set(_opts, _name, _v)					\
do {									\
	__set_bit(Opt_##_name, (_opts).defined.d);			\
	(_opts)._name = _v;						\
} while (0)

static inline struct bch_opts bch2_opts_empty(void)
{
	return (struct bch_opts) { 0 };
}

#endif /* _BCACHEFS_OPTS_H */
