
#include <getopt.h>
#include <stdio.h>

#include "bcachefs_ioctl.h"
#include "btree/cache.h"
#include "data/move.h"

#include "cmds.h"
#include "libbcachefs.h"

/* Obsolete, will be deleted */

static void data_rereplicate_usage(void)
{
	puts("bcachefs data rereplicate\n"
	     "Usage: bcachefs data rereplicate filesystem\n"
	     "\n"
	     "Walks existing data in a filesystem, writing additional copies\n"
	     "of any degraded data\n"
	     "\n"
	     "Options:\n"
	     "  -h, --help                   Display this help and exit\n"
	     "\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
	exit(EXIT_SUCCESS);
}

static int cmd_data_rereplicate(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "help",		no_argument, NULL, 'h' },
		{ NULL }
	};
	int opt;

	while ((opt = getopt_long(argc, argv, "h", longopts, NULL)) != -1)
		switch (opt) {
		case 'h':
			data_rereplicate_usage();
		}
	args_shift(optind);

	if (bcachefs_kernel_version() >= bcachefs_metadata_version_reconcile)
		die("rereplicate no longer required or support >= reconcile; use 'bcachefs reconcile wait'");

	char *fs_path = arg_pop();
	if (!fs_path)
		die("Please supply a filesystem");

	if (argc)
		die("too many arguments");

	return bchu_data(bcache_fs_open(fs_path), (struct bch_ioctl_data) {
		.op		= BCH_DATA_OP_rereplicate,
		.start_btree	= 0,
		.start_pos	= POS_MIN,
		.end_btree	= BTREE_ID_NR,
		.end_pos	= POS_MAX,
	});
}

static void data_job_usage(void)
{
	puts("bcachefs data job\n"
	     "Usage: bcachefs data job [job} filesystem\n"
	     "\n"
	     "Kick off a data job and report progress\n"
	     "\n"
	     "job: one of scrub, rereplicate, migrate, rewrite_old_nodes, or drop_extra_replicas\n"
	     "\n"
	     "Options:\n"
	     "  -b, --btree btree            Btree to operate on\n"
	     "  -s, --start inode:offset     Start position\n"
	     "  -e, --end   inode:offset     End position\n"
	     "  -h, --help                   Display this help and exit\n"
	     "\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
	exit(EXIT_SUCCESS);
}

static int cmd_data_job(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "btree",		required_argument,	NULL, 'b' },
		{ "start",		required_argument,	NULL, 's' },
		{ "end",		required_argument,	NULL, 'e' },
		{ "help",		no_argument,		NULL, 'h' },
		{ NULL }
	};
	struct bch_ioctl_data op = {
		.start_btree	= 0,
		.start_pos	= POS_MIN,
		.end_btree	= BTREE_ID_NR,
		.end_pos	= POS_MAX,
	};
	int opt;

	while ((opt = getopt_long(argc, argv, "b:s:e:h", longopts, NULL)) != -1)
		switch (opt) {
		case 'b':
			op.start_btree = read_string_list_or_die(optarg,
						__bch2_btree_ids, "btree id");
			op.end_btree = op.start_btree;
			break;
		case 's':
			op.start_pos	= bpos_parse(optarg);
			break;
		case 'e':
			op.end_pos	= bpos_parse(optarg);
			break;
		case 'h':
			data_job_usage();
		}
	args_shift(optind);

	char *job = arg_pop();
	if (!job)
		die("please specify which type of job");

	op.op = read_string_list_or_die(job, bch2_data_ops_strs, "bad job type");

	if (op.op == BCH_DATA_OP_scrub)
		die("scrub should be invoked with 'bcachefs data scrub'");

	if ((op.op == BCH_DATA_OP_rereplicate ||
	     op.op == BCH_DATA_OP_migrate ||
	     op.op == BCH_DATA_OP_drop_extra_replicas) &&
	    bcachefs_kernel_version() >= bcachefs_metadata_version_reconcile)
		die("%s no longer required or support >= reconcile; use 'bcachefs reconcile wait'", job);

	char *fs_path = arg_pop();
	if (!fs_path)
		fs_path = ".";

	if (argc)
		die("too many arguments");

	return bchu_data(bcache_fs_open(fs_path), op);
}

static int data_usage(void)
{
	puts("bcachefs data - manage filesystem data\n"
	     "Usage: bcachefs data <rereplicate|scrub|job> [OPTION]...\n"
	     "\n"
	     "Commands:\n"
	     "  rereplicate                  Rereplicate degraded data\n"
	     "  scrub                        Verify checksums and correct errors, if possible\n"
	     "  job                          Kick off low level data jobs\n"
	     "\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
	exit(EXIT_SUCCESS);
}

int data_cmds(int argc, char *argv[])
{
	char *cmd = pop_cmd(&argc, argv);

	if (argc < 1)
		return data_usage();
	if (!strcmp(cmd, "rereplicate"))
		return cmd_data_rereplicate(argc, argv);
	if (!strcmp(cmd, "job"))
		return cmd_data_job(argc, argv);

	data_usage();
	return -EINVAL;
}
