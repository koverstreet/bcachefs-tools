#include <errno.h>
#include <fcntl.h>
#include <getopt.h>
#include <unistd.h>
#include <uuid/uuid.h>

#include "cmds.h"
#include "libbcachefs/checksum.h"
#include "crypto.h"
#include "libbcachefs.h"
#include "tools-util.h"

static void unlock_usage(void)
{
	puts("bcachefs unlock - unlock an encrypted filesystem so it can be mounted\n"
	     "Usage: bcachefs unlock [OPTION] device\n"
	     "\n"
	     "Options:\n"
	     "  -c, --check            Check if a device is encrypted\n"
	     "  -k, --keyring (session|user|user_session)\n"
	     "                         Keyring to add to (default: user)\n"
	     "  -f, --file             Passphrase file to read from (disables passphrase prompt)\n"
	     "  -h, --help             Display this help and exit\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
}

int cmd_unlock(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "check",		no_argument,		NULL, 'c' },
		{ "keyring",		required_argument,	NULL, 'k' },
		{ "file",		required_argument,	NULL, 'f' },
		{ "help",		no_argument,		NULL, 'h' },
		{ NULL }
	};
	const char *keyring = "user";
	bool check = false;
	const char *passphrase_file_path = NULL;
	char *passphrase = NULL;

	int opt;
	while ((opt = getopt_long(argc, argv, "cf:k:h", longopts, NULL)) != -1)
		switch (opt) {
		case 'c':
			check = true;
			break;
		case 'k':
			keyring = strdup(optarg);
			break;
		case 'f':
			passphrase_file_path = strdup(optarg);
			break;
		case 'h':
			unlock_usage();
			exit(EXIT_SUCCESS);
		}
	args_shift(optind);

	char *dev = arg_pop();
	if (!dev) {
		unlock_usage();
		die("Please supply a device");
	}

	if (argc)
		die("Too many arguments");

	struct bch_opts opts = bch2_opts_empty();

	opt_set(opts, noexcl, true);
	opt_set(opts, nochanges, true);

	struct bch_sb_handle sb;
	int ret = bch2_read_super(dev, &opts, &sb);
	if (ret)
		die("Error opening %s: %s", dev, bch2_err_str(ret));

	if (!bch2_sb_is_encrypted(sb.sb))
		die("%s is not encrypted", dev);

	if (check)
		exit(EXIT_SUCCESS);
	if (passphrase_file_path){
		passphrase = read_file_str(AT_FDCWD, passphrase_file_path);
	} else {
		passphrase = read_passphrase("Enter passphrase: ");
	}

	bch2_add_key(sb.sb, "user", keyring, passphrase);

	bch2_free_super(&sb);
	memzero_explicit(passphrase, strlen(passphrase));
	free(passphrase);
	return 0;
}

static void set_passphrase_usage(void)
{
	puts("bcachefs set-passphares - change passphrase on an encrypted filesystem\n"
	     "Usage: bcachefs set-passphares device\n"
	     "\n"
	     "Options:\n"
	     "  -h, --help             Display this help and exit\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
}

int cmd_set_passphrase(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "help",		no_argument,		NULL, 'h' },
		{ NULL }
	};

	int opt;
	while ((opt = getopt_long(argc, argv, "h", longopts, NULL)) != -1)
		switch (opt) {
		case 'h':
			set_passphrase_usage();
			exit(EXIT_SUCCESS);
		}
	args_shift(optind);

	if (!argc) {
		set_passphrase_usage();
		die("Please supply one or more devices");
	}

	darray_const_str devs = get_or_split_cmdline_devs(argc, argv);

	struct bch_opts opts = bch2_opts_empty();
	opt_set(opts, nostart, true);

	/*
	 * we use bch2_fs_open() here, instead of just reading the superblock,
	 * to make sure we're opening and updating every component device:
	 */

	struct bch_fs *c = bch2_fs_open(&devs, &opts);
	if (IS_ERR(c))
		die("Error opening %s: %s", argv[1], bch2_err_str(PTR_ERR(c)));

	struct bch_sb *sb = c->disk_sb.sb;
	struct bch_sb_field_crypt *crypt = bch2_sb_field_get(sb, crypt);
	if (!crypt)
		die("Filesystem does not have encryption enabled");

	struct bch_key key;
	int ret = bch2_decrypt_sb_key(c, crypt, &key);
	if (ret)
		die("Error getting current key");

	char *new_passphrase = read_passphrase_twice("Enter new passphrase: ");

	bch_crypt_update_passphrase(sb, crypt, &key, new_passphrase);

	bch2_revoke_key(c->disk_sb.sb);
	bch2_write_super(c);
	bch2_fs_stop(c);
	return 0;
}

static void remove_passphrase_usage(void)
{
	puts("bcachefs remove-passphares - remove passphrase on an encrypted filesystem\n"
	     "Usage: bcachefs remove-passphares device\n"
	     "\n"
	     "Options:\n"
	     "  -h, --help             Display this help and exit\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
}

int cmd_remove_passphrase(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "help",		no_argument,		NULL, 'h' },
		{ NULL }
	};

	int opt;
	while ((opt = getopt_long(argc, argv, "h", longopts, NULL)) != -1)
		switch (opt) {
		case 'h':
			set_passphrase_usage();
			exit(EXIT_SUCCESS);
		}
	args_shift(optind);

	if (!argc) {
		set_passphrase_usage();
		die("Please supply one or more devices");
	}

	darray_const_str devs = get_or_split_cmdline_devs(argc, argv);

	struct bch_opts opts = bch2_opts_empty();
	opt_set(opts, nostart, true);

	struct bch_fs *c = bch2_fs_open(&devs, &opts);
	if (IS_ERR(c))
		die("Error opening %s: %s", argv[1], bch2_err_str(PTR_ERR(c)));

	struct bch_sb *sb = c->disk_sb.sb;
	struct bch_sb_field_crypt *crypt = bch2_sb_field_get(sb, crypt);
	if (!crypt)
		die("Filesystem does not have encryption enabled");

	struct bch_key key;
	int ret = bch2_decrypt_sb_key(c, crypt, &key);
	if (ret)
		die("Error getting current key");

	bch_crypt_update_passphrase(sb, crypt, &key, NULL);

	bch2_write_super(c);
	bch2_fs_stop(c);
	return 0;
}
