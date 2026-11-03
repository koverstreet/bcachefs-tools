/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_ASYNC_OBJS_H
#define _BCACHEFS_ASYNC_OBJS_H

static inline void __async_object_list_del(struct fast_list *head, unsigned *idx)
{
	fast_list_remove(head, *idx);
	*idx = 0;
}

static inline int __async_object_list_add(struct fast_list *head, void *obj, unsigned *idx)
{
	int ret = fast_list_add(head, obj);
	*idx = ret > 0 ? ret : 0;
	return ret < 0 ? ret : 0;
}

#define async_object_list_del(_c, _list, idx)		\
	__async_object_list_del(&(_c)->async_objs[BCH_ASYNC_OBJ_LIST_##_list].list, &idx)

#define async_object_list_add(_c, _list, obj, idx)		\
	__async_object_list_add(&(_c)->async_objs[BCH_ASYNC_OBJ_LIST_##_list].list, obj, idx)

#endif /* _BCACHEFS_ASYNC_OBJS_H */
