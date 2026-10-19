/* SPDX-License-Identifier: GPL-2.0 */

#include "sb/io.h"
#include "util/vstructs.h"

static inline unsigned bch2_nr_journal_buckets(struct bch_sb_field_journal *j)
{
	return j
		? (__le64 *) vstruct_end(&j->field) - j->buckets
		: 0;
}

static inline unsigned bch2_sb_field_journal_v2_nr_entries(struct bch_sb_field_journal_v2 *j)
{
	if (!j)
		return 0;

	return (struct bch_sb_field_journal_v2_entry *) vstruct_end(&j->field) - &j->d[0];
}

#include "journal/sb_gen.h"
