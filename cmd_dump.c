#include <fcntl.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>

#include "cmds.h"
#include "libbcachefs.h"
#include "qcow2.h"

#include "libbcachefs/bcachefs.h"
#include "libbcachefs/btree_cache.h"
#include "libbcachefs/btree_iter.h"
#include "libbcachefs/error.h"
#include "libbcachefs/extents.h"
#include "libbcachefs/super.h"

static void dump_usage(void)
{
	puts("bcachefs dump - dump filesystem metadata\n"
	     "Usage: bcachefs dump [OPTION]... <devices>\n"
	     "\n"
	     "Options:\n"
	     "  -o output     Output qcow2 image(s)\n"
	     "  -f            Force; overwrite when needed\n"
	     "  -j            Dump entire journal, not just dirty entries\n"
	     "  -h            Display this help and exit\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
}

static void dump_one_device(struct bch_fs *c, struct bch_dev *ca, int fd,
			    bool entire_journal)
{
	struct bch_sb *sb = ca->disk_sb.sb;
	ranges data = { 0 };
	unsigned i;
	int ret;

	/* Superblock: */
	range_add(&data, BCH_SB_LAYOUT_SECTOR << 9,
		  sizeof(struct bch_sb_layout));

	for (i = 0; i < sb->layout.nr_superblocks; i++)
		range_add(&data,
			  le64_to_cpu(sb->layout.sb_offset[i]) << 9,
			  vstruct_bytes(sb));

	/* Journal: */
	for (i = 0; i < ca->journal.nr; i++)
		if (entire_journal ||
		    ca->journal.bucket_seq[i] >= c->journal.last_seq_ondisk) {
			u64 bucket = ca->journal.buckets[i];

			range_add(&data,
				  bucket_bytes(ca) * bucket,
				  bucket_bytes(ca));
		}

	/* Btree: */
	for (i = 0; i < BTREE_ID_NR; i++) {
		const struct bch_extent_ptr *ptr;
		struct bkey_ptrs_c ptrs;
		struct btree_trans trans;
		struct btree_iter iter;
		struct btree *b;

		bch2_trans_init(&trans, c, 0, 0);

		__for_each_btree_node(&trans, iter, i, POS_MIN, 0, 1, 0, b, ret) {
			struct btree_node_iter iter;
			struct bkey u;
			struct bkey_s_c k;

			for_each_btree_node_key_unpack(b, k, &iter, &u) {
				ptrs = bch2_bkey_ptrs_c(k);

				bkey_for_each_ptr(ptrs, ptr)
					if (ptr->dev == ca->dev_idx)
						range_add(&data,
							  ptr->offset << 9,
							  btree_bytes(c));
			}
		}

		if (ret)
			die("error %s walking btree nodes", bch2_err_str(ret));

		b = bch2_btree_id_root(c, i)->b;
		if (!btree_node_fake(b)) {
			ptrs = bch2_bkey_ptrs_c(bkey_i_to_s_c(&b->key));

			bkey_for_each_ptr(ptrs, ptr)
				if (ptr->dev == ca->dev_idx)
					range_add(&data,
						  ptr->offset << 9,
						  btree_bytes(c));
		}

		bch2_trans_iter_exit(&trans, &iter);
		bch2_trans_exit(&trans);
	}

	qcow2_write_image(ca->disk_sb.bdev->bd_buffered_fd, fd, &data,
			  max_t(unsigned, btree_bytes(c) / 8, block_bytes(c)));
	darray_exit(&data);
}

int cmd_dump(int argc, char *argv[])
{
	struct bch_opts opts = bch2_opts_empty();
	struct bch_dev *ca;
	char *out = NULL;
	unsigned i, nr_devices = 0;
	bool force = false, entire_journal = false;
	int fd, opt;

	opt_set(opts, nochanges,	true);
	opt_set(opts, norecovery,	true);
	opt_set(opts, degraded,		true);
	opt_set(opts, errors,		BCH_ON_ERROR_continue);
	opt_set(opts, fix_errors,	FSCK_FIX_no);

	while ((opt = getopt(argc, argv, "o:fjvh")) != -1)
		switch (opt) {
		case 'o':
			out = optarg;
			break;
		case 'f':
			force = true;
			break;
		case 'j':
			entire_journal = true;
			break;
		case 'v':
			opt_set(opts, verbose, true);
			break;
		case 'h':
			dump_usage();
			exit(EXIT_SUCCESS);
		}
	args_shift(optind);

	if (!out)
		die("Please supply output filename");

	if (!argc)
		die("Please supply device(s) to check");

	struct bch_fs *c = bch2_fs_open(argv, argc, opts);
	if (IS_ERR(c))
		die("error opening %s: %s", argv[0], bch2_err_str(PTR_ERR(c)));

	down_read(&c->gc_lock);

	for_each_online_member(ca, c, i)
		nr_devices++;

	BUG_ON(!nr_devices);

	for_each_online_member(ca, c, i) {
		int flags = O_WRONLY|O_CREAT|O_TRUNC;

		if (!force)
			flags |= O_EXCL;

		if (!c->devs[i])
			continue;

		char *path = nr_devices > 1
			? mprintf("%s.%u.qcow2", out, i)
			: mprintf("%s.qcow2", out);
		fd = xopen(path, flags, 0600);
		free(path);

		dump_one_device(c, ca, fd, entire_journal);
		close(fd);
	}

	up_read(&c->gc_lock);

	bch2_fs_stop(c);
	return 0;
}
