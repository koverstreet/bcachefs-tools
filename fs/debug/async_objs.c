// SPDX-License-Identifier: GPL-2.0
/*
 * Async obj debugging: keep asynchronous objects on (very fast) lists, make
 * them visibile in debugfs - the files are async_objs.rs's:
 */

#include "bcachefs.h"

#ifdef CONFIG_BCACHEFS_ASYNC_OBJECT_LISTS

#include "btree/read.h"
#include "btree/write.h"

#include "data/read.h"
#include "data/write.h"

#include "debug/async_objs.h"

static __cold void promote_obj_to_text(struct printbuf *out,
				struct bch_fs *c,
				void *obj)
{
	bch2_promote_op_to_text(out, c, obj);
}

static __cold void rbio_obj_to_text(struct printbuf *out,
			     struct bch_fs *c,
			     void *obj)
{
	bch2_read_bio_to_text(out, c, obj);
}

static __cold void write_op_obj_to_text(struct printbuf *out,
				 struct bch_fs *c,
				 void *obj)
{
	bch2_write_op_to_text(out, obj);
}

static __cold void btree_read_bio_obj_to_text(struct printbuf *out,
				       struct bch_fs *c,
				       void *obj)
{
	struct btree_read_bio *rbio = obj;
	bch2_btree_read_bio_to_text(out, rbio);
}

static __cold void btree_write_bio_obj_to_text(struct printbuf *out,
					struct bch_fs *c,
					void *obj)
{
	struct btree_write_bio *wbio = obj;
	bch2_bio_to_text(out, &wbio->wbio.bio);
}

void bch2_fs_async_obj_exit(struct bch_fs *c)
{
	for (unsigned i = 0; i < ARRAY_SIZE(c->async_objs); i++)
		fast_list_exit(&c->async_objs[i].list);
}

int bch2_fs_async_obj_init(struct bch_fs *c)
{
	for (unsigned i = 0; i < ARRAY_SIZE(c->async_objs); i++) {
		if (fast_list_init(&c->async_objs[i].list))
			return -BCH_ERR_ENOMEM_async_obj_init;
		c->async_objs[i].idx = i;
	}

#define x(n) c->async_objs[BCH_ASYNC_OBJ_LIST_##n].obj_to_text = n##_obj_to_text;
	BCH_ASYNC_OBJ_LISTS()
#undef x

	return 0;
}

#endif /* CONFIG_BCACHEFS_ASYNC_OBJECT_LISTS */
