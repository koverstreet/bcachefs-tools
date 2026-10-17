/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_TEST_H
#define _BCACHEFS_TEST_H

#include "debug/tests_gen.h"

struct bch_fs;

#ifdef CONFIG_BCACHEFS_TESTS

int bch2_compress_test(struct bch_fs *, const char *, u64, unsigned);

#endif /* CONFIG_BCACHEFS_TESTS */

#endif /* _BCACHEFS_TEST_H */
