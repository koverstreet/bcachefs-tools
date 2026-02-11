/*
 * Author: Kent Overstreet <kent.overstreet@gmail.com>
 *
 * GPLv2
 */

#ifndef _CMDS_H
#define _CMDS_H

#include "tools-util.h"

int cmd_format(int argc, char *argv[]);
int cmd_strip_alloc(int argc, char *argv[]);

int image_cmds(int argc, char *argv[]);

int device_cmds(int argc, char *argv[]);

int cmd_dump(int argc, char *argv[]);

int cmd_list_journal(int argc, char *argv[]);
int cmd_kill_btree_node(int argc, char *argv[]);

int cmd_migrate(int argc, char *argv[]);
int cmd_migrate_superblock(int argc, char *argv[]);

int cmd_version(int argc, char *argv[]);

int cmd_fusemount(int argc, char *argv[]);

void bcachefs_usage(void);

#endif /* _CMDS_H */
