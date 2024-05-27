
#include <errno.h>
#include <getopt.h>
#include <sys/uio.h>
#include <unistd.h>
#include "cmds.h"
#include "libbcachefs/error.h"
#include "libbcachefs.h"
#include "libbcachefs/super.h"
#include "libbcachefs/super-io.h"
#include "tools-util.h"

static void fsck_usage(void)
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
	     "  -k, --kernel            Use the in-kernel fsck implementation\n"
	     "  -v                      Be verbose\n"
	     "  -h, --help              Display this help and exit\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
}

static void setnonblocking(int fd)
{
	int flags = fcntl(fd, F_GETFL);
	if (fcntl(fd, F_SETFL, flags|O_NONBLOCK))
		die("fcntl error: %m");
}

static int do_splice(int rfd, int wfd)
{
	char buf[4096], *b = buf;

	int r = read(rfd, buf, sizeof(buf));
	if (r < 0 && errno == EAGAIN)
		return 0;
	if (r < 0)
		return r;
	if (!r)
		return 1;
	do {
		ssize_t w = write(wfd, b, r);
		if (w < 0)
			die("%s: write error: %m", __func__);
		r -= w;
		b += w;
	} while (r);
	return 0;
}

static int splice_fd_to_stdinout(int fd)
{
	setnonblocking(STDIN_FILENO);
	setnonblocking(fd);

	bool stdin_closed = false;

	while (true) {
		fd_set fds;

		FD_ZERO(&fds);
		FD_SET(fd, &fds);
		if (!stdin_closed)
			FD_SET(STDIN_FILENO, &fds);

		if (select(fd + 1, &fds, NULL, NULL, NULL) < 0)
			die("select error: %m");

		int r = do_splice(fd, STDOUT_FILENO);
		if (r < 0)
			return r;
		if (r)
			break;

		r = do_splice(STDIN_FILENO, fd);
		if (r < 0)
			return r;
		if (r)
			stdin_closed = true;
	}

	return close(fd);
}

static int fsck_online(const char *dev_path)
{
	int dev_idx;
	struct bchfs_handle fs = bchu_fs_open_by_dev(dev_path, &dev_idx);

	struct bch_ioctl_fsck_online fsck = { 0 };

	int fsck_fd = ioctl(fs.ioctl_fd, BCH_IOCTL_FSCK_ONLINE, &fsck);
	if (fsck_fd < 0)
		die("BCH_IOCTL_FSCK_ONLINE error: %s", bch2_err_str(errno));

	return splice_fd_to_stdinout(fsck_fd);
}

static void append_opt(struct printbuf *out, const char *opt)
{
	if (out->pos)
		prt_char(out, ',');
	prt_str(out, opt);
}

static bool should_use_kernel_fsck(darray_str devs)
{
	unsigned kernel_version = !access("/sys/module/bcachefs/parameters/version", R_OK)
	    ? read_file_u64(AT_FDCWD, "/sys/module/bcachefs/parameters/version")
	    : 0;

	if (!kernel_version)
		return false;

	if (kernel_version == bcachefs_metadata_version_current)
		return false;

	struct bch_opts opts = bch2_opts_empty();
	opt_set(opts, nostart, true);
	opt_set(opts, noexcl, true);
	opt_set(opts, nochanges, true);
	opt_set(opts, read_only, true);

	struct bch_fs *c = bch2_fs_open(devs.data, devs.nr, opts);
	if (IS_ERR(c))
		return false;

	bool ret = ((bcachefs_metadata_version_current < kernel_version &&
		     kernel_version <= c->sb.version) ||
		    (c->sb.version <= kernel_version &&
		     kernel_version < bcachefs_metadata_version_current));

	if (ret) {
		struct printbuf buf = PRINTBUF;

		prt_str(&buf, "fsck binary is version ");
		bch2_version_to_text(&buf, bcachefs_metadata_version_current);
		prt_str(&buf, " but filesystem is ");
		bch2_version_to_text(&buf, c->sb.version);
		prt_str(&buf, " and kernel is ");
		bch2_version_to_text(&buf, kernel_version);
		prt_str(&buf, ", using kernel fsck\n");

		printf("%s", buf.buf);
		printbuf_exit(&buf);
	}

	bch2_fs_stop(c);

	return ret;
}

int cmd_fsck(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "ratelimit_errors",	no_argument,		NULL, 'r' },
		{ "reconstruct_alloc",	no_argument,		NULL, 'R' },
		{ "kernel",		no_argument,		NULL, 'k' },
		{ "no-kernel",		no_argument,		NULL, 'K' },
		{ "help",		no_argument,		NULL, 'h' },
		{ NULL }
	};
	int kernel = -1; /* unset */
	int opt, ret = 0;
	struct printbuf opts_str = PRINTBUF;

	if (getenv("BCACHEFS_KERNEL_ONLY"))
		kernel = true;

	append_opt(&opts_str, "degraded");
	append_opt(&opts_str, "fsck");
	append_opt(&opts_str, "fix_errors=ask");
	append_opt(&opts_str, "read_only");

	while ((opt = getopt_long(argc, argv,
				  "apynfo:rRkvh",
				  longopts, NULL)) != -1)
		switch (opt) {
		case 'a': /* outdated alias for -p */
		case 'p':
		case 'y':
			append_opt(&opts_str, "fix_errors=yes");
			break;
		case 'n':
			append_opt(&opts_str, "nochanges");
			append_opt(&opts_str, "fix_errors=no");
			break;
		case 'f':
			/* force check, even if filesystem marked clean: */
			break;
		case 'o':
			append_opt(&opts_str, optarg);
			break;
		case 'r':
			append_opt(&opts_str, "ratelimit_errors");
			break;
		case 'R':
			append_opt(&opts_str, "reconstruct_alloc");
			break;
		case 'k':
			kernel = true;
			break;
		case 'K':
			kernel = false;
			break;
		case 'v':
			append_opt(&opts_str, "verbose");
			break;
		case 'h':
			fsck_usage();
			exit(16);
		}
	args_shift(optind);

	if (!argc) {
		fprintf(stderr, "Please supply device(s) to check\n");
		exit(8);
	}

	darray_str devs = get_or_split_cmdline_devs(argc, argv);

	int kernel_probed = kernel;
	if (kernel_probed < 0)
		kernel_probed = should_use_kernel_fsck(devs);

	struct bch_opts opts = bch2_opts_empty();

	if (kernel_probed) {
		struct bch_ioctl_fsck_offline *fsck = calloc(sizeof(*fsck) +
							     sizeof(u64) * devs.nr, 1);

		fsck->opts = (unsigned long)opts_str.buf;
		darray_for_each(devs, i)
			fsck->devs[i - devs.data] = (unsigned long) *i;
		fsck->nr_devs = devs.nr;

		int ctl_fd = bcachectl_open();
		int fsck_fd = ioctl(ctl_fd, BCH_IOCTL_FSCK_OFFLINE, fsck);
		free(fsck);

		if (fsck_fd < 0 && kernel < 0)
			goto userland_fsck;

		if (fsck_fd < 0)
			die("BCH_IOCTL_FSCK_OFFLINE error: %s", bch2_err_str(errno));

		ret = splice_fd_to_stdinout(fsck_fd);
	} else {
userland_fsck:
		ret = bch2_parse_mount_opts(NULL, &opts, opts_str.buf);
		if (ret)
			return ret;

		darray_for_each(devs, i)
			if (dev_mounted(*i))
				return fsck_online(*i);

		struct bch_fs *c = bch2_fs_open(devs.data, devs.nr, opts);
		if (IS_ERR(c))
			exit(8);

		if (test_bit(BCH_FS_errors_fixed, &c->flags)) {
			fprintf(stderr, "%s: errors fixed\n", c->name);
			ret |= 1;
		}
		if (test_bit(BCH_FS_error, &c->flags)) {
			fprintf(stderr, "%s: still has errors\n", c->name);
			ret |= 4;
		}

		bch2_fs_stop(c);
	}

	printbuf_exit(&opts_str);
	return ret;
}
