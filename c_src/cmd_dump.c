#include <fcntl.h>
#include <getopt.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>

#include "cmds.h"
#include "libbcachefs.h"
#include "qcow2.h"

#include "libbcachefs/bcachefs.h"
#include "libbcachefs/btree_cache.h"
#include "libbcachefs/btree_io.h"
#include "libbcachefs/btree_iter.h"
#include "libbcachefs/error.h"
#include "libbcachefs/extents.h"
#include "libbcachefs/sb-members.h"
#include "libbcachefs/super.h"

static void dump_usage(void)
{
	puts("bcachefs dump - dump filesystem metadata\n"
	     "Usage: bcachefs dump [OPTION]... <devices>\n"
	     "\n"
	     "Options:\n"
	     "  -o output       Output qcow2 image(s)\n"
	     "  -f, --force     Force; overwrite when needed\n"
	     "      --nojournal Don't dump entire journal, just dirty entries\n"
	     "      --noexcl    Open devices with O_NOEXCL (not recommended)\n"
	     "  -h, --help      Display this help and exit\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
}

struct dump_dev {
	ranges	sb, journal, btree;
};
typedef DARRAY(struct dump_dev) dump_devs;

static void dump_node(struct bch_fs *c, dump_devs *devs, struct bkey_s_c k)
{
	struct bkey_ptrs_c ptrs = bch2_bkey_ptrs_c(k);
	unsigned bytes = btree_ptr_sectors_written(k) << 9 ?: c->opts.btree_node_size;

	bkey_for_each_ptr(ptrs, ptr)
		range_add(&devs->data[ptr->dev].btree,
			  ptr->offset << 9, bytes);
}

static void get_sb_journal(struct bch_fs *c, struct bch_dev *ca,
			    bool entire_journal,
			    struct dump_dev *d)
{
	struct bch_sb *sb = ca->disk_sb.sb;

	/* Superblock: */
	range_add(&d->sb, BCH_SB_LAYOUT_SECTOR << 9,
		  sizeof(struct bch_sb_layout));

	for (unsigned i = 0; i < sb->layout.nr_superblocks; i++)
		range_add(&d->sb,
			  le64_to_cpu(sb->layout.sb_offset[i]) << 9,
			  vstruct_bytes(sb));

	/* Journal: */
	for (unsigned i = 0; i < ca->journal.nr; i++)
		if (entire_journal ||
		    ca->journal.bucket_seq[i] >= c->journal.last_seq_ondisk) {
			u64 bucket = ca->journal.buckets[i];

			range_add(&d->journal,
				  bucket_bytes(ca) * bucket,
				  bucket_bytes(ca));
		}
}

int cmd_dump(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "force",		no_argument,		NULL, 'f' },
		{ "nojournal",		no_argument,		NULL, 'j' },
		{ "noexcl",		no_argument,		NULL, 'e' },
		{ "verbose",		no_argument,		NULL, 'v' },
		{ "help",		no_argument,		NULL, 'h' },
		{ NULL }
	};
	struct bch_opts opts = bch2_opts_empty();
	char *out = NULL;
	bool force = false, entire_journal = true;
	int fd, opt;

	opt_set(opts, direct_io,	false);
	opt_set(opts, read_only,	true);
	opt_set(opts, nochanges,	true);
	opt_set(opts, norecovery,	true);
	opt_set(opts, degraded,		BCH_DEGRADED_very);
	opt_set(opts, errors,		BCH_ON_ERROR_continue);
	opt_set(opts, fix_errors,	FSCK_FIX_no);

	while ((opt = getopt_long(argc, argv, "o:fvh",
				  longopts, NULL)) != -1)
		switch (opt) {
		case 'o':
			out = optarg;
			break;
		case 'f':
			force = true;
			break;
		case 'j':
			entire_journal = false;
			break;
		case 'e':
			opt_set(opts, noexcl,		true);
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

	darray_const_str dev_names = get_or_split_cmdline_devs(argc, argv);

	struct bch_fs *c = bch2_fs_open(&dev_names, &opts);
	if (IS_ERR(c))
		die("error opening devices: %s", bch2_err_str(PTR_ERR(c)));

	dump_devs devs = {};
	while (devs.nr < c->sb.nr_devices)
		darray_push(&devs, (struct dump_dev) {});

	down_read(&c->state_lock);

	unsigned nr_online = 0;
	for_each_online_member(c, ca, 0) {
		get_sb_journal(c, ca, entire_journal, &devs.data[ca->dev_idx]);
		nr_online++;
	}

	for (unsigned i = 0; i < BTREE_ID_NR; i++) {
		CLASS(btree_trans, trans)(c);

		int ret = __for_each_btree_node(trans, iter, i, POS_MIN, 0, 1, 0, b, ({
			struct btree_node_iter iter;
			struct bkey u;
			struct bkey_s_c k;

			for_each_btree_node_key_unpack(b, k, &iter, &u)
				dump_node(c, &devs, k);
			0;
		}));

		if (ret)
			die("error %s walking btree nodes", bch2_err_str(ret));

		struct btree *b = bch2_btree_id_root(c, i)->b;
		if (!btree_node_fake(b))
			dump_node(c, &devs, bkey_i_to_s_c(&b->key));
	}

	for_each_online_member(c, ca, 0) {
		int flags = O_WRONLY|O_CREAT|O_TRUNC;

		if (!force)
			flags |= O_EXCL;

		char *path = nr_online > 1
			? mprintf("%s.%u.qcow2", out, ca->dev_idx)
			: mprintf("%s.qcow2", out);
		fd = xopen(path, flags, 0600);
		free(path);

		struct qcow2_image img;
		qcow2_image_init(&img, ca->disk_sb.bdev->bd_fd, fd, c->opts.block_size);

		qcow2_write_ranges(&img, &devs.data[ca->dev_idx].sb);
		qcow2_write_ranges(&img, &devs.data[ca->dev_idx].journal);
		qcow2_write_ranges(&img, &devs.data[ca->dev_idx].btree);

		qcow2_image_finish(&img);
		xclose(fd);
	}

	up_read(&c->state_lock);

	bch2_fs_stop(c);

	darray_for_each(devs, d) {
		darray_exit(&d->sb);
		darray_exit(&d->journal);
		darray_exit(&d->btree);
	}
	darray_exit(&devs);
	darray_exit(&dev_names);
	return 0;
}
