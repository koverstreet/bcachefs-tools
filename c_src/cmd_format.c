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

#include "cmds.h"
#include "cmd_super.h"
#include "tools-util.h"
#include "posix_to_bcachefs.h"
#include "libbcachefs.h"
#include "crypto.h"
#include "libbcachefs/errcode.h"
#include "libbcachefs/opts.h"
#include "libbcachefs/super-io.h"
#include "libbcachefs/util.h"

#include "libbcachefs/darray.h"

#define OPTS						\
x(0,	replicas,		required_argument)	\
x(0,	encrypted,		no_argument)		\
x(0,	passphrase_file,	required_argument)	\
x(0,	no_passphrase,		no_argument)		\
x('L',	fs_label,		required_argument)	\
x('U',	uuid,			required_argument)	\
x(0,	fs_size,		required_argument)	\
x(0,	superblock_size,	required_argument)	\
x('l',	label,			required_argument)	\
x(0,	version,		required_argument)	\
x(0,	no_initialize,		no_argument)		\
x(0,	source,			required_argument)	\
x('f',	force,			no_argument)		\
x('q',	quiet,			no_argument)		\
x('v',	verbose,		no_argument)		\
x('h',	help,			no_argument)

static void format_usage(void)
{
	puts("bcachefs format - create a new bcachefs filesystem on one or more devices\n"
	     "Usage: bcachefs format [OPTION]... <devices>\n"
	     "\n"
	     "Options:");

	bch2_opts_usage(OPT_FORMAT|OPT_FS);

	puts("      --replicas=#            Sets both data and metadata replicas\n"
	     "      --encrypted             Enable whole filesystem encryption (chacha20/poly1305)\n"
	     "      --passphrase_file=file  File containing passphrase used for encryption/decryption\n"
	     "      --no_passphrase         Don't encrypt master encryption key\n"
	     "  -L, --fs_label=label\n"
	     "  -U, --uuid=uuid\n"
	     "      --superblock_size=size\n"
	     "      --version=version       Create filesystem with specified on disk format version instead of the latest\n"
	     "      --source=path           Initialize the bcachefs filesystem from this root directory\n"
	     "\n"
	     "Device specific options:");

	bch2_opts_usage(OPT_FORMAT|OPT_DEVICE);

	puts("      --fs_size=size          Size of filesystem on device\n"
	     "  -l, --label=label           Disk label\n"
	     "\n"
	     "  -f, --force\n"
	     "  -q, --quiet                 Only print errors\n"
	     "  -v, --verbose               Verbose filesystem initialization\n"
	     "  -h, --help                  Display this help and exit\n"
	     "\n"
	     "Device specific options must come before corresponding devices, e.g.\n"
	     "  bcachefs format --label cache /dev/sdb /dev/sdc\n"
	     "\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
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

static int build_fs(struct bch_fs *c, const char *src_path)
{
	struct copy_fs_state s = {};
	int src_fd = xopen(src_path, O_RDONLY|O_NOATIME);

	return copy_fs(c, &s, src_fd, src_path);
}

int cmd_format(int argc, char *argv[])
{
	dev_opts_list devices = {};
	darray_const_str device_paths = {};
	struct format_opts opts	= format_opts_default();
	struct dev_opts dev_opts = dev_opts_default();
	bool force = false, no_passphrase = false, quiet = false, initialize = true, verbose = false;
	bool unconsumed_dev_option = false;
	unsigned v;

	struct bch_opt_strs fs_opt_strs = {};
	struct bch_opts fs_opts = bch2_opts_empty();

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

			if (opt->flags & OPT_DEVICE) {
				bch2_opt_set_by_id(&dev_opts.opts, id, v);
				unconsumed_dev_option = true;
			} else if (opt->flags & OPT_FS) {
				bch2_opt_set_by_id(&fs_opts, id, v);
			} else {
				die("got bch_opt of wrong type %s", opt->attr.name);
			}

			continue;
		}

		int optid = getopt_long(argc, argv,
					"-L:l:U:g:fqhv",
					format_opts,
					NULL);
		if (optid == -1)
			break;

		switch (optid) {
		case O_replicas:
			if (kstrtouint(optarg, 10, &v) ||
			    !v ||
			    v > BCH_REPLICAS_MAX)
				die("invalid replicas");

			opt_set(fs_opts, metadata_replicas, v);
			opt_set(fs_opts, data_replicas, v);
			break;
		case O_source:
			opts.source = optarg;
			break;
		case O_encrypted:
			opts.encrypted = true;
			break;
		case O_passphrase_file:
			opts.passphrase_file = optarg;
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
			if (bch2_strtoull_h(optarg, &dev_opts.fs_size))
				die("invalid filesystem size");
			unconsumed_dev_option = true;
			break;
		case O_superblock_size:
			if (bch2_strtouint_h(optarg, &opts.superblock_size))
				die("invalid filesystem size");

			opts.superblock_size >>= 9;
			break;
		case O_label:
		case 'l':
			dev_opts.label = optarg;
			unconsumed_dev_option = true;
			break;
		case O_version:
			opts.version = version_parse(optarg);
			break;
		case O_no_initialize:
			initialize = false;
			break;
		case O_no_opt:
			darray_push(&device_paths, optarg);
			dev_opts.path = optarg;
			darray_push(&devices, dev_opts);
			dev_opts.fs_size = 0;
			unconsumed_dev_option = false;
			break;
		case O_quiet:
		case 'q':
			quiet = true;
			break;
		case 'v':
			verbose = true;
			break;
		case O_help:
		case 'h':
			format_usage();
			exit(EXIT_SUCCESS);
			break;
		case '?':
			exit(EXIT_FAILURE);
			break;
		default:
			die("getopt ret %i %c", optid, optid);
		}
	}

	if (unconsumed_dev_option)
		die("Options for devices apply to subsequent devices; got a device option with no device");

	if (!devices.nr) {
		format_usage();
		die("Please supply a device");
	}

	if (opts.source && !initialize)
		die("--source, --no_initialize are incompatible");

	if (opts.passphrase_file && !opts.encrypted)
		die("--passphrase_file, requires --encrypted set");

	if (opts.passphrase_file && no_passphrase) {
		die("--passphrase_file, --no_passphrase are incompatible");
	}

	if (opts.encrypted && !no_passphrase) {
		if (opts.passphrase_file) {
			opts.passphrase =  read_file_str(AT_FDCWD, opts.passphrase_file);
		} else {
			opts.passphrase = read_passphrase_twice("Enter passphrase: ");
		}
		initialize = false;
	}

	if (!opts.source) {
		if (getenv("BCACHEFS_KERNEL_ONLY"))
			initialize = false;

		if (opts.version != bcachefs_metadata_version_current) {
			printf("version mismatch, not initializing");
			if (opts.source)
				die("--source, --version are incompatible");
			initialize = false;
		}
	}

	darray_for_each(devices, dev) {
		int ret = open_for_format(dev, 0, force);
		if (ret)
			die("Error opening %s: %s", dev->path, strerror(-ret));
	}

	struct bch_sb *sb = bch2_format(fs_opt_strs, fs_opts, opts, devices);

	if (!quiet) {
		struct printbuf buf = PRINTBUF;
		buf.human_readable_units = true;

		bch2_sb_to_text_with_names(&buf, sb, false, 1 << BCH_SB_FIELD_members_v2, -1);
		printf("%s", buf.buf);
		printbuf_exit(&buf);
	}
	free(sb);

	if (opts.passphrase) {
		memzero_explicit(opts.passphrase, strlen(opts.passphrase));
		free(opts.passphrase);
	}

	if (initialize) {
		/*
		 * Start the filesystem once, to allocate the journal and create
		 * the root directory:
		 */
		struct bch_opts open_opts = bch2_opts_empty();
		struct bch_fs *c = bch2_fs_open(&device_paths, &open_opts);
		if (IS_ERR(c))
			die("error opening %s: %s", device_paths.data[0],
			    bch2_err_str(PTR_ERR(c)));

		if (opts.source)
			build_fs(c, opts.source);

		bch2_fs_stop(c);
	}
	bch2_opt_strs_free(&fs_opt_strs);
	darray_exit(&devices);
	darray_exit(&device_paths);
	return 0;
}
