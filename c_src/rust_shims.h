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
 * Bitmap shim — set_bit() is atomic (locked bitops in the kernel),
 * can't be inlined through bindgen.
 */
void rust_set_bit(unsigned long nr, unsigned long *addr);

/*
 * Device reference shims — wraps static inline bch2_dev_tryget_noerror()
 * and bch2_dev_put() for Rust.
 */
struct bch_dev *rust_dev_tryget_noerror(struct bch_fs *c, unsigned dev);
void rust_dev_put(struct bch_dev *ca);

/*
 * Data IO shims — wraps static inlines not available through bindgen.
 * Data must be block-aligned and <= 1MB.
 */
#define RUST_IO_MAX	(1 << 20)

/*
 * Submit a write — Rust handles completion via op->end_io.
 * Caller must heap-allocate op and bvecs (they must outlive the IO).
 * Sets BCH_WRITE_sync so completion is inline for now; Rust can drop
 * the flag later to go fully async.
 * Returns 0 on successful submit, or -errno from disk reservation.
 */
int rust_write_submit(struct bch_fs *c,
		      struct bch_write_op *op,
		      struct bio_vec *bvecs, unsigned nr_bvecs,
		      const void *buf, size_t len,
		      __u64 inum, __u64 offset,
		      __u32 subvol, __u32 replicas,
		      __u64 new_i_size,
		      void (*end_io)(struct bch_write_op *));

/*
 * Submit a read without waiting — Rust handles completion via endio.
 * Caller must heap-allocate rbio and bvecs (they must outlive the IO).
 */
void rust_read_submit(struct bch_fs *c,
		      struct bch_read_bio *rbio,
		      struct bio_vec *bvecs, unsigned nr_bvecs,
		      void *buf, size_t len,
		      __u64 offset,
		      struct bch_inode_opts opts,
		      subvol_inum inum,
		      bio_end_io_t endio);

/*
 * Extent construction for migrate — wraps bkey_extent_init,
 * bch2_bkey_append_ptr, bucket_gen, bch2_disk_reservation_get/put,
 * bch2_btree_insert. All static inlines or macro-generated,
 * not available through bindgen.
 */
int rust_link_data(struct bch_fs *c,
		   __u64 dst_inum, __s64 *sectors_delta,
		   __u64 logical, __u64 physical, __u64 length);

/*
 * Accounting read shim — wraps the static inline bch2_accounting_mem_read
 * which uses percpu_read guard + eytzinger search.
 */
struct bpos;
void rust_accounting_mem_read(struct bch_fs *c, struct bpos p,
			      __u64 *v, unsigned nr);

#endif /* _RUST_SHIMS_H */
