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

#endif /* _RUST_SHIMS_H */
