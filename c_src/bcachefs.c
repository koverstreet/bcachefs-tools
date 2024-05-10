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

void bcachefs_usage(void)
{
	puts("bcachefs - tool for managing bcachefs filesystems\n"
	     "usage: bcachefs <command> [<args>]\n"
	     "\n"
	     "Superblock commands:\n"
	     "  format                   Format a new filesystem\n"
	     "  show-super               Dump superblock information to stdout\n"
	     "  set-option               Set a filesystem option\n"
	     "  reset-counters           Reset all counters on an unmounted device\n"
	     "\n"
	     "Mount:\n"
	     "  mount                    Mount a filesystem\n"
	     "\n"
	     "Repair:\n"
	     "  fsck                     Check an existing filesystem for errors\n"
	     "\n"
#if 0
	     "Startup/shutdown, assembly of multi device filesystems:\n"
	     "  assemble                 Assemble an existing multi device filesystem\n"
	     "  incremental              Incrementally assemble an existing multi device filesystem\n"
	     "  run                      Start a partially assembled filesystem\n"
	     "  stop	                 Stop a running filesystem\n"
	     "\n"
#endif
	     "Commands for managing a running filesystem:\n"
	     "  fs usage                 Show disk usage\n"
	     "\n"
	     "Commands for managing devices within a running filesystem:\n"
	     "  device add               Add a new device to an existing filesystem\n"
	     "  device remove            Remove a device from an existing filesystem\n"
	     "  device online            Re-add an existing member to a filesystem\n"
	     "  device offline           Take a device offline, without removing it\n"
	     "  device evacuate          Migrate data off of a specific device\n"
	     "  device set-state         Mark a device as failed\n"
	     "  device resize            Resize filesystem on a device\n"
	     "  device resize-journal    Resize journal on a device\n"
	     "\n"
	     "Commands for managing subvolumes and snapshots:\n"
	     "  subvolume create         Create a new subvolume\n"
	     "  subvolume delete         Delete an existing subvolume\n"
	     "  subvolume snapshot       Create a snapshot\n"
	     "\n"
	     "Commands for managing filesystem data:\n"
	     "  data rereplicate         Rereplicate degraded data\n"
	     "  data job                 Kick off low level data jobs\n"
	     "\n"
	     "Encryption:\n"
	     "  unlock                   Unlock an encrypted filesystem prior to running/mounting\n"
	     "  set-passphrase           Change passphrase on an existing (unmounted) filesystem\n"
	     "  remove-passphrase        Remove passphrase on an existing (unmounted) filesystem\n"
	     "\n"
	     "Migrate:\n"
	     "  migrate                  Migrate an existing filesystem to bcachefs, in place\n"
	     "  migrate-superblock       Add default superblock, after bcachefs migrate\n"
	     "\n"
	     "Commands for operating on files in a bcachefs filesystem:\n"
	     "  setattr                  Set various per file attributes\n"
	     "\n"
	     "Debug:\n"
	     "These commands work on offline, unmounted filesystems\n"
	     "  dump                     Dump filesystem metadata to a qcow2 image\n"
	     "  list                     List filesystem metadata in textual form\n"
	     "  list_journal             List contents of journal\n"
	     "\n"
	     "FUSE:\n"
	     "  fusemount                Mount a filesystem via FUSE\n"
	     "\n"
	     "Miscellaneous:\n"
	     "  completions              Generate shell completions\n"
	     "  version                  Display the version of the invoked bcachefs tool\n");
}

static char *pop_cmd(int *argc, char *argv[])
{
	char *cmd = argv[1];
	if (!(*argc < 2))
		memmove(&argv[1], &argv[2], (*argc - 2) * sizeof(argv[0]));
	(*argc)--;
	argv[*argc] = NULL;

	return cmd;
}

int fs_cmds(int argc, char *argv[])
{
	char *cmd = pop_cmd(&argc, argv);

	if (argc < 1) {
		bcachefs_usage();
		exit(EXIT_FAILURE);
	}
	if (!strcmp(cmd, "usage"))
		return cmd_fs_usage(argc, argv);

	return 0;
}

int device_cmds(int argc, char *argv[])
{
	char *cmd = pop_cmd(&argc, argv);

	if (argc < 1)
		return device_usage();
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
	if (!strcmp(cmd, "resize-journal"))
		return cmd_device_resize_journal(argc, argv);

	return 0;
}

int data_cmds(int argc, char *argv[])
{
	char *cmd = pop_cmd(&argc, argv);

	if (argc < 1)
		return data_usage();
	if (!strcmp(cmd, "rereplicate"))
		return cmd_data_rereplicate(argc, argv);
	if (!strcmp(cmd, "job"))
		return cmd_data_job(argc, argv);

	return 0;
}

int subvolume_cmds(int argc, char *argv[])
{
	char *cmd = pop_cmd(&argc, argv);
	if (argc < 1)
		return subvolume_usage();
	if (!strcmp(cmd, "create"))
		return cmd_subvolume_create(argc, argv);
	if (!strcmp(cmd, "delete"))
		return cmd_subvolume_delete(argc, argv);
	if (!strcmp(cmd, "snapshot"))
		return cmd_subvolume_snapshot(argc, argv);

	return 0;
}

int main(int argc, char *argv[])
{
	raid_init();

	char *full_cmd = argv[0];

	/* Are we being called via a symlink? */

	if (strstr(full_cmd, "mkfs"))
		return cmd_format(argc, argv);

	if (strstr(full_cmd, "fsck"))
		return cmd_fsck(argc, argv);

#ifdef BCACHEFS_FUSE
	if (strstr(full_cmd, "mount.fuse"))
		return cmd_fusemount(argc, argv);
#endif

	if (strstr(full_cmd, "mount"))
		return cmd_mount(argc, argv);

	setvbuf(stdout, NULL, _IOLBF, 0);

	char *cmd = pop_cmd(&argc, argv);
	if (!cmd) {
		puts("missing command\n");
		goto usage;
	}

	/* these subcommands display usage when argc < 2 */
	if (!strcmp(cmd, "device"))
		return device_cmds(argc, argv);
	if (!strcmp(cmd, "fs"))
		return fs_cmds(argc, argv);
	if (!strcmp(cmd, "data"))
		return data_cmds(argc, argv);
	if (!strcmp(cmd, "subvolume"))
		return subvolume_cmds(argc, argv);
	if (!strcmp(cmd, "format"))
		return cmd_format(argc, argv);
	if (!strcmp(cmd, "fsck"))
		return cmd_fsck(argc, argv);
	if (!strcmp(cmd, "version"))
		return cmd_version(argc, argv);
	if (!strcmp(cmd, "show-super"))
		return cmd_show_super(argc, argv);
	if (!strcmp(cmd, "set-option"))
		return cmd_set_option(argc, argv);
	if (!strcmp(cmd, "reset-counters"))
		return cmd_reset_counters(argc, argv);

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
#if 0
	if (!strcmp(cmd, "list"))
		return cmd_list(argc, argv);
#endif
	if (!strcmp(cmd, "list_journal"))
		return cmd_list_journal(argc, argv);

	if (!strcmp(cmd, "setattr"))
		return cmd_setattr(argc, argv);

#if 0
	if (!strcmp(cmd, "completions"))
		return cmd_completions(argc, argv);
#endif

#ifdef BCACHEFS_FUSE
	if (!strcmp(cmd, "fusemount"))
		return cmd_fusemount(argc, argv);
#endif

	if (!strcmp(cmd, "mount"))
		return cmd_mount(argc, argv);

	if (!strcmp(cmd, "--help")) {
		bcachefs_usage();
		return 0;
	}

	printf("Unknown command %s\n", cmd);
usage:
	bcachefs_usage();
	exit(EXIT_FAILURE);
}
