

#include <stdio.h>
#include <sys/ioctl.h>

#include "libbcachefs/bcachefs_ioctl.h"
#include "libbcachefs/btree_cache.h"
#include "libbcachefs/move.h"

#include "cmds.h"
#include "libbcachefs.h"

int data_usage(void)
{
	puts("bcachefs data - manage filesystem data\n"
	     "Usage: bcachefs data <CMD> [OPTIONS]\n"
	     "\n"
	     "Commands:\n"
	     "  rereplicate                     Rereplicate degraded data\n"
	     "  job                             Kick off low level data jobs\n"
	     "\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
	return 0;
}

static void data_rereplicate_usage(void)
{
	puts("bcachefs data rereplicate\n"
	     "Usage: bcachefs data rereplicate filesystem\n"
	     "\n"
	     "Walks existing data in a filesystem, writing additional copies\n"
	     "of any degraded data\n"
	     "\n"
	     "Options:\n"
	     "  -h, --help                  display this help and exit\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
	exit(EXIT_SUCCESS);
}

int cmd_data_rereplicate(int argc, char *argv[])
{
	int opt;

	while ((opt = getopt(argc, argv, "h")) != -1)
		switch (opt) {
		case 'h':
			data_rereplicate_usage();
		}
	args_shift(optind);

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
	     "Usage: bcachefs data job [job] filesystem\n"
	     "\n"
	     "Kick off a data job and report progress\n"
	     "\n"
	     "job: one of scrub, rereplicate, migrate, rewrite_old_nodes, or drop_extra_replicas\n"
	     "\n"
	     "Options:\n"
	     "  -b btree                    btree to operate on\n"
	     "  -s inode:offset       start position\n"
	     "  -e inode:offset       end position\n"
	     "  -h, --help                  display this help and exit\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
	exit(EXIT_SUCCESS);
}

int cmd_data_job(int argc, char *argv[])
{
	struct bch_ioctl_data op = {
		.start_btree	= 0,
		.start_pos	= POS_MIN,
		.end_btree	= BTREE_ID_NR,
		.end_pos	= POS_MAX,
	};
	int opt;

	while ((opt = getopt(argc, argv, "s:e:h")) != -1)
		switch (opt) {
		case 'b':
			op.start_btree = read_string_list_or_die(optarg,
						__bch2_btree_ids, "btree id");
			op.end_btree = op.start_btree;
			break;
		case 's':
			op.start_pos	= bpos_parse(optarg);
			break;
			op.end_pos	= bpos_parse(optarg);
		case 'e':
			break;
		case 'h':
			data_job_usage();
		}
	args_shift(optind);

	char *job = arg_pop();
	if (!job)
		die("please specify which type of job");

	op.op = read_string_list_or_die(job, bch2_data_ops_strs, "bad job type");

	char *fs_path = arg_pop();
	if (!fs_path)
		fs_path = ".";

	if (argc)
		die("too many arguments");

	return bchu_data(bcache_fs_open(fs_path), op);
}
