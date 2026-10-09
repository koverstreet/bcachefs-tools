/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _RUST_SHIMS_H
#define _RUST_SHIMS_H

/*
 * C wrapper functions for Rust code that needs to call static inline
 * functions or functions whose types don't work well with bindgen.
 */

#include <stddef.h>
#include <linux/fs.h>

/*
 * Block device ioctl numbers, for Rust.
 *
 * These encode the direction bits and sizeof() of the argument type, both of
 * which vary by architecture and word size, so they can't be written down -
 * they have to come from the kernel headers for the target.
 *
 * Taking them as bindgen macros meant going through clang_macro_fallback, which
 * compiles a throwaway probe per macro and never checks whether the probe
 * compiled: reparse() looks at CXErrorCode, which says whether the reparse ran
 * rather than whether the code was valid, and nothing reads the diagnostics.
 * Both ways that can go wrong turned up in the field in the same week - one
 * build dropped BLKGETSIZE64 and failed at the use site, another kept it with
 * the size field 1 instead of 8 and shipped a binary that sent the kernel an
 * ioctl it answered with ENOTTY, partway through a device resize.
 *
 * So don't ask bindgen to evaluate these. Evaluate them the way everything else
 * here gets evaluated - by the C compiler, against the real headers - and hand
 * Rust a plain integer. Note that nothing below restates what _IO() and _IOR()
 * mean, which is the other trap: an open-coded 0x127E for BLKROTATIONAL is
 * correct on exactly the architectures whose _IOC layout you had in mind.
 */
static const unsigned long BCH_BLKGETSIZE64	= BLKGETSIZE64;
static const unsigned long BCH_BLKPBSZGET	= BLKPBSZGET;
static const unsigned long BCH_BLKROTATIONAL	= BLKROTATIONAL;

/*
 * FS_IOC_GETFSSYSFSPATH is a generic VFS ioctl, so it's in neither the block
 * list above nor bcachefs's own (bcachefs_ioctl.h) - and it was open-coded in Rust
 * as (2 << 30) | (size << 16) | (0x15 << 8) | 1, which is the same asm-generic
 * restatement #904 was about, still wrong on ppc64le, three files from the fix.
 * Reported by logan2611.
 *
 * It arrived in 6.11, so building against older headers means supplying it -
 * but the fallback asks _IOR() rather than writing the bits down. We supply the
 * struct; the target supplies the encoding.
 *
 * The opcode encodes sizeof(struct fs_sysfs_path), and Rust carries its own
 * definition because linux/fs.h's types don't survive the bindgen blocklist. So
 * export the size too: the two have to agree, and disagreeing would otherwise
 * surface as an ENOTTY at runtime rather than an error at build time.
 */
#ifndef FS_IOC_GETFSSYSFSPATH
struct fs_sysfs_path {
	__u8			len;
	__u8			name[128];
};
#define FS_IOC_GETFSSYSFSPATH	_IOR(0x15, 1, struct fs_sysfs_path)
#endif

static const unsigned long BCH_FS_IOC_GETFSSYSFSPATH = FS_IOC_GETFSSYSFSPATH;
static const size_t BCH_SIZEOF_FS_SYSFS_PATH = sizeof(struct fs_sysfs_path);

/*
 * Same story for FIEMAP, one layer out: the fiemap crate had it as a literal
 * 0xC020660B, so migrate and copy_fs couldn't read an extent map on any
 * architecture that isn't asm-generic. We vendor that crate now (fs/vendor/
 * fiemap) for this one line.
 */
#include <linux/fiemap.h>
static const unsigned long BCH_FS_IOC_FIEMAP = FS_IOC_FIEMAP;

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
 * Bitmap shim — set_bit() is atomic (locked bitops in the kernel),
 * can't be inlined through bindgen.
 */
void rust_set_bit(unsigned long nr, unsigned long *addr);

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

struct bch_write_op;
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

struct bch_read_bio;
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
 * bch2_bkey_append_ptr, bucket_gen, bch2_disk_reservation_add/put,
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

/*
 * Unit test for the eytzinger sort/search primitive and the darray 1-based
 * wrapper (snapshot_id_dying's lookup path). Runs under `cargo test` via a
 * Rust #[test] wrapper. Returns the number of failed assertions (0 == pass);
 * failure details are printed to stderr.
 */
int rust_eytzinger_test(void);

/*
 * Unit test for the cuckoo u64 set (fs/util/cuckoo.h). Runs under `cargo test`
 * via a Rust #[test] wrapper. Returns the number of failed assertions (0 ==
 * pass); failure details are printed to stderr.
 */
int rust_cuckoo_test(void);

/*
 * Unit test for the userspace percpu shim's slot lifetime (linux/percpu.c):
 * exited threads return their slot, live threads never share one. Runs under
 * `cargo test` via a Rust #[test] wrapper. Returns the number of failed
 * assertions (0 == pass); failure details are printed to stderr.
 */
int rust_percpu_test(void);

#endif /* _RUST_SHIMS_H */
