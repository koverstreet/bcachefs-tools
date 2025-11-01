
#include <getopt.h>
#include <stdio.h>
#include <sys/ioctl.h>
#include <unistd.h>

#include "bcachefs_ioctl.h"
#include "alloc/accounting.h"
#include "btree/cache.h"
#include "data/move.h"

#include "cmds.h"
#include "libbcachefs.h"

static void scrub_usage(void)
{
	puts("bcachefs scrub\n"
	     "Usage: bcachefs scrub [filesystem|device]\n"
	     "\n"
	     "Check data for errors, fix from another replica if possible\n"
	     "\n"
	     "Options:\n"
	     "  -m, --metadata               Check metadata only\n"
	     "  -h, --help                   Display this help and exit\n"
	     "\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
	exit(EXIT_SUCCESS);
}

int cmd_scrub(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "metadata",		no_argument,		NULL, 'm' },
		{ "help",		no_argument,		NULL, 'h' },
		{ NULL }
	};
	struct bch_ioctl_data cmd = {
		.op			= BCH_DATA_OP_scrub,
		.scrub.data_types	= ~0,
	};
	int ret = 0, opt;

	while ((opt = getopt_long(argc, argv, "hm", longopts, NULL)) != -1)
		switch (opt) {
		case 'm':
			cmd.scrub.data_types = BIT(BCH_DATA_btree);
			break;
		case 'h':
			scrub_usage();
			break;
		}
	args_shift(optind);

	char *path = arg_pop();
	if (!path)
		die("Please supply a filesystem");

	if (argc)
		die("too many arguments");

	printf("Starting scrub on");

	struct bchfs_handle fs = bcache_fs_open(path);
	dev_names dev_names = bchu_fs_get_devices(fs);

	struct scrub_device {
		const char	*name;
		int		progress_fd;
		u64		done, corrected, uncorrected, total;
		enum bch_ioctl_data_event_ret	ret;
	};
	DARRAY(struct scrub_device) scrub_devs = {};

	if (fs.dev_idx >= 0) {
		cmd.scrub.dev = fs.dev_idx;
		struct scrub_device d = {
			.name		= dev_idx_to_name(&dev_names, fs.dev_idx)->dev,
			.progress_fd	= xioctl(fs.ioctl_fd, BCH_IOCTL_DATA, &cmd),
		};
		darray_push(&scrub_devs, d);
	} else {
		/* Scrubbing every device */
		darray_for_each(dev_names, dev) {
			cmd.scrub.dev = dev->idx;
			struct scrub_device d = {
				.name		= dev->dev,
				.progress_fd	= xioctl(fs.ioctl_fd, BCH_IOCTL_DATA, &cmd),
			};
			darray_push(&scrub_devs, d);
		}
	}

	printf(" %zu devices: ", scrub_devs.nr);
	darray_for_each(scrub_devs, dev)
		printf(" %s", dev->name);
	printf("\n");

	struct timespec now, last;
	bool first = true;

	struct printbuf buf = PRINTBUF;
	printbuf_tabstop_push(&buf, 16);
	printbuf_tabstop_push(&buf, 12);
	printbuf_tabstop_push(&buf, 12);
	printbuf_tabstop_push(&buf, 12);
	printbuf_tabstop_push(&buf, 12);
	printbuf_tabstop_push(&buf, 6);

	prt_printf(&buf, "device\t");
	prt_printf(&buf, "checked\r");
	prt_printf(&buf, "corrected\r");
	prt_printf(&buf, "uncorrected\r");
	prt_printf(&buf, "total\r");
	puts(buf.buf);

	while (1) {
		bool done = true;

		printbuf_reset_keep_tabstops(&buf);

		clock_gettime(CLOCK_MONOTONIC, &now);
		u64 ns_since_last = 0;
		if (!first)
			ns_since_last = (now.tv_sec - last.tv_sec) * NSEC_PER_SEC +
				now.tv_nsec - last.tv_nsec;

		darray_for_each(scrub_devs, dev) {
			struct bch_ioctl_data_event e;

			if (dev->progress_fd >= 0 &&
			    read(dev->progress_fd, &e, sizeof(e)) != sizeof(e)) {
				xclose(dev->progress_fd);
				dev->progress_fd = -1;
			}

			u64 rate = 0;

			if (dev->progress_fd >= 0) {
				if (ns_since_last)
					rate = ((e.p.sectors_done - dev->done) << 9)
						* NSEC_PER_SEC
						/ ns_since_last;

				dev->done	= e.p.sectors_done;
				dev->corrected	= e.p.sectors_error_corrected;
				dev->uncorrected= e.p.sectors_error_uncorrected;
				dev->total	= e.p.sectors_total;

				if (dev->corrected)
					ret |= 2;
				if (dev->uncorrected)
					ret |= 4;
			}

			if (dev->progress_fd >= 0 && e.ret) {
				xclose(dev->progress_fd);
				dev->progress_fd = -1;
				dev->ret = e.ret;
			}

			if (dev->progress_fd >= 0)
				done = false;

			prt_printf(&buf, "%s\t", dev->name ?: "(offline)");

			prt_human_readable_u64(&buf, dev->done << 9);
			prt_tab_rjust(&buf);

			prt_human_readable_u64(&buf, dev->corrected << 9);
			prt_tab_rjust(&buf);

			prt_human_readable_u64(&buf, dev->uncorrected << 9);
			prt_tab_rjust(&buf);

			prt_human_readable_u64(&buf, dev->total << 9);
			prt_tab_rjust(&buf);

			prt_printf(&buf, "%llu%%",
				   dev->total
				   ? dev->done * 100 / dev->total
				   : 0);
			prt_tab_rjust(&buf);

			prt_str(&buf, "  ");

			if (dev->progress_fd >= 0) {
				prt_human_readable_u64(&buf, rate);
				prt_str(&buf, "/sec");
			} else if (dev->ret == BCH_IOCTL_DATA_EVENT_RET_device_offline) {
				prt_str(&buf, "offline");
			} else {
				prt_str(&buf, "complete");
			}

			if (dev != &darray_last(scrub_devs))
				prt_newline(&buf);
		}

		fputs(buf.buf, stdout);
		fflush(stdout);

		if (done)
			break;

		last = now;
		first = false;
		sleep(1);

		for (unsigned i = 0; i < scrub_devs.nr; i++) {
			if (i)
				printf("\033[1A");
			printf("\33[2K\r");
		}
	}

	fputs("\n", stdout);
	printbuf_exit(&buf);

	return ret;
}

static void reconcile_wait_usage(void)
{
	CLASS(printbuf, buf)();
	prt_bitflags(&buf, __bch2_rebalance_accounting_types, ~0UL);

	printf("bcachefs reconcile wait\n"
	     "Usage: bcachefs reconcile wait [OPTION]... <mountpoint>\n"
	     "\n"
	     "Wait for reconcile to finish background data processing of one or more types\n"
	     "\n"
	     "Options:\n"
	     "  -t, --types=TYPES            List of reconcile types to wait on\n"
	     "                               %s\n"
	     "  -h, --help                   Display this help and exit\n"
	     "\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>\n",
	     buf.buf);
	exit(EXIT_SUCCESS);
}

static bool reconcile_status(struct printbuf *out,
			     struct bchfs_handle fs,
			     unsigned types)
{
	bool scan_pending = read_file_u64(fs.sysfs_fd, "rebalance_scan_pending");

	u64 v[BCH_REBALANCE_ACCOUNTING_NR];
	memset(v, 0, sizeof(v));

	struct bch_ioctl_query_accounting *a =
		bchu_fs_accounting(fs, BIT(BCH_DISK_ACCOUNTING_rebalance_work_v2));

	/*
	 * This would be cleaner if we had an interface for doing
	 * lookups on specific keys
	 */
	for (struct bkey_i_accounting *k = a->accounting;
	     k < (struct bkey_i_accounting *) ((u64 *) a->accounting + a->accounting_u64s);
	     k = bkey_i_to_accounting(bkey_next(&k->k_i))) {
		struct disk_accounting_pos acc_k;
		bpos_to_disk_accounting_pos(&acc_k, k->k.p);

		v[acc_k.rebalance_work_v2.type] = k->v.d[0];
	}
	free(a);

	if (!out->nr_tabstops)
		printbuf_tabstop_push(out, 32);

	prt_printf(out, "Scan pending:\t%u\n", scan_pending);
	bool have_pending = scan_pending;

	for (unsigned i = 0; i < ARRAY_SIZE(v); i++)
		if (types & BIT(i)) {
			prt_printf(out, "Pending %s:\t", __bch2_rebalance_accounting_types[i]);
			prt_human_readable_u64(out, v[i] << 9);
			prt_newline(out);
			have_pending |= v[i] != 0;
		}

	return have_pending;
}

static size_t count_newlines(const char *str)
{
	size_t ret = 0;
	const char *n;
	while ((n = strchr(str, '\n'))) {
		str = n + 1;
		ret++;
	}

	return ret;
}

int cmd_reconcile_wait(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "types",		required_argument,	NULL, 't' },
		{ "help",		no_argument,		NULL, 'h' },
		{ NULL }
	};
	unsigned types = ~0U;
	int opt;

	while ((opt = getopt_long(argc, argv, "t:h", longopts, NULL)) != -1)
		switch (opt) {
		case 't':
			types = read_flag_list_or_die(optarg,
					__bch2_rebalance_accounting_types, "reconcile type");
			break;
		case 'h':
			reconcile_wait_usage();
		}
	args_shift(optind);

	char *fs_path = arg_pop();
	if (!fs_path)
		die("Please supply a filesystem");

	if (argc)
		die("too many arguments");

	struct bchfs_handle fs = bcache_fs_open(fs_path);

	while (true) {
		CLASS(printbuf, buf)();
		bool pending = reconcile_status(&buf, fs, types);

		printf("\033[%zuF\033[J", count_newlines(buf.buf));
		fputs(buf.buf, stdout);

		if (!pending)
			break;

		sleep(1);
	}

	return 0;
}

static void reconcile_status_usage(void)
{
	CLASS(printbuf, buf)();
	prt_bitflags(&buf, __bch2_rebalance_accounting_types, ~0UL);

	printf("bcachefs reconcile status\n"
	       "Usage: bcachefs reconcile status [OPTION]... <mountpoint>\n"
	       "\n"
	       "Options:\n"
	       "  -t, --types=TYPES            List of reconcile types to wait on\n"
	     "                               %s\n"
	       "  -h, --help                   Display this help and exit\n"
	       "\n"
	       "Report bugs to <linux-bcachefs@vger.kernel.org>\n",
	       buf.buf);
	exit(EXIT_SUCCESS);
}

int cmd_reconcile_status(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "types",		required_argument,	NULL, 't' },
		{ "help",		no_argument,		NULL, 'h' },
		{ NULL }
	};
	unsigned types = ~0U;
	int opt;

	while ((opt = getopt_long(argc, argv, "t:h", longopts, NULL)) != -1)
		switch (opt) {
		case 't':
			types = read_flag_list_or_die(optarg,
					__bch2_rebalance_accounting_types, "reconcile type");
			break;
		case 'h':
			reconcile_status_usage();
		}
	args_shift(optind);

	char *fs_path = arg_pop();
	if (!fs_path)
		die("Please supply a filesystem");

	if (argc)
		die("too many arguments");

	struct bchfs_handle fs = bcache_fs_open(fs_path);

	CLASS(printbuf, buf)();
	reconcile_status(&buf, fs, types);
	fputs(buf.buf, stdout);

	return 0;
}

static int reconcile_usage(void)
{
	puts("bcachefs reconcile - manage data reconcile\n"
	     "Usage: bcachefs reconcile <CMD> [OPTIONS]\n"
	     "\n"
	     "Commands:\n"
	     "  status                       Show status of background data processing\n"
	     "  wait                         Wait on background data processing to complete\n"
	     "\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
	return 0;
}

int reconcile_cmds(int argc, char *argv[])
{
	char *cmd = pop_cmd(&argc, argv);

	if (argc < 1)
		return reconcile_usage();
	if (!strcmp(cmd, "status"))
		return cmd_reconcile_status(argc, argv);
	if (!strcmp(cmd, "wait"))
		return cmd_reconcile_wait(argc, argv);

	reconcile_usage();
	return -EINVAL;
}

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

static int data_usage(void)
{
	puts("bcachefs data - manage filesystem data\n"
	     "Usage: bcachefs data <CMD> [OPTIONS]\n"
	     "\n"
	     "Commands:\n"
	     "  rereplicate                  Rereplicate degraded data\n"
	     "\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
	return 0;
}

int data_cmds(int argc, char *argv[])
{
	char *cmd = pop_cmd(&argc, argv);

	if (argc < 1)
		return data_usage();
	if (!strcmp(cmd, "rereplicate"))
		return cmd_data_rereplicate(argc, argv);

	data_usage();
	return -EINVAL;
}
