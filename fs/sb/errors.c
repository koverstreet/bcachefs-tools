// SPDX-License-Identifier: GPL-2.0

#include "bcachefs.h"

#include "sb/errors.h"
#include "sb/io.h"

#include "util/darray.h"

const char * const bch2_sb_error_strs[] = {
#define x(t, n, ...) [n] = #t,
	BCH_SB_ERRS()
#undef x
};

/*
 * The ids are persistent indices into on-disk error counters: a duplicate
 * silently aliases two errors' counts, and neither C nor the enum will
 * complain. Enforce at build time that the ids are exactly 0..MAX:
 *
 * - the switch makes a duplicate id a compile error (duplicate case label)
 * - given distinctness, the minimum possible sum of MAX+1 non-negative
 *   integers is 0+1+...+MAX, achieved only by exactly that set - so the sum
 *   check rules out gaps and strays above MAX in one shot
 */
static inline void __maybe_unused bch2_sb_errs_check_unique(void)
{
	switch (0) {
	case -1:
#define x(t, n, ...) case n:
	BCH_SB_ERRS()
#undef x
		;
	}
}

#define x(t, n, ...) + (n)
static_assert((0ULL BCH_SB_ERRS()) ==
	      (unsigned long long) BCH_FSCK_ERR_MAX * (BCH_FSCK_ERR_MAX + 1) / 2,
	      "sb error ids are not dense: gap, duplicate, or id > MAX");
#undef x

__cold void bch2_sb_error_id_to_text(struct printbuf *out, enum bch_sb_error_id id)
{
	if (id < BCH_FSCK_ERR_MAX)
		prt_str(out, bch2_sb_error_strs[id]);
	else
		prt_printf(out, "(unknown error %u)", id);
}

static inline unsigned bch2_sb_field_errors_nr_entries(struct bch_sb_field_errors *e)
{
	return bch2_sb_field_nr_entries(e);
}

static inline unsigned bch2_sb_field_errors_u64s(unsigned nr)
{
	return (sizeof(struct bch_sb_field_errors) +
		sizeof(struct bch_sb_field_error_entry) * nr) / sizeof(u64);
}

static int bch2_sb_errors_validate(struct bch_sb *sb, struct bch_sb_field *f,
				   enum bch_validate_flags flags, struct printbuf *err)
{
	struct bch_sb_field_errors *e = field_to_type(f, errors);
	unsigned i, nr = bch2_sb_field_errors_nr_entries(e);

	for (i = 0; i < nr; i++) {
		if (!BCH_SB_ERROR_ENTRY_NR(&e->entries[i])) {
			prt_printf(err, "entry with count 0 (id ");
			bch2_sb_error_id_to_text(err, BCH_SB_ERROR_ENTRY_ID(&e->entries[i]));
			prt_printf(err, ")");
			return -BCH_ERR_invalid_sb_errors;
		}

		if (i + 1 < nr &&
		    BCH_SB_ERROR_ENTRY_ID(&e->entries[i]) >=
		    BCH_SB_ERROR_ENTRY_ID(&e->entries[i + 1])) {
			prt_printf(err, "entries out of order");
			return -BCH_ERR_invalid_sb_errors;
		}
	}

	return 0;
}

static int error_entry_cmp(const void *_l, const void *_r)
{
	const struct bch_sb_field_error_entry *l = _l;
	const struct bch_sb_field_error_entry *r = _r;

	return -cmp_int(l->last_error_time, r->last_error_time);
}

DEFINE_DARRAY(bch_sb_field_error_entry);

static __cold void bch2_sb_errors_to_text(struct printbuf *out,
				   struct bch_fs *c,
				   struct bch_sb *sb,
				   struct bch_sb_field *f)
{
	struct bch_sb_field_errors *e = field_to_type(f, errors);
	unsigned nr = bch2_sb_field_errors_nr_entries(e);

	if (out->nr_tabstops <= 1)
		printbuf_tabstop_push(out, 16);

	CLASS(darray_bch_sb_field_error_entry, sorted)();

	for (struct bch_sb_field_error_entry *i = e->entries; i < e->entries + nr; i++)
		darray_push(&sorted, *i);

	darray_sort(sorted, error_entry_cmp);

	darray_for_each(sorted, i) {
		bch2_sb_error_id_to_text(out, BCH_SB_ERROR_ENTRY_ID(i));
		prt_tab(out);
		prt_u64(out, BCH_SB_ERROR_ENTRY_NR(i));
		prt_tab(out);
		bch2_prt_datetime(out, le64_to_cpu(i->last_error_time));
		prt_newline(out);
	}
}

const struct bch_sb_field_ops bch_sb_field_ops_errors = {
	.validate	= bch2_sb_errors_validate,
	.to_text	= bch2_sb_errors_to_text,
};

/* v2: adds the time of first occurrence */

static inline unsigned bch2_sb_field_errors_v2_nr_entries(struct bch_sb_field_errors_v2 *e)
{
	return bch2_sb_field_nr_entries(e);
}

static inline unsigned bch2_sb_field_errors_v2_u64s(unsigned nr)
{
	return (sizeof(struct bch_sb_field_errors_v2) +
		sizeof(struct bch_sb_field_error_entry_v2) * nr) / sizeof(u64);
}

/* A saturated count is a floor, not an exact value: */
void bch2_prt_error_nr(struct printbuf *out, u64 nr)
{
	prt_u64(out, nr);
	if (nr >= BCH_SB_ERROR_ENTRY_V2_NR_MAX)
		prt_char(out, '+');
}

static int bch2_sb_errors_v2_validate(struct bch_sb *sb, struct bch_sb_field *f,
				      enum bch_validate_flags flags, struct printbuf *err)
{
	struct bch_sb_field_errors_v2 *e = field_to_type(f, errors_v2);
	unsigned i, nr = bch2_sb_field_errors_v2_nr_entries(e);

	for (i = 0; i < nr; i++) {
		if (!BCH_SB_ERROR_ENTRY_V2_NR(&e->entries[i])) {
			prt_printf(err, "entry with count 0 (id ");
			bch2_sb_error_id_to_text(err, BCH_SB_ERROR_ENTRY_V2_ID(&e->entries[i]));
			prt_printf(err, ")");
			return -BCH_ERR_invalid_sb_errors;
		}

		if (BCH_SB_ERROR_ENTRY_V2_FIRST(&e->entries[i]) >
		    BCH_SB_ERROR_ENTRY_V2_LAST(&e->entries[i])) {
			prt_printf(err, "entry with first occurrence after last (id ");
			bch2_sb_error_id_to_text(err, BCH_SB_ERROR_ENTRY_V2_ID(&e->entries[i]));
			prt_printf(err, ")");
			return -BCH_ERR_invalid_sb_errors;
		}

		if (i + 1 < nr &&
		    BCH_SB_ERROR_ENTRY_V2_ID(&e->entries[i]) >=
		    BCH_SB_ERROR_ENTRY_V2_ID(&e->entries[i + 1])) {
			prt_printf(err, "entries out of order");
			return -BCH_ERR_invalid_sb_errors;
		}
	}

	return 0;
}

static int error_entry_v2_cmp(const void *_l, const void *_r)
{
	const struct bch_sb_field_error_entry_v2 *l = _l;
	const struct bch_sb_field_error_entry_v2 *r = _r;

	return -cmp_int(BCH_SB_ERROR_ENTRY_V2_LAST(l),
			BCH_SB_ERROR_ENTRY_V2_LAST(r));
}

DEFINE_DARRAY(bch_sb_field_error_entry_v2);

static __cold void bch2_sb_errors_v2_to_text(struct printbuf *out,
					     struct bch_fs *c,
					     struct bch_sb *sb,
					     struct bch_sb_field *f)
{
	struct bch_sb_field_errors_v2 *e = field_to_type(f, errors_v2);
	unsigned nr = bch2_sb_field_errors_v2_nr_entries(e);

	if (out->nr_tabstops <= 1)
		printbuf_tabstop_push(out, 16);

	CLASS(darray_bch_sb_field_error_entry_v2, sorted)();

	for (struct bch_sb_field_error_entry_v2 *i = e->entries; i < e->entries + nr; i++)
		darray_push(&sorted, *i);

	darray_sort(sorted, error_entry_v2_cmp);

	darray_for_each(sorted, i) {
		bch2_sb_error_id_to_text(out, BCH_SB_ERROR_ENTRY_V2_ID(i));
		prt_tab(out);
		bch2_prt_error_nr(out, BCH_SB_ERROR_ENTRY_V2_NR(i));
		prt_tab(out);
		bch2_prt_datetime(out, BCH_SB_ERROR_ENTRY_V2_FIRST(i));
		prt_tab(out);
		bch2_prt_datetime(out, BCH_SB_ERROR_ENTRY_V2_LAST(i));
		prt_newline(out);
	}
}

const struct bch_sb_field_ops bch_sb_field_ops_errors_v2 = {
	.validate	= bch2_sb_errors_v2_validate,
	.to_text	= bch2_sb_errors_v2_to_text,
};

__cold void bch2_fs_errors_to_text(struct printbuf *out, struct bch_fs *c)
{
	if (out->nr_tabstops < 1)
		printbuf_tabstop_push(out, 48);
	if (out->nr_tabstops < 2)
		printbuf_tabstop_push(out, 8);
	if (out->nr_tabstops < 3)
		printbuf_tabstop_push(out, 16);
	if (out->nr_tabstops < 4)
		printbuf_tabstop_push(out, 16);

	guard(mutex)(&c->errors.counts_lock);

	bch_sb_errors_cpu *e = &c->errors.counts;
	darray_for_each(*e, i) {
		bch2_sb_error_id_to_text(out, i->id);
		prt_tab(out);
		prt_u64(out, i->nr);
		prt_tab(out);
		bch2_prt_datetime(out, i->first_error_time);
		prt_tab(out);
		bch2_prt_datetime(out, i->last_error_time);
		prt_newline(out);
	}
}

void bch2_sb_error_count(struct bch_fs *c, enum bch_sb_error_id err)
{
	bch_sb_errors_cpu *e = &c->errors.counts;
	struct bch_sb_error_entry_cpu n = {
		.id = err,
		.nr = 1,
		.first_error_time = ktime_get_real_seconds(),
	};
	n.last_error_time = n.first_error_time;
	unsigned i;

	guard(mutex)(&c->errors.counts_lock);

	for (i = 0; i < e->nr; i++) {
		if (err == e->data[i].id) {
			e->data[i].nr++;
			e->data[i].last_error_time = n.last_error_time;
			return;
		}
		if (err < e->data[i].id)
			break;
	}

	if (darray_make_room(e, 1))
		return;

	darray_insert_item(e, i, n);
}

void bch2_sb_errors_from_cpu(struct bch_fs *c)
{
	guard(mutex)(&c->errors.counts_lock);

	bch_sb_errors_cpu *src = &c->errors.counts;
	struct bch_sb_field_errors_v2 *dst =
		bch2_sb_field_resize(&c->disk_sb, errors_v2,
				     bch2_sb_field_errors_v2_u64s(src->nr));
	if (!dst)
		return;

	for (unsigned i = 0; i < src->nr; i++) {
		SET_BCH_SB_ERROR_ENTRY_V2_ID(&dst->entries[i],		src->data[i].id);
		SET_BCH_SB_ERROR_ENTRY_V2_NR(&dst->entries[i],		src->data[i].nr);
		SET_BCH_SB_ERROR_ENTRY_V2_FIRST(&dst->entries[i],	src->data[i].first_error_time);
		SET_BCH_SB_ERROR_ENTRY_V2_LAST(&dst->entries[i],	src->data[i].last_error_time);
	}

	bch2_sb_field_delete(&c->disk_sb, BCH_SB_FIELD_errors);
}

/*
 * Reading counts in merges both sections, because both present at once
 * means the filesystem was downgraded: writing v2 always deletes legacy
 * in the same superblock write, so a legacy section coexisting with v2
 * was recreated by an old kernel afterwards and holds exactly the counts
 * accrued since. Sum the counts, take the union of the time ranges (a
 * legacy timestamp is both first and last: the error happened at least
 * that recently), and the next writeout folds everything back into v2 -
 * nothing lost or double counted across downgrade/upgrade cycles.
 *
 * Both sections are validated strictly ascending by id, so this is a
 * plain sorted merge.
 */
int bch2_sb_errors_to_cpu(struct bch_fs *c)
{
	guard(mutex)(&c->errors.counts_lock);

	bch_sb_errors_cpu *dst = &c->errors.counts;

	struct bch_sb_field_errors_v2 *v2 = bch2_sb_field_get(c->disk_sb.sb, errors_v2);
	struct bch_sb_field_errors *legacy = bch2_sb_field_get(c->disk_sb.sb, errors);
	unsigned nr_v2	= bch2_sb_field_errors_v2_nr_entries(v2);
	unsigned nr_l	= bch2_sb_field_errors_nr_entries(legacy);

	try(darray_make_room(dst, nr_v2 + nr_l));
	dst->nr = 0;

	unsigned i = 0, j = 0;
	while (i < nr_v2 || j < nr_l) {
		u64 id_v2 = i < nr_v2 ? BCH_SB_ERROR_ENTRY_V2_ID(&v2->entries[i]) : U64_MAX;
		u64 id_l  = j < nr_l  ? BCH_SB_ERROR_ENTRY_ID(&legacy->entries[j]) : U64_MAX;
		struct bch_sb_error_entry_cpu n;

		if (id_v2 < id_l) {
			n = (struct bch_sb_error_entry_cpu) {
				.id			= id_v2,
				.nr			= BCH_SB_ERROR_ENTRY_V2_NR(&v2->entries[i]),
				.first_error_time	= BCH_SB_ERROR_ENTRY_V2_FIRST(&v2->entries[i]),
				.last_error_time	= BCH_SB_ERROR_ENTRY_V2_LAST(&v2->entries[i]),
			};
			i++;
		} else if (id_l < id_v2) {
			u64 t = le64_to_cpu(legacy->entries[j].last_error_time);
			n = (struct bch_sb_error_entry_cpu) {
				.id			= id_l,
				.nr			= BCH_SB_ERROR_ENTRY_NR(&legacy->entries[j]),
				.first_error_time	= t,
				.last_error_time	= t,
			};
			j++;
		} else {
			u64 t = le64_to_cpu(legacy->entries[j].last_error_time);
			n = (struct bch_sb_error_entry_cpu) {
				.id			= id_v2,
				.nr			= BCH_SB_ERROR_ENTRY_V2_NR(&v2->entries[i]) +
					BCH_SB_ERROR_ENTRY_NR(&legacy->entries[j]),
				.first_error_time	= min(BCH_SB_ERROR_ENTRY_V2_FIRST(&v2->entries[i]), t),
				.last_error_time	= max(BCH_SB_ERROR_ENTRY_V2_LAST(&v2->entries[i]), t),
			};
			i++;
			j++;
		}

		dst->data[dst->nr++] = n;
	}

	return 0;
}
