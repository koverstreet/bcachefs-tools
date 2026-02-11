/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _RUST_SHIMS_H
#define _RUST_SHIMS_H

/*
 * C wrapper functions for Rust code that needs to call static inline
 * functions or functions whose types don't work well with bindgen.
 */

struct bch_fs;

/*
 * Wrapper around copy_fs() for format --source: opens src_path,
 * creates a zeroed copy_fs_state, and copies the directory tree.
 */
int rust_fmt_build_fs(struct bch_fs *c, const char *src_path);

/*
 * Capture bch2_opts_usage output as a string, with proper flag
 * filtering: flags_all bits must all be set, flags_none bits must
 * not be set. Caller must free the returned buffer.
 */
char *rust_opts_usage_to_str(unsigned flags_all, unsigned flags_none);

#endif /* _RUST_SHIMS_H */
