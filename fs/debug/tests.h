/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_TEST_H
#define _BCACHEFS_TEST_H

struct bch_fs;

#ifdef CONFIG_BCACHEFS_TESTS
/* implemented in Rust: debug/tests.rs */
int bch2_btree_perf_test(struct bch_fs *, const char *, u64, unsigned);
#endif

#endif /* _BCACHEFS_TEST_H */
