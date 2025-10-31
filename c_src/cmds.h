/*
 * Author: Kent Overstreet <kent.overstreet@gmail.com>
 *
 * GPLv2
 */

#ifndef _CMDS_H
#define _CMDS_H

#include "tools-util.h"

int cmd_format(int argc, char *argv[]);
int cmd_show_super(int argc, char *argv[]);
int cmd_recover_super(int argc, char *argv[]);
int cmd_reset_counters(int argc, char *argv[]);
int cmd_strip_alloc(int argc, char *argv[]);
int cmd_set_option(int argc, char *argv[]);

int image_cmds(int argc, char *argv[]);

int fs_usage(void);
int cmd_fs_usage(int argc, char *argv[]);
int cmd_fs_top(int argc, char *argv[]);

int device_cmds(int argc, char *argv[]);

int cmd_scrub(int argc, char *argv[]);
int reconcile_cmds(int argc, char *argv[]);
int data_cmds(int argc, char *argv[]);

int cmd_unlock(int argc, char *argv[]);
int cmd_set_passphrase(int argc, char *argv[]);
int cmd_remove_passphrase(int argc, char *argv[]);

int cmd_fsck(int argc, char *argv[]);
int cmd_recovery_pass(int argc, char *argv[]);

int cmd_dump(int argc, char *argv[]);
int cmd_undump(int argc, char *argv[]);

int cmd_list_journal(int argc, char *argv[]);
int cmd_kill_btree_node(int argc, char *argv[]);

int cmd_migrate(int argc, char *argv[]);
int cmd_migrate_superblock(int argc, char *argv[]);

int cmd_version(int argc, char *argv[]);

int cmd_setattr(int argc, char *argv[]);

int subvolume_usage(void);
int cmd_subvolume_create(int argc, char *argv[]);
int cmd_subvolume_delete(int argc, char *argv[]);
int cmd_subvolume_snapshot(int argc, char *argv[]);

int cmd_fusemount(int argc, char *argv[]);

void bcachefs_usage(void);
int fs_cmds(int argc, char *argv[]);
int subvolume_cmds(int argc, char *argv[]);

#endif /* _CMDS_H */
