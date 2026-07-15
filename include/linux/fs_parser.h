/* SPDX-License-Identifier: GPL-2.0-or-later */
/* Filesystem parameter description and parser
 *
 * Copyright (C) 2018 Red Hat, Inc. All Rights Reserved.
 * Written by David Howells (dhowells@redhat.com)
 */

#ifndef _LINUX_FS_PARSER_H
#define _LINUX_FS_PARSER_H

struct constant_table {
	const char	*name;
	int		value;
};

extern int lookup_constant(const struct constant_table tbl[], const char *name, int not_found);

extern const struct constant_table bool_names[];

#endif /* _LINUX_FS_PARSER_H */
