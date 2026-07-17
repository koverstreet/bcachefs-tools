/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_TEST_H
#define _BCACHEFS_TEST_H

struct bch_fs;

/*
 * bch2_btree_perf_test() is implemented in Rust (debug/tests.rs), so it exists
 * only when bcachefs's Rust is actually built — not merely when TESTS is on.
 */
#if defined(CONFIG_BCACHEFS_TESTS) && defined(CONFIG_BCACHEFS_RUST)

int bch2_btree_perf_test(struct bch_fs *, const char *, u64, unsigned);

#else

#endif /* CONFIG_BCACHEFS_TESTS */

#endif /* _BCACHEFS_TEST_H */
