
#include <getopt.h>
#include "cmds.h"
#include "libbcachefs/error.h"
#include "libbcachefs.h"
#include "libbcachefs/super.h"
#include "tools-util.h"

static void usage(void)
{
	puts("bcachefs fsck - filesystem check and repair\n"
	     "Usage: bcachefs fsck [OPTION]... <devices>\n"
	     "\n"
	     "Options:\n"
	     "  -p                      Automatic repair (no questions)\n"
	     "  -n                      Don't repair, only check for errors\n"
	     "  -y                      Assume \"yes\" to all questions\n"
	     "  -f                      Force checking even if filesystem is marked clean\n"
	     "  -r, --ratelimit_errors  Don't display more than 10 errors of a given type\n"
	     "  -R, --reconstruct_alloc Reconstruct the alloc btree\n"
	     "  -v                      Be verbose\n"
	     "  -h, --help              Display this help and exit\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
}

int cmd_fsck(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "ratelimit_errors",	no_argument,		NULL, 'r' },
		{ "reconstruct_alloc",	no_argument,		NULL, 'R' },
		{ "help",		no_argument,		NULL, 'h' },
		{ NULL }
	};
	struct bch_opts opts = bch2_opts_empty();
	unsigned i;
	int opt, ret = 0;

	opt_set(opts, degraded, true);
	opt_set(opts, fsck, true);
	opt_set(opts, fix_errors, FSCK_FIX_ask);

	while ((opt = getopt_long(argc, argv,
				  "apynfo:rvh",
				  longopts, NULL)) != -1)
		switch (opt) {
		case 'a': /* outdated alias for -p */
		case 'p':
		case 'y':
			opt_set(opts, fix_errors, FSCK_FIX_yes);
			break;
		case 'n':
			opt_set(opts, nochanges, true);
			opt_set(opts, fix_errors, FSCK_FIX_no);
			break;
		case 'f':
			/* force check, even if filesystem marked clean: */
			break;
		case 'o':
			ret = bch2_parse_mount_opts(NULL, &opts, optarg);
			if (ret)
				return ret;
			break;
		case 'r':
			opt_set(opts, ratelimit_errors, true);
			break;
		case 'R':
			opt_set(opts, reconstruct_alloc, true);
			break;
		case 'v':
			opt_set(opts, verbose, true);
			break;
		case 'h':
			usage();
			exit(16);
		}
	args_shift(optind);

	if (!argc) {
		fprintf(stderr, "Please supply device(s) to check\n");
		exit(8);
	}

	for (i = 0; i < argc; i++) {
		switch (dev_mounted(argv[i])) {
		case 1:
			ret |= 2;
			break;
		case 2:
			fprintf(stderr, "%s is mounted read-write - aborting\n", argv[i]);
			exit(8);
		}
	}

	struct bch_fs *c = bch2_fs_open(argv, argc, opts);
	if (IS_ERR(c)) {
		fprintf(stderr, "error opening %s: %s\n", argv[0], bch2_err_str(PTR_ERR(c)));
		exit(8);
	}

	if (test_bit(BCH_FS_errors_fixed, &c->flags)) {
		fprintf(stderr, "%s: errors fixed\n", c->name);
		ret |= 1;
	}
	if (test_bit(BCH_FS_error, &c->flags)) {
		fprintf(stderr, "%s: still has errors\n", c->name);
		ret |= 4;
	}

	bch2_fs_stop(c);
	return ret;
}
