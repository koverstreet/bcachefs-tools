#include <fcntl.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>

#include "cmds.h"
#include "libbcachefs.h"
#include "tools-util.h"

#include "libbcachefs/bcachefs.h"
#include "libbcachefs/btree_iter.h"
#include "libbcachefs/errcode.h"
#include "libbcachefs/error.h"
#include "libbcachefs/sb-members.h"
#include "libbcachefs/super.h"

static void kill_btree_node_usage(void)
{
	puts("bcachefs kill_btree_node - make btree nodes unreadable\n"
	     "Usage: bcachefs kill_btree_node [OPTION]... <devices>\n"
	     "\n"
	     "Options:\n"
	     "  -b (extents|inodes|dirents|xattrs)    Btree to delete from\n"
	     "  -l level                              Levle to delete from (0 == leaves)\n"
	     "  -i index                              Index of btree node to kill\n"
	     "  -h                                    Display this help and exit\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
}

struct kill_node {
	unsigned	btree;
	unsigned	level;
	u64		idx;
};

int cmd_kill_btree_node(int argc, char *argv[])
{
	struct bch_opts opts = bch2_opts_empty();
	DARRAY(struct kill_node) kill_nodes = {};
	int opt;

	opt_set(opts, read_only,	true);

	while ((opt = getopt(argc, argv, "n:h")) != -1)
		switch (opt) {
		case 'n': {
			char *p = optarg;
			const char *str_btree	= strsep(&p, ":");
			const char *str_level	= strsep(&p, ":");
			const char *str_idx	= strsep(&p, ":");

			struct kill_node n = {
				.btree = read_string_list_or_die(str_btree,
						__bch2_btree_ids, "btree id"),
			};

			if (str_level &&
			    (kstrtouint(str_level, 10, &n.level) || n.level >= BTREE_MAX_DEPTH))
				die("invalid level");

			if (str_idx &&
			    kstrtoull(optarg, 10, &n.idx))
				die("invalid index %s", str_idx);

			darray_push(&kill_nodes, n);
			break;
		}
		case 'h':
			kill_btree_node_usage();
			exit(EXIT_SUCCESS);
		}
	args_shift(optind);

	if (!argc)
		die("Please supply device(s)");

	struct bch_fs *c = bch2_fs_open(argv, argc, opts);
	if (IS_ERR(c))
		die("error opening %s: %s", argv[0], bch2_err_str(PTR_ERR(c)));

	int ret;
	void *zeroes;

	ret = posix_memalign(&zeroes, c->opts.block_size, c->opts.block_size);
	if (ret)
		die("error %s from posix_memalign", bch2_err_str(ret));

	struct btree_trans *trans = bch2_trans_get(c);

	darray_for_each(kill_nodes, i) {
		ret = __for_each_btree_node(trans, iter, i->btree, POS_MIN, 0, i->level, 0, b, ({
			if (b->c.level != i->level)
				continue;

			int ret2 = 0;
			if (!i->idx) {
				struct printbuf buf = PRINTBUF;
				bch2_bkey_val_to_text(&buf, c, bkey_i_to_s_c(&b->key));
				bch_info(c, "killing btree node %s l=%u %s",
					 bch2_btree_id_str(i->btree), i->level, buf.buf);
				printbuf_exit(&buf);

				ret2 = 1;

				struct bkey_ptrs_c ptrs = bch2_bkey_ptrs_c(bkey_i_to_s_c(&b->key));
				bkey_for_each_ptr(ptrs, ptr) {
					struct bch_dev *ca = bch2_dev_tryget(c, ptr->dev);
					if (!ca)
						continue;

					int ret3 = pwrite(ca->disk_sb.bdev->bd_fd, zeroes,
						     c->opts.block_size, ptr->offset << 9);
					bch2_dev_put(ca);
					if (ret3 != c->opts.block_size) {
						bch_err(c, "pwrite error: expected %u got %i %s",
							c->opts.block_size, ret, strerror(errno));
						ret2 = EXIT_FAILURE;
					}
				}
			}

			i->idx--;
			ret2;
		}));

		if (ret < 0) {
			bch_err(c, "error %i walking btree nodes", ret);
			break;
		} else if (!ret) {
			bch_err(c, "node at specified index not found");
			ret = EXIT_FAILURE;
			break;
		}
	}

	bch2_trans_put(trans);
	bch2_fs_stop(c);
	darray_exit(&kill_nodes);
	return ret < 0 ? ret : 0;
}
