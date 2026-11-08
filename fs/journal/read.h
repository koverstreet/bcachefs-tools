/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_JOURNAL_READ_H
#define _BCACHEFS_JOURNAL_READ_H

#include "data/checksum.h"

#include "data_structures/darray.h"

static inline struct jset_entry *__jset_entry_type_next(struct jset *jset,
					struct jset_entry *entry, unsigned type)
{
	while (entry < vstruct_last(jset)) {
		if (entry->type == type)
			return entry;

		entry = vstruct_next(entry);
	}

	return NULL;
}

#define for_each_jset_entry_type(entry, jset, type)			\
	for (struct jset_entry *entry = (jset)->start;			\
	     (entry = __jset_entry_type_next(jset, entry, type));	\
	     entry = vstruct_next(entry))

#define jset_entry_for_each_key(_e, _k)					\
	for (struct bkey_i *_k = (_e)->start;				\
	     _k < vstruct_last(_e);					\
	     _k = bkey_next(_k))

#define for_each_jset_key(k, entry, jset)				\
	for_each_jset_entry_type(entry, jset, BCH_JSET_ENTRY_btree_keys)\
		jset_entry_for_each_key(entry, k)

static inline time64_t jset_datetime(struct jset *j)
{
	for_each_jset_entry_type(entry, j, BCH_JSET_ENTRY_datetime) {
		struct jset_entry_datetime *dt =
			container_of(entry, struct jset_entry_datetime, entry);
		return le64_to_cpu(dt->seconds);
	}
	return 0;
}


#endif /* _BCACHEFS_JOURNAL_READ_H */
