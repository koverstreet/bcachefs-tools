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

/*
 * Compute the checksum of an on-disk superblock, using the csum type
 * stored in the sb itself.  Wraps the csum_vstruct() macro.
 */
struct bch_csum rust_csum_vstruct_sb(struct bch_sb *sb);

/*
 * Wrapper around copy_fs() for format --source: opens src_path,
 * creates a zeroed copy_fs_state, and copies the directory tree.
 */
int rust_fmt_build_fs(struct bch_fs *c, const char *src_path);

/*
 * Check if filesystem is ready for strip-alloc:
 *   returns 0 if clean and capacity <= 1TB
 *   returns 1 if not clean (caller should run recovery and reopen)
 *   returns -ERANGE if capacity too large
 */
int rust_strip_alloc_check(struct bch_fs *c);

/*
 * Strip alloc info from a clean filesystem: removes alloc btree roots
 * from the clean section, replicas, and journal fields.
 */
void strip_fs_alloc(struct bch_fs *c);

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
 * Dump sanitize shims — wraps crypto operations for encrypted fs dumps.
 */
struct jset;
struct bset;

int rust_jset_decrypt(struct bch_fs *c, struct jset *j);
int rust_bset_decrypt(struct bch_fs *c, struct bset *i, unsigned offset);

/*
 * Device reference shims — wraps static inline bch2_dev_tryget_noerror()
 * and bch2_dev_put() for Rust.
 */
struct bch_dev *rust_dev_tryget_noerror(struct bch_fs *c, unsigned dev);
void rust_dev_put(struct bch_dev *ca);

/*
 * Superblock display with device names — scans for devices by UUID,
 * prints member info with device model and name.
 */
struct printbuf;
void bch2_sb_to_text_with_names(struct printbuf *out,
				struct bch_fs *c, struct bch_sb *sb,
				bool print_layout, unsigned fields, int field_only);

#endif /* _RUST_SHIMS_H */
