/*
 * Authors: Kent Overstreet <kent.overstreet@gmail.com>
 *	    Gabriel de Perthuis <g2p.code@gmail.com>
 *	    Jacob Malevich <jam@datera.io>
 *
 * GPLv2
 */

#include <stdlib.h>
#include <stdio.h>
#include <ctype.h>
#include <errno.h>
#include <inttypes.h>
#include <limits.h>
#include <fcntl.h>
#include <unistd.h>
#include <stdbool.h>
#include <stdint.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/types.h>
#include <sys/stat.h>

#include <raid/raid.h>

#include "cmds.h"

static void usage(void)
{
	puts("bcachefs - tool for managing bcachefs filesystems\n"
	     "usage: bcachefs <command> [<args>]\n"
	     "\n"
	     "Superblock commands:\n"
	     "  format               Format a new filesystem\n"
	     "  show-super           Dump superblock information to stdout\n"
	     "\n"
	     "Repair:\n"
	     "  fsck                 Check an existing filesystem for errors\n"
	     "\n"
	     "Startup/shutdown, assembly of multi device filesystems:\n"
#if 0
	     "  assemble             Assemble an existing multi device filesystem\n"
	     "  incremental          Incrementally assemble an existing multi device filesystem\n"
	     "  run                  Start a partially assembled filesystem\n"
	     "  stop	               Stop a running filesystem\n"
#endif
	     "\n"
	     "Commands for managing a running filesystem:\n"
	     "  fs usage             Show disk usage\n"
	     "\n"
	     "Commands for managing devices within a running filesystem:\n"
	     "  device add           Add a new device to an existing filesystem\n"
	     "  device remove        Remove a device from an existing filesystem\n"
	     "  device online        Re-add an existing member to a filesystem\n"
	     "  device offline       Take a device offline, without removing it\n"
	     "  device evacuate      Migrate data off of a specific device\n"
	     "  device set-state     Mark a device as failed\n"
	     "  device resize        Resize filesystem on a device\n"
	     "\n"
	     "Commands for managing filesystem data:\n"
	     "  data rereplicate     Rereplicate degraded data\n"
	     "\n"
	     "Encryption:\n"
	     "  unlock               Unlock an encrypted filesystem prior to running/mounting\n"
	     "  set-passphrase       Change passphrase on an existing (unmounted) filesystem\n"
	     "  remove-passphrase    Remove passphrase on an existing (unmounted) filesystem\n"
	     "\n"
	     "Migrate:\n"
	     "  migrate              Migrate an existing filesystem to bcachefs, in place\n"
	     "  migrate-superblock   Add default superblock, after bcachefs migrate\n"
	     "\n"
	     "Commands for operating on files in a bcachefs filesystem:\n"
	     "  setattr              Set various per file attributes\n"
	     "Debug:\n"
	     "These commands work on offline, unmounted filesystems\n"
	     "  dump                 Dump filesystem metadata to a qcow2 image\n"
	     "  list                 List filesystem metadata in textual form\n"
	     "\n"
	     "Miscellaneous:\n"
	     "  version              Display the version of the invoked bcachefs tool\n");
}

static char *full_cmd;

static char *pop_cmd(int *argc, char *argv[])
{
	if (*argc < 2) {
		printf("%s: missing command\n", argv[0]);
		usage();
		exit(EXIT_FAILURE);
	}

	char *cmd = argv[1];
	memmove(&argv[1], &argv[2], *argc * sizeof(argv[0]));
	(*argc)--;

	full_cmd = mprintf("%s %s", full_cmd, cmd);
	return cmd;
}

static int fs_cmds(int argc, char *argv[])
{
	char *cmd = pop_cmd(&argc, argv);

	if (!strcmp(cmd, "usage"))
		return cmd_fs_usage(argc, argv);

	usage();
	return 0;
}

static int device_cmds(int argc, char *argv[])
{
	char *cmd = pop_cmd(&argc, argv);

	if (!strcmp(cmd, "add"))
		return cmd_device_add(argc, argv);
	if (!strcmp(cmd, "remove"))
		return cmd_device_remove(argc, argv);
	if (!strcmp(cmd, "online"))
		return cmd_device_online(argc, argv);
	if (!strcmp(cmd, "offline"))
		return cmd_device_offline(argc, argv);
	if (!strcmp(cmd, "evacuate"))
		return cmd_device_evacuate(argc, argv);
	if (!strcmp(cmd, "set-state"))
		return cmd_device_set_state(argc, argv);
	if (!strcmp(cmd, "resize"))
		return cmd_device_resize(argc, argv);

	usage();
	return 0;
}

static int data_cmds(int argc, char *argv[])
{
	char *cmd = pop_cmd(&argc, argv);

	if (!strcmp(cmd, "rereplicate"))
		return cmd_data_rereplicate(argc, argv);

	usage();
	return 0;
}

int main(int argc, char *argv[])
{
	raid_init();

	full_cmd = argv[0];

	setvbuf(stdout, NULL, _IOLBF, 0);

	char *cmd = pop_cmd(&argc, argv);

	if (!strcmp(cmd, "version"))
		return cmd_version(argc, argv);
	if (!strcmp(cmd, "format"))
		return cmd_format(argc, argv);
	if (!strcmp(cmd, "show-super"))
		return cmd_show_super(argc, argv);

	if (!strcmp(cmd, "fsck"))
		return cmd_fsck(argc, argv);

#if 0
	if (!strcmp(cmd, "assemble"))
		return cmd_assemble(argc, argv);
	if (!strcmp(cmd, "incremental"))
		return cmd_incremental(argc, argv);
	if (!strcmp(cmd, "run"))
		return cmd_run(argc, argv);
	if (!strcmp(cmd, "stop"))
		return cmd_stop(argc, argv);
#endif

	if (!strcmp(cmd, "fs"))
		return fs_cmds(argc, argv);

	if (!strcmp(cmd, "device"))
		return device_cmds(argc, argv);

	if (!strcmp(cmd, "data"))
		return data_cmds(argc, argv);

	if (!strcmp(cmd, "unlock"))
		return cmd_unlock(argc, argv);
	if (!strcmp(cmd, "set-passphrase"))
		return cmd_set_passphrase(argc, argv);
	if (!strcmp(cmd, "remove-passphrase"))
		return cmd_remove_passphrase(argc, argv);

	if (!strcmp(cmd, "migrate"))
		return cmd_migrate(argc, argv);
	if (!strcmp(cmd, "migrate-superblock"))
		return cmd_migrate_superblock(argc, argv);

	if (!strcmp(cmd, "dump"))
		return cmd_dump(argc, argv);
	if (!strcmp(cmd, "list"))
		return cmd_list(argc, argv);

	if (!strcmp(cmd, "setattr"))
		return cmd_setattr(argc, argv);

	if (!strcmp(cmd, "fusemount"))
		return cmd_fusemount(argc, argv);

	if (!strcmp(cmd, "--help")) {
		usage();
		return 0;
	}

	printf("Unknown command %s\n", cmd);
	usage();
	exit(EXIT_FAILURE);
}
