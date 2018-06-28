/*
 * Authors: Kent Overstreet <kent.overstreet@gmail.com>
 *	    Gabriel de Perthuis <g2p.code@gmail.com>
 *	    Jacob Malevich <jam@datera.io>
 *
 * GPLv2
 */
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

#define OPTS									\
t("bcachefs format - create a new bcachefs filesystem on one or more devices")	\
t("Usage: bcachefs format [OPTION]... <devices>")				\
t("")										\
x('b',	block_size,		"size",			NULL)			\
x(0,	btree_node_size,	"size",			"Default 256k")		\
x(0,	metadata_checksum_type,	"(none|crc32c|crc64)",	NULL)			\
x(0,	data_checksum_type,	"(none|crc32c|crc64)",	NULL)			\
x(0,	compression_type,	"(none|lz4|gzip)",	NULL)			\
x(0,	background_compression_type,	"(none|lz4|gzip)",	NULL)		\
x(0,	replicas,		"#",			NULL)			\
x(0,	data_replicas,		"#",			NULL)			\
x(0,	metadata_replicas,	"#",			NULL)			\
x(0,	foreground_target,	"target",		NULL)			\
x(0,	background_target,	"target",		NULL)			\
x(0,	promote_target,		"target",		NULL)			\
x(0,	encrypted,		NULL,			"Enable whole filesystem encryption (chacha20/poly1305)")\
x(0,	no_passphrase,		NULL,			"Don't encrypt master encryption key")\
x('e',	error_action,		"(continue|remount-ro|panic)", NULL)		\
x('L',	label,			"label",		NULL)			\
x('U',	uuid,			"uuid",			NULL)			\
x('f',	force,			NULL,			NULL)			\
t("")										\
t("Device specific options:")							\
x(0,	fs_size,		"size",			"Size of filesystem on device")\
x(0,	bucket_size,		"size",			"Bucket size")		\
x('g',	group,			"label",		"Disk group")\
x(0,	discard,		NULL,			NULL)			\
x(0,	data_allowed,		"journal,btree,data",	"Allowed types of data on this device")\
x(0,	durability,		"#",			"Number of times data written to this device will have been considered replicated")\
t("Device specific options must come before corresponding devices, e.g.")	\
t("  bcachefs format --group cache /dev/sdb --tier 1 /dev/sdc")			\
t("")										\
x('q',	quiet,			NULL,			"Only print errors")	\
x('h',	help,			NULL,			"Display this help and exit")

static void usage(void)
{
#define t(text)				puts(text "\n")
#define x(shortopt, longopt, arg, help) do {				\
	OPTS
#undef x
#undef t

	puts("bcachefs format - create a new bcachefs filesystem on one or more devices\n"
	     "Usage: bcachefs format [OPTION]... <devices>\n"
	     "\n"
	     "Options:\n"
	     "  -b, --block=size\n"
	     "      --btree_node=size       Btree node size, default 256k\n"
	     "      --metadata_checksum_type=(none|crc32c|crc64)\n"
	     "      --data_checksum_type=(none|crc32c|crc64)\n"
	     "      --compression_type=(none|lz4|gzip|zstd)\n"
	     "      --background_compression_type=(none|lz4|gzip|zstd)\n"
	     "      --data_replicas=#       Number of data replicas\n"
	     "      --metadata_replicas=#   Number of metadata replicas\n"
	     "      --replicas=#            Sets both data and metadata replicas\n"
	     "      --foreground_target=target\n"
	     "      --background_target=target\n"
	     "      --promote_target=target\n"
	     "      --encrypted             Enable whole filesystem encryption (chacha20/poly1305)\n"
	     "      --no_passphrase         Don't encrypt master encryption key\n"
	     "      --error_action=(continue|remount-ro|panic)\n"
	     "                              Action to take on filesystem error\n"
	     "  -L, --label=label\n"
	     "  -U, --uuid=uuid\n"
	     "  -f, --force\n"
	     "\n"
	     "Device specific options:\n"
	     "      --fs_size=size          Size of filesystem on device\n"
	     "      --bucket_size=size      Bucket size\n"
	     "      --discard               Enable discards\n"
	     "      --data_allowed=journal,btree,data\n"
	     "                              Allowed types of data on this device\n"
	     "      --durability=#          Device durability (0-4)\n"
	     "  -g, --group=label           Disk group\n"
	     "\n"
	     "  -q, --quiet                 Only print errors\n"
	     "  -h, --help                  Display this help and exit\n"
	     "\n"
	     "Device specific options must come before corresponding devices, e.g.\n"
	     "  bcachefs format --group cache /dev/sdb --tier 1 /dev/sdc\n"
	     "\n"
	     "Report bugs to <linux-bcache@vger.kernel.org>");
}

enum {
	O_no_opt = 1,
#define t(text)
#define x(shortopt, longopt, arg, help)	O_##longopt,
	OPTS
#undef x
#undef t
};

static const struct option format_opts[] = {
#define t(text)
#define x(shortopt, longopt, arg, help)	{				\
	.name		= #longopt,					\
	.has_arg	= arg ? required_argument : no_argument,	\
	.flag		= NULL,						\
	.val		= O_##longopt,					\
},
	OPTS
#undef x
#undef t
	{ NULL }
};

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
	struct format_opts opts	= format_opts_default();
	struct dev_opts dev_opts = dev_opts_default(), *dev;
	bool force = false, no_passphrase = false, quiet = false;
	int opt;

	darray_init(devices);

	while ((opt = getopt_long(argc, argv,
				  "-b:e:L:U:ft:qh",
				  format_opts,
				  NULL)) != -1)
		switch (opt) {
		case O_block_size:
		case 'b':
			opts.block_size =
				hatoi_validate(optarg, "block size");
			break;
		case O_btree_node_size:
			opts.btree_node_size =
				hatoi_validate(optarg, "btree node size");
			break;
		case O_metadata_checksum_type:
			opts.meta_csum_type =
				read_string_list_or_die(optarg,
						bch2_csum_types, "checksum type");
			break;
		case O_data_checksum_type:
			opts.data_csum_type =
				read_string_list_or_die(optarg,
						bch2_csum_types, "checksum type");
			break;
		case O_compression_type:
			opts.compression_type =
				read_string_list_or_die(optarg,
						bch2_compression_types,
						"compression type");
			break;
		case O_background_compression_type:
			opts.background_compression_type =
				read_string_list_or_die(optarg,
						bch2_compression_types,
						"compression type");
			break;
		case O_data_replicas:
			if (kstrtouint(optarg, 10, &opts.data_replicas) ||
			    !opts.data_replicas ||
			    opts.data_replicas > BCH_REPLICAS_MAX)
				die("invalid replicas");
			break;
		case O_metadata_replicas:
			if (kstrtouint(optarg, 10, &opts.meta_replicas) ||
			    !opts.meta_replicas ||
			    opts.meta_replicas > BCH_REPLICAS_MAX)
				die("invalid replicas");
			break;
		case O_replicas:
			if (kstrtouint(optarg, 10, &opts.data_replicas) ||
			    !opts.data_replicas ||
			    opts.data_replicas > BCH_REPLICAS_MAX)
				die("invalid replicas");
			opts.meta_replicas = opts.data_replicas;
			break;
		case O_foreground_target:
			opts.foreground_target = strdup(optarg);
			break;
		case O_background_target:
			opts.background_target = strdup(optarg);
			break;
		case O_promote_target:
			opts.promote_target = strdup(optarg);
			break;
		case O_encrypted:
			opts.encrypted = true;
			break;
		case O_no_passphrase:
			no_passphrase = true;
			break;
		case O_error_action:
		case 'e':
			opts.on_error_action =
				read_string_list_or_die(optarg,
						bch2_error_actions, "error action");
			break;
		case O_label:
		case 'L':
			opts.label = strdup(optarg);
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

			dev_opts.size >>= 9;
			break;
		case O_bucket_size:
			dev_opts.bucket_size =
				hatoi_validate(optarg, "bucket size");
			break;
		case O_group:
		case 'g':
			dev_opts.group = strdup(optarg);
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
		case O_no_opt:
			dev_opts.path = strdup(optarg);
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
		}

	if (!darray_size(devices))
		die("Please supply a device");

	if (opts.encrypted && !no_passphrase)
		opts.passphrase = read_passphrase_twice("Enter passphrase: ");

	darray_foreach(dev, devices)
		dev->fd = open_for_format(dev->path, force);

	struct bch_sb *sb =
		bch2_format(opts, devices.item, darray_size(devices));

	if (!quiet)
		bch2_sb_print(sb, false, 1 << BCH_SB_FIELD_members, HUMAN_READABLE);
	free(sb);

	if (opts.passphrase) {
		memzero_explicit(opts.passphrase, strlen(opts.passphrase));
		free(opts.passphrase);
	}

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
