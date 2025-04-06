/*
 * Authors: Kent Overstreet <kent.overstreet@gmail.com>
 *
 * GPLv2
 */
#include <ctype.h>
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <getopt.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

#include <uuid/uuid.h>

#include "cmds.h"
#include "cmd_strip_alloc.h"
#include "posix_to_bcachefs.h"
#include "libbcachefs.h"
#include "crypto.h"
#include "libbcachefs/alloc_background.h"
#include "libbcachefs/alloc_foreground.h"
#include "libbcachefs/data_update.h"
#include "libbcachefs/errcode.h"
#include "libbcachefs/move.h"
#include "libbcachefs/opts.h"
#include "libbcachefs/super-io.h"
#include "libbcachefs/util.h"

#include "libbcachefs/darray.h"

static u64 count_input_size(int dirfd)
{
	DIR *dir = fdopendir(dirfd);
	struct dirent *d;
	u64 bytes = 0;

	while ((errno = 0), (d = readdir(dir))) {
		struct stat stat =
			xfstatat(dirfd, d->d_name, AT_SYMLINK_NOFOLLOW);

		if (!strcmp(d->d_name, ".") ||
		    !strcmp(d->d_name, "..") ||
		    !strcmp(d->d_name, "lost+found"))
			continue;

		bytes += stat.st_blocks << 9;

		if (mode_to_type(stat.st_mode) == DT_DIR) {
			int fd = xopenat(dirfd, d->d_name, O_RDONLY|O_NOATIME);
			bytes += count_input_size(fd);
			xclose(fd);
		}
	}

	if (errno)
		die("readdir error: %m");
	return bytes;
}

struct move_btree_args {
	bool		move_alloc;
	unsigned	target;
};

static bool move_btree_pred(struct bch_fs *c, void *_arg,
			    enum btree_id btree, struct bkey_s_c k,
			    struct bch_io_opts *io_opts,
			    struct data_update_opts *data_opts)
{
	struct move_btree_args *args = _arg;

	data_opts->target = dev_to_target(0);
	data_opts->target = args->target;

	if (k.k->type != KEY_TYPE_btree_ptr_v2)
		return false;

	if (!args->move_alloc && btree_id_is_alloc(btree))
		return false;

	return true;
	return k.k->type == KEY_TYPE_btree_ptr_v2 && !btree_id_is_alloc(btree);
}

static int move_btree(struct bch_fs *c, bool move_alloc, unsigned target_dev)
{
	struct move_btree_args args = {
		.move_alloc	= move_alloc,
		.target		= dev_to_target(target_dev),
	};

	struct moving_context ctxt;
	bch2_moving_ctxt_init(&ctxt, c, NULL, NULL, writepoint_hashed(1), false);
	int ret = 0;

	for (unsigned btree = 0; btree < BTREE_ID_NR; btree++) {
		if (!move_alloc && btree_id_is_alloc(btree))
			continue;

		for (unsigned level = 1; level < BTREE_MAX_DEPTH; level++) {
			ret = bch2_move_data_btree(&ctxt,
						   POS_MIN, SPOS_MAX,
						   move_btree_pred, &args,
						   btree, level);
			if (ret)
				goto err;
		}
	}
err:
	bch2_moving_ctxt_exit(&ctxt);
	return ret;
}

static void check_gaps(struct bch_fs *c)
{
	/* Check for gaps, make sure the allocator is behaving correctly */
	u64 prev_bucket = 0;
	bch2_trans_run(c,
		for_each_btree_key_max(trans, iter, BTREE_ID_alloc, POS_MIN, POS(0, U64_MAX), 0, k, ({
			if (k.k->type == KEY_TYPE_alloc_v4) {
				struct bkey_s_c_alloc_v4 a = bkey_s_c_to_alloc_v4(k);

				if ((prev_bucket && prev_bucket + 1 != k.k->p.offset) ||
				    a.v->dirty_sectors != c->devs[0]->mi.bucket_size)
					pr_info("%llu %llu %s %u", prev_bucket, k.k->p.offset,
						__bch2_data_types[a.v->data_type],
						a.v->dirty_sectors);
				prev_bucket = k.k->p.offset;
			}

			0;
		})));
}

static int get_nbuckets_used(struct bch_fs *c, u64 *nbuckets)
{
	struct btree_trans *trans = bch2_trans_get(c);
	struct btree_iter iter;
	bch2_trans_iter_init(trans, &iter, BTREE_ID_alloc, POS(0, U64_MAX), 0);
	struct bkey_s_c k;
	int ret = lockrestart_do(trans, bkey_err(k = bch2_btree_iter_peek_prev(trans, &iter)));
	if (!ret && k.k->type != KEY_TYPE_alloc_v4)
		ret = -ENOENT;
	if (ret) {
		fprintf(stderr, "error looking up last alloc key: %s\n", bch2_err_str(ret));
		goto err;
	}

	*nbuckets = (k.k->p.offset + 1);
err:
	bch2_trans_iter_exit(trans, &iter);
	bch2_trans_put(trans);
	return ret;

}

static void print_dev_usage_all(struct bch_fs *c)
{
	struct printbuf buf = PRINTBUF;

	for_each_member_device(c, ca) {
		struct bch_dev_usage_full stats = bch2_dev_usage_full_read(ca);
		bch2_dev_usage_to_text(&buf, ca, &stats);
	}

	printf("%s", buf.buf);
	printbuf_exit(&buf);
}

/*
 * Build an image file:
 *
 * Use a temporary second device for metadata, so that we can write out data
 * reprodicably, sequentially from the start of the device.
 *
 * After data is written out, the metadata that we want to keep is moved to the
 * real image file. By default, alloc info is left out: it will be recreated on
 * first RW mount.
 *
 * After migrating metadata, the image file is trimmed and the temporary
 * metadata device is dropped.
 */
static void image_create(struct bch_opt_strs	fs_opt_strs,
			 struct bch_opts		fs_opts,
			 struct format_opts	format_opts,
			 struct dev_opts		dev_opts,
			 const char		*src_path,
			 bool			keep_alloc,
			 unsigned		verbosity)
{
	int src_fd = xopen(src_path, O_RDONLY);

	if (!S_ISDIR(xfstat(src_fd).st_mode))
		die("%s is not a directory", src_path);

	u64 input_bytes = count_input_size(src_fd);
	lseek(src_fd, 0, SEEK_SET);

	dev_opts_list devs = {};
	darray_push(&devs, dev_opts);

	dev_opts.path = mprintf("%s.metadata", devs.data[0].path),
		darray_push(&devs, dev_opts);

	if (!access(devs.data[1].path, F_OK))
		die("temporary metadata device %s already exists", devs.data[1].path);

	opt_set(devs.data[0].opts, data_allowed, BIT(BCH_DATA_user));
	opt_set(devs.data[1].opts, data_allowed, BIT(BCH_DATA_journal)|BIT(BCH_DATA_btree));

	darray_for_each(devs, dev) {
		int ret = open_for_format(dev, BLK_OPEN_CREAT, false);
		if (ret) {
			fprintf(stderr, "Error opening %s: %s", dev->path, strerror(-ret));
			goto err;
		}

		if (ftruncate(dev->bdev->bd_fd, input_bytes * 2)) {
			fprintf(stderr, "ftruncate error: %m");
			goto err;
		}
	}

	format_opts.no_sb_at_end = true;
	struct bch_sb *sb = bch2_format(fs_opt_strs, fs_opts, format_opts, devs);
	if (verbosity > 1) {
		struct printbuf buf = PRINTBUF;
		buf.human_readable_units = true;

		bch2_sb_to_text(&buf, sb, false, 1 << BCH_SB_FIELD_members_v2);
		printf("%s", buf.buf);
		printbuf_exit(&buf);
	}

	darray_const_str device_paths = {};
	darray_for_each(devs, dev)
		darray_push(&device_paths, dev->path);

	struct bch_opts opts = bch2_opts_empty();
	opt_set(opts, copygc_enabled,		false);
	opt_set(opts, rebalance_enabled,	false);

	struct bch_fs *c = bch2_fs_open(&device_paths, &opts);

	unlink(device_paths.data[1]);

	int ret = PTR_ERR_OR_ZERO(c);
	if (ret) {
		fprintf(stderr, "error opening %s: %s\n",
			device_paths.data[0], bch2_err_str(ret));
		goto err;
	}

	struct copy_fs_state s = {};
	copy_fs(c, src_fd, src_path, &s, 0);

	printf("moving non-alloc btree to primary device\n");

	mutex_lock(&c->sb_lock);
	struct bch_member *m = bch2_members_v2_get_mut(c->disk_sb.sb, 0);
	SET_BCH_MEMBER_DATA_ALLOWED(m, BCH_MEMBER_DATA_ALLOWED(m)|BIT(BCH_DATA_btree));
	bch2_write_super(c);
	mutex_unlock(&c->sb_lock);

	bch2_dev_allocator_set_rw(c, c->devs[0], true);

	ret = move_btree(c, keep_alloc, 0);
	if (ret) {
		fprintf(stderr, "error migrating btree from temporary device: %s\n",
			bch2_err_str(ret));
		goto err;
	}

	bch2_fs_read_only(c);

	if (verbosity > 1)
		print_dev_usage_all(c);

	if (0)
		check_gaps(c);

	u64 nbuckets;
	ret = get_nbuckets_used(c, &nbuckets);
	if (ret)
		goto err;

	if (ftruncate(c->devs[0]->disk_sb.bdev->bd_fd, nbuckets * bucket_bytes(c->devs[0]))) {
		fprintf(stderr, "truncate error: %m\n");
		goto err;
	}

	mutex_lock(&c->sb_lock);
	if (!keep_alloc) {
		printf("Stripping alloc info\n");
		strip_fs_alloc(c);
	}

	rcu_assign_pointer(c->devs[1], NULL);

	m = bch2_members_v2_get_mut(c->disk_sb.sb, 0);
	SET_BCH_MEMBER_DATA_ALLOWED(m, BCH_MEMBER_DATA_ALLOWED(m)|BIT(BCH_DATA_journal));

	bch2_members_v2_get_mut(c->disk_sb.sb, 0)->nbuckets = cpu_to_le64(nbuckets);

	for_each_online_member(c, ca, 0) {
		struct bch_member *m = bch2_members_v2_get_mut(c->disk_sb.sb, ca->dev_idx);
		SET_BCH_MEMBER_RESIZE_ON_MOUNT(m, true);
	}

	c->disk_sb.sb->features[0] |= cpu_to_le64(BIT_ULL(BCH_FEATURE_small_image));

	/*
	 * sb->nr_devices must be 1 so that it can be mounted without UUID
	 * conflicts
	 */
	unsigned u64s = DIV_ROUND_UP(sizeof(struct bch_sb_field_members_v2) +
				     sizeof(struct bch_member), sizeof(u64));
	bch2_sb_field_resize(&c->disk_sb, members_v2, u64s);
	c->disk_sb.sb->nr_devices = 1;
	SET_BCH_SB_MULTI_DEVICE(c->disk_sb.sb, false);

	bch2_write_super(c);
	mutex_unlock(&c->sb_lock);

	bch2_fs_stop(c);
	darray_exit(&device_paths);
	xclose(src_fd);
	return;
err:
	darray_for_each(devs, d)
		unlink(d->path);
	exit(EXIT_FAILURE);
}

static void image_create_usage(void)
{
	puts("bcachefs image create - create a minimum size, reproducible filesystem image\n"
	     "Usage: bcachefs image create [OPTION]... <file>\n"
	     "\n"
	     "Options:\n"
	     "      --source=path           Source directory to be used as content for the new image\n"
	     "  -a, --keep-alloc            Include allocation info in the filesystem\n"
	     "                              6.16+ regenerates alloc info on first rw mount\n"
	     "      --encrypted             Enable whole filesystem encryption (chacha20/poly1305)\n"
	     "  -L, --fs_label=label\n"
	     "  -U, --uuid=uuid\n"
	     "      --superblock_size=size\n"
	     "      --bucket_size=size\n"
	     "      --fs_size=size          Expected size of device image will be used on, hint for bucket size\n"
	     "  -f, --force\n"
	     "  -q, --quiet                 Only print errors\n"
	     "  -v, --verbose               Verbose filesystem initialization\n"
	     "  -h, --help                  Display this help and exit\n"
	     "\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
}

static int cmd_image_create(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "source",		required_argument,	NULL, 's' },
		{ "keep-alloc",		no_argument,		NULL, 'a' },
		{ "encrypted",		required_argument,	NULL, 'e' },
		{ "fs_label",		required_argument,	NULL, 'L' },
		{ "uuid",		required_argument,	NULL, 'U' },
		{ "superblock_size",	required_argument,	NULL, 'S' },
		{ "force",		no_argument,		NULL, 'f' },
		{ "quiet",		no_argument,		NULL, 'q' },
		{ "verbose",		no_argument,		NULL, 'v' },
		{ "help",		no_argument,		NULL, 'h' },
		{ NULL }
	};
	struct format_opts opts	= format_opts_default();
	struct dev_opts dev_opts = dev_opts_default();
	bool keep_alloc = false, force = false;
	unsigned verbosity = 1;
	struct bch_opt_strs fs_opt_strs = {};
	struct bch_opts fs_opts = bch2_opts_empty();

	opts.superblock_size = 128;	/* 64k */

	while (true) {
		const struct bch_option *opt =
			bch2_cmdline_opt_parse(argc, argv, OPT_FORMAT|OPT_FS|OPT_DEVICE);
		if (opt) {
			unsigned id = opt - bch2_opt_table;
			u64 v;
			struct printbuf err = PRINTBUF;
			int ret = bch2_opt_parse(NULL, opt, optarg, &v, &err);
			if (ret == -BCH_ERR_option_needs_open_fs) {
				fs_opt_strs.by_id[id] = strdup(optarg);
				continue;
			}
			if (ret)
				die("invalid option: %s", err.buf);

			if (opt->flags & OPT_DEVICE)
				bch2_opt_set_by_id(&dev_opts.opts, id, v);
			else if (opt->flags & OPT_FS)
				bch2_opt_set_by_id(&fs_opts, id, v);
			else
				die("got bch_opt of wrong type %s", opt->attr.name);

			continue;
		}

		int optid = getopt_long(argc, argv,
					"s:aeL:U:S:fqvh",
					longopts, NULL);
		if (optid == -1)
			break;

		switch (optid) {
		case 's':
			opts.source = optarg;
			break;
		case 'a':
			keep_alloc = true;
			break;
		case 'L':
			opts.label = optarg;
			break;
		case 'U':
			if (uuid_parse(optarg, opts.uuid.b))
				die("Bad uuid");
			break;
		case 'S':
			if (bch2_strtouint_h(optarg, &opts.superblock_size))
				die("invalid filesystem size");

			opts.superblock_size >>= 9;
			break;
		case 'f':
			force = true;
			break;
		case 'q':
			verbosity = 0;
			break;
		case 'v':
			verbosity++;
			break;
		case 'h':
			image_create_usage();
			exit(EXIT_SUCCESS);
			break;
		case '?':
			exit(EXIT_FAILURE);
			break;
		default:
			die("getopt ret %i %c", optid, optid);
		}
	}
	args_shift(optind);

	if (argc != 1)
		die("Please supply a filename for the new image");

	dev_opts.path = argv[0];

	image_create(fs_opt_strs, fs_opts, opts, dev_opts, opts.source, keep_alloc, verbosity);
	bch2_opt_strs_free(&fs_opt_strs);
	return 0;
}

static int image_usage(void)
{
	puts("bcachefs image - commands for creating and updating image files\n"
	     "Usage: bcachefs image <CMD> [OPTION]...\n"
            "\n"
            "Commands:\n"
            "  create                  Create a minimally-sized disk image\n"
            "\n"
            "Report bugs to <linux-bcachefs@vger.kernel.org>");
	return 0;
}

int image_cmds(int argc, char *argv[])
{
	char *cmd = pop_cmd(&argc, argv);

	if (argc < 1)
		return image_usage();
	if (!strcmp(cmd, "create"))
		return cmd_image_create(argc, argv);

	image_usage();
	return -EINVAL;
}
