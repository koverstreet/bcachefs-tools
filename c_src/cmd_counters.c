#include <getopt.h>

#include "cmds.h"
#include "libbcachefs.h"
#include "libbcachefs/super-io.h"

static void reset_counters_usage(void)
{
	puts("bcachefs reset-counters \n"
	     "Usage: bcachefs reset-counters device\n"
	     "\n"
	     "Options:\n"
	     "  -h, --help                  display this help and exit\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
	exit(EXIT_SUCCESS);
}

int cmd_reset_counters(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "help",			0, NULL, 'h' },
		{ NULL }
	};
	int opt;

	while ((opt = getopt_long(argc, argv, "h", longopts, NULL)) != -1)
		switch (opt) {
		case 'h':
			reset_counters_usage();
			break;
		}
	args_shift(optind);

	char *dev = arg_pop();
	if (!dev) {
		reset_counters_usage();
		die("please supply a device");
	}
	if (argc)
		die("too many arguments");

	struct bch_opts opts = bch2_opts_empty();
	struct bch_sb_handle sb;
	int ret = bch2_read_super(dev, &opts, &sb);
	if (ret)
		die("Error opening %s: %s", dev, bch2_err_str(ret));

	bch2_sb_field_resize(&sb, counters, 0);

	bch2_super_write(sb.bdev->bd_fd, sb.sb);
	bch2_free_super(&sb);
	return 0;
}
