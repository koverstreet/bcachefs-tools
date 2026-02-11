/*
 * Authors: Kent Overstreet <kent.overstreet@gmail.com>
 *	    Gabriel de Perthuis <g2p.code@gmail.com>
 *	    Jacob Malevich <jam@datera.io>
 *
 * GPLv2
 */
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

#include "cmd_super.h"
#include "libbcachefs.h"

#include "bcachefs.h"

#include "sb/io.h"
#include "sb/members.h"

#include "util/darray.h"

#include "src/rust_to_c.h"

static struct sb_name *sb_dev_to_name(sb_names sb_names, unsigned idx)
{
	darray_for_each(sb_names, i)
		if (i->sb.sb->dev_idx == idx)
			return i;
	return NULL;
}

static void print_one_member(struct printbuf *out, sb_names sb_names,
			     struct bch_sb *sb,
			     struct bch_sb_field_disk_groups *gi,
			     struct bch_member m, unsigned idx)
{
	if (!bch2_member_alive(&m))
		return;

	struct sb_name *name = sb_dev_to_name(sb_names, idx);
	prt_printf(out, "Device %u:\t%s\t", idx, name ? name->name : "(not found)");

	if (name) {
		char *model = fd_to_dev_model(name->sb.bdev->bd_fd);
		prt_str(out, model);
		free(model);
	}
	prt_newline(out);

	printbuf_indent_add(out, 2);
	bch2_member_to_text(out, &m, gi, sb, idx);
	printbuf_indent_sub(out, 2);
}

void bch2_sb_to_text_with_names(struct printbuf *out,
				struct bch_fs *c, struct bch_sb *sb,
				bool print_layout, unsigned fields, int field_only)
{
	CLASS(printbuf, uuid_buf)();
	prt_str(&uuid_buf, "UUID=");
	pr_uuid(&uuid_buf, sb->user_uuid.b);

	sb_names sb_names = {};
	bch2_scan_device_sbs(uuid_buf.buf, &sb_names);

	if (field_only >= 0) {
		struct bch_sb_field *f = bch2_sb_field_get_id(sb, field_only);

		if (f)
			__bch2_sb_field_to_text(out, c, sb, f);
	} else {
		printbuf_tabstop_push(out, 44);

		bch2_sb_to_text(out, c, sb, print_layout,
				fields & ~(BIT(BCH_SB_FIELD_members_v1)|
					   BIT(BCH_SB_FIELD_members_v2)));

		struct bch_sb_field_disk_groups *gi = bch2_sb_field_get(sb, disk_groups);

		struct bch_sb_field_members_v1 *mi1;
		if ((fields & BIT(BCH_SB_FIELD_members_v1)) &&
		    (mi1 = bch2_sb_field_get(sb, members_v1)))
			for (unsigned i = 0; i < sb->nr_devices; i++)
				print_one_member(out, sb_names, sb, gi, bch2_members_v1_get(mi1, i), i);

		struct bch_sb_field_members_v2 *mi2;
		if ((fields & BIT(BCH_SB_FIELD_members_v2)) &&
		    (mi2 = bch2_sb_field_get(sb, members_v2)))
			for (unsigned i = 0; i < sb->nr_devices; i++)
				print_one_member(out, sb_names, sb, gi, bch2_members_v2_get(mi2, i), i);
	}
}

