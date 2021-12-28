/*
 * Authors: Kent Overstreet <kent.overstreet@gmail.com>
 *	    Gabriel de Perthuis <g2p.code@gmail.com>
 *	    Jacob Malevich <jam@datera.io>
 *
 * GPLv2
 */
#include <ctype.h>
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

#include "ccan/darray/darray.h"

#include "cmds.h"
#include "libbcachefs.h"
#include "crypto.h"
#include "libbcachefs/opts.h"
#include "libbcachefs/super-io.h"
#include "libbcachefs/util.h"

#define OPTS						\
x(0,	replicas,		required_argument)	\
x(0,	encrypted,		no_argument)		\
x(0,	no_passphrase,		no_argument)		\
x('L',	fs_label,		required_argument)	\
x('U',	uuid,			required_argument)	\
x(0,	fs_size,		required_argument)	\
x(0,	superblock_size,	required_argument)	\
x(0,	bucket_size,		required_argument)	\
x('l',	label,			required_argument)	\
x(0,	discard,		no_argument)		\
x(0,	data_allowed,		required_argument)	\
x(0,	durability,		required_argument)	\
x(0,	version,		required_argument)	\
x(0,	no_initialize,		no_argument)		\
x('f',	force,			no_argument)		\
x('q',	quiet,			no_argument)		\
x('h',	help,			no_argument)

static void usage(void)
{
	puts("bcachefs format - create a new bcachefs filesystem on one or more devices\n"
	     "Usage: bcachefs format [OPTION]... <devices>\n"
	     "\n"
	     "Options:");

	bch2_opts_usage(OPT_FORMAT);

	puts(
	     "      --replicas=#            Sets both data and metadata replicas\n"
	     "      --encrypted             Enable whole filesystem encryption (chacha20/poly1305)\n"
	     "      --no_passphrase         Don't encrypt master encryption key\n"
	     "  -L, --fs_label=label\n"
	     "  -U, --uuid=uuid\n"
	     "      --superblock_size=size\n"
	     "\n"
	     "Device specific options:");

	bch2_opts_usage(OPT_DEVICE);

	puts("  -l, --label=label           Disk label\n"
	     "\n"
	     "  -f, --force\n"
	     "  -q, --quiet                 Only print errors\n"
	     "  -h, --help                  Display this help and exit\n"
	     "\n"
	     "Device specific options must come before corresponding devices, e.g.\n"
	     "  bcachefs format --label cache /dev/sdb /dev/sdc\n"
	     "\n"
	     "Report bugs to <linux-bcache@vger.kernel.org>");
}

enum {
	O_no_opt = 1,
#define x(shortopt, longopt, arg)	O_##longopt,
	OPTS
#undef x
};

#define x(shortopt, longopt, arg) {			\
	.name		= #longopt,			\
	.has_arg	= arg,				\
	.flag		= NULL,				\
	.val		= O_##longopt,			\
},
static const struct option format_opts[] = {
	OPTS
	{ NULL }
};
#undef x

u64 read_flag_list_or_die(char *opt, const char * const list[],
			  const char *msg)
{
	u64 v = bch2_read_flag_list(opt, list);
	if (v == (u64) -1)
		die("Bad %s %s", msg, opt);

	return v;
}

int cmd_format(int argc, char *argv[])
{
	darray(struct dev_opts) devices;
	darray(char *) device_paths;
	struct format_opts opts	= format_opts_default();
	struct dev_opts dev_opts = dev_opts_default(), *dev;
	bool force = false, no_passphrase = false, quiet = false, initialize = true;
	unsigned v;
	int opt;

	darray_init(devices);
	darray_init(device_paths);

	struct bch_opt_strs fs_opt_strs =
		bch2_cmdline_opts_get(&argc, argv, OPT_FORMAT);
	struct bch_opts fs_opts = bch2_parse_opts(fs_opt_strs);

	while ((opt = getopt_long(argc, argv,
				  "-L:U:g:fqh",
				  format_opts,
				  NULL)) != -1)
		switch (opt) {
		case O_replicas:
			if (kstrtouint(optarg, 10, &v) ||
			    !v ||
			    v > BCH_REPLICAS_MAX)
				die("invalid replicas");

			opt_set(fs_opts, metadata_replicas, v);
			opt_set(fs_opts, data_replicas, v);
			break;
		case O_encrypted:
			opts.encrypted = true;
			break;
		case O_no_passphrase:
			no_passphrase = true;
			break;
		case O_fs_label:
		case 'L':
			opts.label = optarg;
			break;
		case O_uuid:
		case 'U':
			if (uuid_parse(optarg, opts.uuid.b))
				die("Bad uuid");
			break;
		case O_force:
		case 'f':
			force = true;
			break;
		case O_fs_size:
			if (bch2_strtoull_h(optarg, &dev_opts.size))
				die("invalid filesystem size");
			break;
		case O_superblock_size:
			if (bch2_strtouint_h(optarg, &opts.superblock_size))
				die("invalid filesystem size");

			opts.superblock_size >>= 9;
			break;
		case O_bucket_size:
			if (bch2_strtoull_h(optarg, &dev_opts.bucket_size))
				die("bad bucket_size %s", optarg);
			break;
		case O_label:
		case 'l':
			dev_opts.label = optarg;
			break;
		case O_discard:
			dev_opts.discard = true;
			break;
		case O_data_allowed:
			dev_opts.data_allowed =
				read_flag_list_or_die(optarg,
					bch2_data_types, "data type");
			break;
		case O_durability:
			if (kstrtouint(optarg, 10, &dev_opts.durability) ||
			    dev_opts.durability > BCH_REPLICAS_MAX)
				die("invalid durability");
			break;
		case O_version:
			if (kstrtouint(optarg, 10, &opts.version))
				die("invalid version");
			break;
		case O_no_initialize:
			initialize = false;
			break;
		case O_no_opt:
			darray_append(device_paths, optarg);
			dev_opts.path = optarg;
			darray_append(devices, dev_opts);
			dev_opts.size = 0;
			break;
		case O_quiet:
		case 'q':
			quiet = true;
			break;
		case O_help:
		case 'h':
			usage();
			exit(EXIT_SUCCESS);
			break;
		case '?':
			exit(EXIT_FAILURE);
			break;
		}

	if (darray_empty(devices))
		die("Please supply a device");

	if (opts.encrypted && !no_passphrase) {
		opts.passphrase = read_passphrase_twice("Enter passphrase: ");
		initialize = false;
	}

	darray_foreach(dev, devices)
		dev->fd = open_for_format(dev->path, force);

	struct bch_sb *sb =
		bch2_format(fs_opt_strs,
			    fs_opts,
			    opts,
			    devices.item, darray_size(devices));
	bch2_opt_strs_free(&fs_opt_strs);

	if (!quiet)
		bch2_sb_print(sb, false, 1 << BCH_SB_FIELD_members, HUMAN_READABLE);
	free(sb);

	if (opts.passphrase) {
		memzero_explicit(opts.passphrase, strlen(opts.passphrase));
		free(opts.passphrase);
	}

	darray_free(devices);

	if (initialize) {
		/*
		 * Start the filesystem once, to allocate the journal and create
		 * the root directory:
		 */
		struct bch_fs *c = bch2_fs_open(device_paths.item,
						darray_size(device_paths),
						bch2_opts_empty());
		if (IS_ERR(c))
			die("error opening %s: %s", device_paths.item,
			    strerror(-PTR_ERR(c)));

		bch2_fs_stop(c);
	}

	darray_free(device_paths);

	return 0;
}

static void show_super_usage(void)
{
	puts("bcachefs show-super \n"
	     "Usage: bcachefs show-super [OPTION].. device\n"
	     "\n"
	     "Options:\n"
	     "  -f, --fields=(fields)       list of sections to print\n"
	     "  -l, --layout                print superblock layout\n"
	     "  -h, --help                  display this help and exit\n"
	     "Report bugs to <linux-bcache@vger.kernel.org>");
	exit(EXIT_SUCCESS);
}

int cmd_show_super(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "fields",			1, NULL, 'f' },
		{ "layout",			0, NULL, 'l' },
		{ "help",			0, NULL, 'h' },
		{ NULL }
	};
	unsigned fields = 1 << BCH_SB_FIELD_members;
	bool print_layout = false;
	int opt;

	while ((opt = getopt_long(argc, argv, "f:lh", longopts, NULL)) != -1)
		switch (opt) {
		case 'f':
			fields = !strcmp(optarg, "all")
				? ~0
				: read_flag_list_or_die(optarg,
					bch2_sb_fields, "superblock field");
			break;
		case 'l':
			print_layout = true;
			break;
		case 'h':
			show_super_usage();
			break;
		}
	args_shift(optind);

	char *dev = arg_pop();
	if (!dev)
		die("please supply a device");
	if (argc)
		die("too many arguments");

	struct bch_opts opts = bch2_opts_empty();

	opt_set(opts, noexcl,	true);
	opt_set(opts, nochanges, true);

	struct bch_sb_handle sb;
	int ret = bch2_read_super(dev, &opts, &sb);
	if (ret)
		die("Error opening %s: %s", dev, strerror(-ret));

	bch2_sb_print(sb.sb, print_layout, fields, HUMAN_READABLE);
	bch2_free_super(&sb);
	return 0;
}
