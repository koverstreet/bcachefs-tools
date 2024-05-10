#include <ctype.h>
#include <errno.h>
#include <fcntl.h>
#include <getopt.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

#include <uuid/uuid.h>
#include <blkid/blkid.h>
#include <linux/mount.h>

#include "cmds.h"
#include "libbcachefs.h"
#include "crypto.h"
#include "tools-util.h"
#include "libbcachefs/errcode.h"
#include "libbcachefs/opts.h"
#include "libbcachefs/super-io.h"
#include "libbcachefs/util.h"
#include "libbcachefs/darray.h"

typedef enum {
	POLICY_FAIL,
	POLICY_WAIT,
	POLICY_ASK,
} unlock_policy;

typedef struct {
	const char *name;
	unsigned int mask;
} option_flags;

static const option_flags mount_opt_flags[] = {
	{"rw",		0U},
	{"ro",		MS_RDONLY},
	{"nosuid",	MS_NOSUID},
	{"nodev",	MS_NODEV},
	{"noexec",	MS_NOEXEC},
	{"sync",	MS_SYNCHRONOUS},
	{"remount",	MS_REMOUNT},
	{"mand",	MS_MANDLOCK},
	{"dirsync",	MS_DIRSYNC},
	{"noatime",	MS_NOATIME},
	{"nodiratime",	MS_NODIRATIME},
	{"relatime",	MS_RELATIME},
	{"strictatime",	MS_STRICTATIME},
	{"lazytime",	MS_LAZYTIME},
};
static const int flag_count = sizeof(mount_opt_flags) / sizeof(mount_opt_flags[0]);

static const char *fs_type = "bcachefs";
static int verbose = 0;

static void mount_usage(void)
{
	puts("bcachefs mount - filesystem mount\n"
	     "Usage: bcachefs mount [options] device mountpoint\n"
	     "\n"
	     "Options:\n"
	     "  -o, --options\n"
	     "      Mount options provided as a comma-separated list. See user guide for complete list.\n"
	     "           degraded   Allow mounting with data degraded\n"
	     "           verbose    Extra debugging info during mount/recovery\n"
	     "           fsck       Run fsck during mount\n"
	     "           fix_errors Fix errors without asking during fsck\n"
	     "           read_only  Mount in read only mode\n"
	     "           version_upgrade\n"
	     "  -f, --passphrase_file\n"
	     "      Passphrase file to read from (disables passphrase prompt)\n"
	     "  -k, --key-location=(fail | wait | ask)\n"
	     "      How the password would be loaded. (default: ask).\n"
	     "          fail    don't ask for password, fail if filesystem is encrypted.\n"
	     "          wait    wait for password to become available before mounting.\n"
	     "          ask     prompt the user for password.\n"
	     "  -v, --verbose\n"
	     "      Be verbose. Can be specified more than once.");
}

/* Parse a comma-separated mount options and split out mountflags and filesystem specific options. */
static unsigned int parse_mount_options(const char *_opts, char **mount_options)
{
	unsigned int flag = 0U;
	int i;
	char *opts, *orig, *s, *remain = NULL;

	opts = orig = xstrdup(_opts);
	*mount_options = xmalloc(strlen(orig) + 1);

	while ((s = strsep(&opts, ","))) {
		i = 0;
		for (;;) {
			if (!strcmp(s, mount_opt_flags[i].name)) {
				flag |= mount_opt_flags[i].mask;
				break;
			}
			i++;
			if (i == flag_count) {
				if (!remain) {
					remain = *mount_options;
				} else {
					*remain++ = ',';
				}
				strcpy(remain, s);
				remain += strlen(s);
			}
		}
	}

	free(orig);
	return flag;
}

static char * get_name_from_uuid(const char *uuid)
{
	blkid_cache cache = NULL;
	blkid_dev_iterate iter;
	blkid_dev dev;
	darray_str devs = { 0 };
	int ret;
	int len = 0;
	char *dev_name, *s;

	if ((ret = blkid_get_cache(&cache, NULL)) != 0) {
		die("error creating blkid cache (%d)", ret);
	}

	iter = blkid_dev_iterate_begin(cache);
	blkid_dev_set_search(iter, "UUID", uuid);
	while (blkid_dev_next(iter, &dev) == 0) {
		const char *name = blkid_dev_devname(dev);
		const char *type = blkid_get_tag_value (cache, "TYPE", name);
		if (!strcmp(type, fs_type)) {
			len += strlen(name) + 1;
			darray_push(&devs, xstrdup(name));
		}
	}
	blkid_dev_iterate_end(iter);

	if (!len)
		die("no device found");

	dev_name = s = xmalloc(len);
	darray_for_each(devs, i) {
		char *p = *i;
		strcpy(s, p);
		len = strlen(p);
		free(p);
		s += len;
		*s = ':';
		s++;
	}
	s--;
	*s = '\0';

	darray_exit(&devs);

	return dev_name;
}

static void unlock_super(const char *devs_str, const char *passphrase_file, unlock_policy policy)
{
	// get the first dev
	char *dev = xstrdup(devs_str);
	char *sep = strchr(dev, ':');
	if (sep)
		*sep = '\0';

	// Check if the filesystem's master key is encrypted
	struct bch_opts opts = bch2_opts_empty();
	opt_set(opts, noexcl, true);
	opt_set(opts, nochanges, true);

	struct bch_sb_handle sb;
	int ret = bch2_read_super(dev, &opts, &sb);
	if (ret)
		die("Error opening %s: %s", dev, bch2_err_str(ret));

	if (bch2_sb_is_encrypted_and_locked(sb.sb)) {
		char *passphrase = NULL;
		// First by password_file, if available
		if (passphrase_file)
			passphrase = read_file_str(AT_FDCWD, passphrase_file);
		else if (policy == policy_ask)
			passphrase = read_passphrase("Enter passphrase: ");
		if (passphrase) {
			bch2_add_key(sb.sb, "user", "user", passphrase);
			bch2_free_super(&sb);
			memzero_explicit(passphrase, strlen(passphrase));
			free(passphrase);
			printf("superblock unlocked: %s\n", dev);
		} else {
			bch2_free_super(&sb);
			die("Failed to decrypt file system");
		}
	} else
		bch2_free_super(&sb);

	free(dev);
}

int cmd_mount(int argc, char *argv[]){
	static const struct option long_opts[] = {
		{"passphrase_file",	optional_argument,	NULL,	'f'},
		{"key_location",	required_argument,	NULL,	'k'},
		{"options",	required_argument,	NULL,	'o'},
		{"verbose",	no_argument,		&verbose,	1},
		{NULL}
	};
	int opt;
	unlock_policy policy = POLICY_ASK;
	unsigned int mount_flags = 0U;
	const char *passphrase_file = NULL;
	char *mount_options = NULL;
	char *devs_str;
	const char *mount_point;

	while ((opt = getopt_long(argc,argv,"f:k:o:v",long_opts,NULL)) != -1)
		switch (opt) {
		case 'f':
			passphrase_file = optarg;
			break;
		case 'k':
			if (!strcmp(optarg,"fail"))
				policy = POLICY_FAIL;
			else if (!strcmp_prefix(optarg,"wait"))
				policy = POLICY_WAIT;
			else if (!strcmp(optarg,"ask"))
				policy = POLICY_ASK;
			else {
				mount_usage();
				exit(16);
			}
			break;
		case 'o':
			mount_flags = parse_mount_options(optarg, &mount_options);
			break;
		case 'v':
			verbose = 1;
			break;
		default:
			mount_usage();
			exit(16);
		}

	args_shift(optind);

	if (argc != 2) {
		mount_usage();
		exit(8);
	}

	mount_point = argv[1];

	if (!strncmp(argv[0], "UUID=", 5))
		devs_str = get_name_from_uuid(argv[0] + 5);
	else if (!strncmp(argv[0], "OLD_BLKID_UUID=", 15))
		devs_str = get_name_from_uuid(argv[0] + 15);
	else
		devs_str = xstrdup(argv[0]);

	unlock_super(devs_str, passphrase_file, policy);

	printf("mounting devices %s to %s\n", devs_str, mount_point);
	int ret = mount(devs_str, mount_point, fs_type, mount_flags, mount_options);
	if (ret)
		die("mount failed: %s", strerror(ret));

	free(devs_str);
	return 0;
}
