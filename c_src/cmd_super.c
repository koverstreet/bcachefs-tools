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
#include "libbcachefs.h"
#include "libbcachefs/opts.h"
#include "libbcachefs/super-io.h"
#include "libbcachefs/util.h"

#include "libbcachefs/darray.h"

#include "src/rust_to_c.h"

static void show_super_usage(void)
{
	puts("bcachefs show-super \n"
	     "Usage: bcachefs show-super [OPTION].. device\n"
	     "\n"
	     "Options:\n"
	     "  -f, --fields=(fields)       list of sections to print\n"
	     "      --field-only=fiel)      print superblock section only, no header\n"
	     "  -l, --layout                print superblock layout\n"
	     "  -h, --help                  display this help and exit\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
	exit(EXIT_SUCCESS);
}

static struct sb_name *sb_dev_to_name(sb_names sb_names, unsigned idx)
{
	darray_for_each(sb_names, i)
		if (i->sb.sb->dev_idx == idx)
			return i;
	return NULL;
}

static void print_one_member(struct printbuf *out, sb_names sb_names,
			     struct bch_sb *sb,
			     struct bch_sb_field_disk_groups *gi,
			     struct bch_member m, unsigned idx)
{
	struct sb_name *name = sb_dev_to_name(sb_names, idx);
	prt_printf(out, "Device %u:\t%s\t", idx, name ? name->name : "(not found)");

	if (name) {
		char *model = fd_to_dev_model(name->sb.bdev->bd_fd);
		prt_str(out, model);
		free(model);
	}
	prt_newline(out);

	printbuf_indent_add(out, 2);
	bch2_member_to_text(out, &m, gi, sb, idx);
	printbuf_indent_sub(out, 2);
}

void bch2_sb_to_text_with_names(struct printbuf *out, struct bch_sb *sb,
				bool print_layout, unsigned fields, int field_only)
{
	CLASS(printbuf, uuid_buf)();
	prt_str(&uuid_buf, "UUID=");
	pr_uuid(&uuid_buf, sb->user_uuid.b);

	sb_names sb_names = {};
	bch2_scan_device_sbs(uuid_buf.buf, &sb_names);

	if (field_only >= 0) {
		struct bch_sb_field *f = bch2_sb_field_get_id(sb, field_only);

		if (f)
			__bch2_sb_field_to_text(out, sb, f);
	} else {
		printbuf_tabstop_push(out, 44);

		bch2_sb_to_text(out, sb, print_layout,
				fields & ~(BIT(BCH_SB_FIELD_members_v1)|
					   BIT(BCH_SB_FIELD_members_v2)));

		struct bch_sb_field_disk_groups *gi = bch2_sb_field_get(sb, disk_groups);

		struct bch_sb_field_members_v1 *mi1;
		if ((fields & BIT(BCH_SB_FIELD_members_v1)) &&
		    (mi1 = bch2_sb_field_get(sb, members_v1)))
			for (unsigned i = 0; i < sb->nr_devices; i++)
				print_one_member(out, sb_names, sb, gi, bch2_members_v1_get(mi1, i), i);

		struct bch_sb_field_members_v2 *mi2;
		if ((fields & BIT(BCH_SB_FIELD_members_v2)) &&
		    (mi2 = bch2_sb_field_get(sb, members_v2)))
			for (unsigned i = 0; i < sb->nr_devices; i++)
				print_one_member(out, sb_names, sb, gi, bch2_members_v2_get(mi2, i), i);
	}
}

int cmd_show_super(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "fields",			1, NULL, 'f' },
		{ "field-only",			1, NULL, 'F' },
		{ "layout",			0, NULL, 'l' },
		{ "help",			0, NULL, 'h' },
		{ NULL }
	};
	unsigned fields = 0;
	int field_only = -1;
	bool print_layout = false;
	bool print_default_fields = true;
	int opt;

	while ((opt = getopt_long(argc, argv, "f:lh", longopts, NULL)) != -1)
		switch (opt) {
		case 'f':
			fields = !strcmp(optarg, "all")
				? ~0
				: read_flag_list_or_die(optarg,
					bch2_sb_fields, "superblock field");
			print_default_fields = false;
			break;
		case 'F':
			field_only = read_string_list_or_die(optarg,
					bch2_sb_fields, "superblock field");
			print_default_fields = false;
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
	if (!dev) {
		show_super_usage();
		die("please supply a device");
	}
	if (argc)
		die("too many arguments");

	struct bch_opts opts = bch2_opts_empty();

	opt_set(opts, noexcl,	true);
	opt_set(opts, nochanges, true);
	opt_set(opts, no_version_check, true);

	struct bch_sb_handle sb;
	int ret = bch2_read_super(dev, &opts, &sb);
	if (ret)
		die("Error opening %s: %s", dev, bch2_err_str(ret));

	if (print_default_fields) {
		fields |= bch2_sb_field_get(sb.sb, members_v2)
			? BIT(BCH_SB_FIELD_members_v2)
			: BIT(BCH_SB_FIELD_members_v1);
		fields |= BIT(BCH_SB_FIELD_errors);
	}

	struct printbuf buf = PRINTBUF;

	buf.human_readable_units = true;

	bch2_sb_to_text_with_names(&buf, sb.sb, print_layout, fields, field_only);
	printf("%s", buf.buf);

	bch2_free_super(&sb);
	printbuf_exit(&buf);
	return 0;
}

#include "libbcachefs/super-io.h"
#include "libbcachefs/sb-members.h"

typedef DARRAY(struct bch_sb *) probed_sb_list;

struct recover_super_args {
	u64		dev_size;
	u64		offset;
	u64		scan_len;

	const char	*src_device;
	int		dev_idx;

	bool		yes;
	bool		verbose;

	const char	*dev_path;
};

static void probe_one_super(int dev_fd, unsigned sb_size, u64 offset,
			    probed_sb_list *sbs, bool verbose)
{
	darray_char sb_buf = {};
	darray_resize(&sb_buf, sb_size);

	xpread(dev_fd, sb_buf.data, sb_buf.size, offset);

	struct printbuf err = PRINTBUF;
	struct bch_opts opts = bch2_opts_empty();
	int ret = bch2_sb_validate((void *) sb_buf.data, &opts, offset >> 9, 0, &err);
	printbuf_exit(&err);

	if (!ret) {
		if (verbose) {
			struct printbuf buf = PRINTBUF;
			prt_human_readable_u64(&buf, offset);
			printf("found superblock at %s\n", buf.buf);
			printbuf_exit(&buf);
		}

		darray_push(sbs, (void *) sb_buf.data);
		sb_buf.data = NULL;
	}

	darray_exit(&sb_buf);
}

static void probe_sb_range(int dev_fd, u64 start_offset, u64 end_offset,
			   probed_sb_list *sbs, bool verbose)
{
	start_offset	&= ~((u64) 511);
	end_offset	&= ~((u64) 511);

	size_t buflen = end_offset - start_offset;
	void *buf = malloc(buflen);
	xpread(dev_fd, buf, buflen, start_offset);

	for (u64 offset = 0; offset < buflen; offset += 512) {
		struct bch_sb *sb = buf + offset;

		if (!uuid_equal(&sb->magic, &BCACHE_MAGIC) &&
		    !uuid_equal(&sb->magic, &BCHFS_MAGIC))
			continue;

		size_t bytes = vstruct_bytes(sb);
		if (offset + bytes > buflen) {
			fprintf(stderr, "found sb %llu size %zu that overran buffer\n",
				start_offset + offset, bytes);
			continue;
		}
		struct printbuf err = PRINTBUF;
		struct bch_opts opts = bch2_opts_empty();
		int ret = bch2_sb_validate(sb, &opts, (start_offset + offset) >> 9, 0, &err);
		if (ret)
			fprintf(stderr, "found sb %llu that failed to validate: %s\n",
				start_offset + offset, err.buf);
		printbuf_exit(&err);

		if (ret)
			continue;

		if (verbose) {
			struct printbuf buf = PRINTBUF;
			prt_human_readable_u64(&buf, start_offset + offset);
			printf("found superblock at %s\n", buf.buf);
			printbuf_exit(&buf);
		}

		void *sb_copy = malloc(bytes);
		memcpy(sb_copy, sb, bytes);
		darray_push(sbs, sb_copy);
	}

	free(buf);
}

static u64 bch2_sb_last_mount_time(struct bch_sb *sb)
{
	u64 ret = 0;
	for (unsigned i = 0; i < sb->nr_devices; i++)
		ret = max(ret, le64_to_cpu(bch2_sb_member_get(sb, i).last_mount));
	return ret;
}

static int bch2_sb_time_cmp(struct bch_sb *l, struct bch_sb *r)
{
	return cmp_int(bch2_sb_last_mount_time(l),
		       bch2_sb_last_mount_time(r));
}

static struct bch_sb *recover_super_from_scan(struct recover_super_args args, int dev_fd)
{
	probed_sb_list sbs = {};

	if (args.offset) {
		probe_one_super(dev_fd, SUPERBLOCK_SIZE_DEFAULT, args.offset, &sbs, args.verbose);
	} else {
		probe_sb_range(dev_fd, 4096, args.scan_len, &sbs, args.verbose);
		probe_sb_range(dev_fd, args.dev_size - args.scan_len, args.dev_size, &sbs, args.verbose);
	}

	if (!sbs.nr) {
		printf("Found no bcachefs superblocks\n");
		exit(EXIT_FAILURE);
	}

	struct bch_sb *best = NULL;
	darray_for_each(sbs, sb)
		if (!best || bch2_sb_time_cmp(best, *sb) < 0)
			best = *sb;

	darray_for_each(sbs, sb)
		if (*sb == best)
			*sb = NULL;

	darray_for_each(sbs, sb)
		kfree(*sb);
	darray_exit(&sbs);
	return best;
}

static struct bch_sb *recover_super_from_member(struct recover_super_args args)
{
	struct bch_opts opts = bch2_opts_empty();
	opt_set(opts, noexcl,		true);
	opt_set(opts, nochanges,	true);

	struct bch_sb_handle src_sb;
	int ret = bch2_read_super(args.src_device, &opts, &src_sb);
	if (ret)
		die("Error opening %s: %s", args.src_device, bch2_err_str(ret));

	if (!bch2_member_exists(src_sb.sb, args.dev_idx))
		die("Member %u does not exist in source superblock", args.dev_idx);

	bch2_sb_field_delete(&src_sb, BCH_SB_FIELD_journal);
	bch2_sb_field_delete(&src_sb, BCH_SB_FIELD_journal_v2);
	src_sb.sb->dev_idx = args.dev_idx;

	struct bch_sb *sb = src_sb.sb;
	src_sb.sb = NULL;

	bch2_free_super(&src_sb);

	struct bch_member m = bch2_sb_member_get(sb, args.dev_idx);

	bch2_sb_layout_init(&sb->layout,
			    le16_to_cpu(sb->block_size) << 9,
			    BCH_MEMBER_BUCKET_SIZE(&m) << 9,
			    1U << sb->layout.sb_max_size_bits,
			    BCH_SB_SECTOR,
			    args.dev_size >> 9,
			    false);

	return sb;
}

static void recover_super_usage(void)
{
	puts("bcachefs recover-super \n"
	     "Usage: bcachefs recover-super [OPTION].. device\n"
	     "\n"
	     "Attempt to recover a filesystem on a device that has had the main superblock\n"
	     "and superblock layout overwritten.\n"
	     "All options will be guessed if not provided\n"
	     "\n"
	     "Options:\n"
	     "  -d, --dev_size              size of filessytem on device, in bytes \n"
	     "  -o, --offset                offset to probe, in bytes\n"
	     "  -l, --scan_len              Length in bytes to scan from start and end of device\n"
	     "                              Should be >= bucket size to find sb at end of device\n"
	     "                              Default 16M\n"
	     "  -s, --src_device            member device to recover from, in a multi device fs\n"
	     "  -i, --dev_idx               index of this device, if recovering from another device\n"
	     "  -y, --yes                   Recover without prompting\n"
	     "  -v, --verbose               Increase logging level\n"
	     "  -h, --help                  display this help and exit\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
	exit(EXIT_SUCCESS);
}

int cmd_recover_super(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "dev_size",			required_argument, NULL, 'd' },
		{ "offset",			required_argument, NULL, 'o' },
		{ "scan_len",			required_argument, NULL, 'l' },
		{ "src_device",			required_argument, NULL, 's' },
		{ "dev_idx",			required_argument, NULL, 'i' },
		{ "yes",			no_argument, NULL, 'y' },
		{ "verbose",			no_argument, NULL, 'v' },
		{ "help",			no_argument, NULL, 'h' },
		{ NULL }
	};
	struct recover_super_args args = {
		.scan_len	= 16 << 20,
		.dev_idx	= -1,
	};
	int opt;

	while ((opt = getopt_long(argc, argv, "d:o:yvh", longopts, NULL)) != -1)
		switch (opt) {
		case 'd':
			if (bch2_strtoull_h(optarg, &args.dev_size))
				die("invalid dev_size");
			break;
		case 'o':
			if (bch2_strtoull_h(optarg, &args.offset))
				die("invalid offset");

			if (args.offset & 511)
				die("offset must be a multiple of 512");
			break;
		case 'l':
			if (bch2_strtoull_h(optarg, &args.scan_len))
				die("invalid scan_len");
			break;
		case 's':
			args.src_device = strdup(optarg);
			break;
		case 'i':
			if (kstrtoint(optarg, 10, &args.dev_idx) ||
			    args.dev_idx < 0)
				die("invalid dev_idx");
			break;
		case 'y':
			args.yes = true;
			break;
		case 'v':
			args.verbose = true;
			break;
		case 'h':
			recover_super_usage();
			break;
		}
	args_shift(optind);

	if (args.src_device && args.dev_idx == -1)
		die("--src_device requires --dev_idx");

	if (args.dev_idx >= 0 && !args.src_device)
		die("--dev_idx requires --src_device");

	char *dev_path = arg_pop();
	if (!dev_path) {
		recover_super_usage();
		die("please supply a device");
	}
	if (argc)
		die("too many arguments");

	int dev_fd = xopen(dev_path, O_RDWR);

	if (!args.dev_size)
		args.dev_size = get_size(dev_fd);

	struct bch_sb *sb = !args.src_device
		? recover_super_from_scan(args, dev_fd)
		: recover_super_from_member(args);

	struct printbuf buf = PRINTBUF;
	bch2_sb_to_text(&buf, sb, true, BIT_ULL(BCH_SB_FIELD_members_v2));

	printf("Found superblock:\n%s\n", buf.buf);

	if (args.yes)
		printf("Recovering\n");
	else
		printf("Recover? ");

	if (args.yes || ask_yn())
		bch2_super_write(dev_fd, sb);

	if (args.src_device)
		printf("Recovered device will no longer have a journal, please run fsck\n");

	printbuf_exit(&buf);
	kvfree(sb);
	xclose(dev_fd);
	return 0;
}
