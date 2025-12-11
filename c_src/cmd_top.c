#include <dirent.h>
#include <getopt.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/types.h>

#include "cmds.h"
#include "libbcachefs.h"

#include "sb/counters.h"

static const u8 counters_to_stable_map[] = {
#define x(n, id, ...)	[BCH_COUNTER_##n] = BCH_COUNTER_STABLE_##n,
	BCH_PERSISTENT_COUNTERS()
#undef x
};

static struct bch_ioctl_query_counters *read_counters(struct bchfs_handle fs, unsigned flags)
{
	struct bch_ioctl_query_counters *ret =
		kzalloc(sizeof(*ret) + sizeof(ret->d[0]) * BCH_COUNTER_NR, GFP_KERNEL);

	ret->nr		= BCH_COUNTER_NR;
	ret->flags	= flags;

	xioctl(fs.ioctl_fd, BCH_IOCTL_QUERY_COUNTERS, ret);
	return ret;
}

static void prt_counter(struct printbuf *out, u64 v,
			bool human_readable,
			enum bch_counters_flags flags)
{
	if (flags & TYPE_SECTORS)
		v <<= 9;

	prt_char(out, ' ');

	if (human_readable)
		prt_human_readable_u64(out, v);
	else
		prt_u64(out, v);

	if (flags & TYPE_SECTORS)
		prt_char(out, 'B');
}

static void fs_top(const char *path, bool human_readable)
{
	struct bchfs_handle fs = bcache_fs_open(path);

	struct bch_ioctl_query_counters *mount = read_counters(fs, BCH_IOCTL_QUERY_COUNTERS_MOUNT);
	struct bch_ioctl_query_counters *start = read_counters(fs, 0);
	struct bch_ioctl_query_counters *curr = read_counters(fs, 0);
	struct bch_ioctl_query_counters *prev = NULL;

	unsigned interval_secs = 1;

	while (true) {
		sleep(interval_secs);
		kfree(prev);
		prev = curr;
		curr = read_counters(fs, 0);

		CLASS(printbuf, buf)();
		/* clear terminal, move cursor to top */
		prt_printf(&buf, "\033[2J");
		prt_printf(&buf, "\033[H");

		printbuf_tabstop_start(&buf);

		prt_printf(&buf,
			   "All counters have a corresponding tracepoint; for more info on any given event, try e.g.\n"
			   "  perf trace -e bcachefs:data_update_pred\n"
			   "\n");

		prt_printf(&buf, "\t%us\ttotal\tmount", interval_secs);
		prt_newline(&buf);

		for (unsigned i = 0; i < BCH_COUNTER_NR; i++) {
			unsigned stable = counters_to_stable_map[i];

			u64 v1 = stable < curr->nr
				? curr->d[stable] - prev->d[stable]
				: 0;

			u64 v2 = stable < curr->nr
				? curr->d[stable] - start->d[stable]
				: 0;

			u64 v3 = curr->d[stable] - mount->d[stable];
			if (!v3)
				continue;

			prt_printf(&buf, "%s\t", bch2_counter_names[i]);

			prt_counter(&buf, v1 / interval_secs, human_readable, bch2_counter_flags[i]);
			prt_str(&buf, "/sec");
			prt_printf(&buf, "\t");

			prt_counter(&buf, v2, human_readable, bch2_counter_flags[i]);
			prt_printf(&buf, "\t");

			prt_counter(&buf, v3, human_readable, bch2_counter_flags[i]);

			prt_newline(&buf);
		}

		printbuf_tabstop_finish(&buf);
		write(STDOUT_FILENO, buf.buf, buf.pos);
		/* XXX: include btree cache size, key cache size, total ram size */
	}

	bcache_fs_close(fs);
}

static void fs_top_usage(void)
{
	puts("bcachefs fs top - display runtime perfomance info\n"
	     "Usage: bcachefs fs top [OPTION]... <mountpoint>\n"
	     "\n"
	     "Options:\n"
	     "  -h, --human-readable         Human readable units\n"
	     "  -H, --help                   Display this help and exit\n"
	     "\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
}

int cmd_fs_top(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "help",		no_argument,		NULL, 'H' },
		{ "human-readable",     no_argument,            NULL, 'h' },
		{ NULL }
	};
	bool human_readable = false;
	int opt;

	while ((opt = getopt_long(argc, argv, "Hh",
				  longopts, NULL)) != -1)
		switch (opt) {
		case 'h':
			human_readable = true;
			break;
		case 'H':
			fs_top_usage();
			exit(EXIT_SUCCESS);
		default:
			fs_top_usage();
			exit(EXIT_FAILURE);
		}
	args_shift(optind);

	fs_top(arg_pop() ?: ".", human_readable) ;
	return 0;
}
