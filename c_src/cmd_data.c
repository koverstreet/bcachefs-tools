
#include <getopt.h>
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

static void data_scrub_usage(void)
{
	puts("bcachefs data scrub\n"
	     "Usage: bcachefs data scrub [filesystem|device]\n"
	     "\n"
	     "Check data for errors, fix from another replica if possible\n"
	     "\n"
	     "Options:\n"
	     "  -m, --metadata              check metadata only\n"
	     "  -h, --help                  display this help and exit\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
	exit(EXIT_SUCCESS);
}

int cmd_data_scrub(int argc, char *argv[])
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
	int opt;

	while ((opt = getopt_long(argc, argv, "hm", longopts, NULL)) != -1)
		switch (opt) {
		case 'm':
			cmd.scrub.data_types = BIT(BCH_DATA_btree);
			break;
		case 'h':
			data_scrub_usage();
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
				close(dev->progress_fd);
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
			}

			if (dev->progress_fd >= 0 && e.ret) {
				close(dev->progress_fd);
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

	return 0;
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
