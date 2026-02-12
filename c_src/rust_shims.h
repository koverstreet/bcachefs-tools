/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _RUST_SHIMS_H
#define _RUST_SHIMS_H

/*
 * C wrapper functions for Rust code that needs to call static inline
 * functions or functions whose types don't work well with bindgen.
 */

struct bch_fs;
struct bch_sb;
struct bch_csum;
struct bch_member;

/* LE64_BITMASK setter shims — wraps static inline SET_* macros */
void rust_set_bch_sb_version_incompat_allowed(struct bch_sb *, __u64);
void rust_set_bch_sb_meta_replicas_req(struct bch_sb *, __u64);
void rust_set_bch_sb_data_replicas_req(struct bch_sb *, __u64);
void rust_set_bch_sb_extent_bp_shift(struct bch_sb *, __u64);
void rust_set_bch_sb_foreground_target(struct bch_sb *, __u64);
void rust_set_bch_sb_background_target(struct bch_sb *, __u64);
void rust_set_bch_sb_promote_target(struct bch_sb *, __u64);
void rust_set_bch_sb_metadata_target(struct bch_sb *, __u64);
void rust_set_bch_sb_encryption_type(struct bch_sb *, __u64);
void rust_set_bch_member_rotational_set(struct bch_member *, __u64);
void rust_set_bch_member_group(struct bch_member *, __u64);
__u64 rust_bch_sb_features_all(void);

/*
 * Compute the checksum of an on-disk superblock, using the csum type
 * stored in the sb itself.  Wraps the csum_vstruct() macro.
 */
struct bch_csum rust_csum_vstruct_sb(struct bch_sb *sb);

/*
 * Size of struct bucket — used by pick_bucket_size to estimate fsck
 * memory requirements. Shim needed because the struct has bitfields.
 */
size_t rust_sizeof_bucket(void);

/*
 * Compute the total byte size of a variable-length superblock struct.
 * Wraps the vstruct_bytes() macro.
 */
size_t rust_vstruct_bytes_sb(const struct bch_sb *sb);

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

/*
 * Collect all non-NULL journal_replay entries from c->journal_entries
 * (genradix) into a flat array. Caller must free entries.
 */
struct journal_replay;

struct rust_journal_entries {
	struct journal_replay	**entries;
	size_t			nr;
};

struct rust_journal_entries rust_collect_journal_entries(struct bch_fs *c);

/*
 * Btree node introspection shims — wraps x-macro generated static
 * inline accessors and static inline bch2_btree_id_root().
 */
struct btree;
bool rust_btree_node_fake(struct btree *b);
struct btree *rust_btree_id_root_b(struct bch_fs *c, unsigned id);
unsigned rust_btree_id_nr_alive(struct bch_fs *c);

/*
 * Online member iteration shim — wraps the static inline
 * bch2_get_next_online_dev() which handles ref counting internally.
 * rust_put_online_dev_ref() is for cleanup on early loop termination.
 */
struct bch_dev;
struct bch_dev *rust_get_next_online_dev(struct bch_fs *c,
					 struct bch_dev *ca,
					 unsigned ref_idx);
void rust_put_online_dev_ref(struct bch_dev *ca, unsigned ref_idx);

/*
 * Sanitize journal/btree data in a buffer for safe sharing.
 * Zeroes inline data extents and optionally filenames; handles
 * decryption, checksum clearing, and vstruct iteration internally.
 */
void rust_sanitize_journal(struct bch_fs *c, void *buf, size_t len,
			   bool sanitize_filenames);
void rust_sanitize_btree(struct bch_fs *c, void *buf, size_t len,
			 bool sanitize_filenames);

#endif /* _RUST_SHIMS_H */
