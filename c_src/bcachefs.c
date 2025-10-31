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
	     "  recover-super            Attempt to recover overwritten superblock from backups\n"
	     "  set-fs-option            Set a filesystem option\n"
	     "  reset-counters           Reset all counters on an unmounted device\n"
	     "  strip-alloc              Strip alloc info on a filesystem to be used read-only\n"
	     "\n"
	     "Commands for managing images:\n"
	     "  image create             Create a new compact disk image\n"
	     "  image update             Sync an image with a directory tree\n"
	     "\n"
	     "Mount:\n"
	     "  mount                    Mount a filesystem\n"
	     "\n"
	     "Repair:\n"
	     "  fsck                     Check an existing filesystem for errors\n"
	     "  recovery-pass            Schedule or deschedule recovery passes\n"
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
	     "  fs top                   Show runtime performance information\n"
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
	     "  reconcile status         Show status of background data processing\n"
	     "  reconcile wait           Wait for background data processing (of a specified type) to complete\n"
	     "  scrub                    Verify checksums and correct errors, if possible\n"
	     "\n"
	     "Commands for managing filesystem data (obsolete):\n"
	     "  data rereplicate         Rereplicate degraded data\n"
	     "  data scrub               Verify checksums and correct errors, if possible\n"
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
	     "  set-file-option          Set various attributes on files or directories\n"
	     "\n"
	     "Debug:\n"
	     "These commands work on offline, unmounted filesystems\n"
	     "  dump                     Dump filesystem metadata to a qcow2 image\n"
	     "  undump                   Convert qcow2 metadata dumps to sparse raw files\n"
	     "  list                     List filesystem metadata in textual form\n"
	     "  list_journal             List contents of journal\n"
	     "\n"
#ifdef BCACHEFS_FUSE
	     "FUSE:\n"
	     "  fusemount                Mount a filesystem via FUSE\n"
	     "\n"
#endif
	     "Miscellaneous:\n"
         "  completions              Generate shell completions\n"
	     "  version                  Display the version of the invoked bcachefs tool\n");
}
