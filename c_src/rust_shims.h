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

/*
 * Check if filesystem is ready for strip-alloc:
 *   returns 0 if clean and capacity <= 1TB
 *   returns 1 if not clean (caller should run recovery and reopen)
 *   returns -ERANGE if capacity too large
 */
int rust_strip_alloc_check(struct bch_fs *c);

/*
 * Strip alloc info: takes sb_lock, calls strip_fs_alloc(),
 * writes superblock, releases lock.
 */
void rust_strip_alloc_do(struct bch_fs *c);

/*
 * Set member state on an offline device: takes sb_lock, modifies
 * the member state via SET_BCH_MEMBER_STATE, writes superblock.
 */
void rust_device_set_state_offline(struct bch_fs *c,
				   unsigned dev_idx, unsigned new_state);

/*
 * Offline device resize: finds the single online device, resizes it.
 * Returns -EINVAL if multiple devices online, -ENOSPC for shrink,
 * or error from bch2_dev_resize.  size is in 512-byte sectors.
 */
int rust_device_resize_offline(struct bch_fs *c, __u64 size);

/*
 * Offline journal resize: finds the single online device, sets
 * the number of journal buckets.  size is in 512-byte sectors.
 */
int rust_device_resize_journal_offline(struct bch_fs *c, __u64 size);

#endif /* _RUST_SHIMS_H */
