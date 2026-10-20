// SPDX-License-Identifier: GPL-2.0

#include "bcachefs.h"

#include "sb/errors.h"
#include "sb/io.h"

#include "util/darray.h"

static inline unsigned bch2_sb_field_errors_nr_entries(struct bch_sb_field_errors *e)
{
	return bch2_sb_field_nr_entries(e);
}

static inline unsigned bch2_sb_field_errors_v2_nr_entries(struct bch_sb_field_errors_v2 *e)
{
	return bch2_sb_field_nr_entries(e);
}

static inline unsigned bch2_sb_field_errors_v2_u64s(unsigned nr)
{
	return (sizeof(struct bch_sb_field_errors_v2) +
		sizeof(struct bch_sb_field_error_entry_v2) * nr) / sizeof(u64);
}

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

/* @err is a BCH_ERR_BLK_STS_* errcode, as thrown by blk_status_to_bch_err() */
enum bch_sb_error_id bch2_blk_sts_sb_err(int err)
{
	switch (abs(err)) {
#define x(_blk_sts, _name)						\
	case BCH_ERR_BLK_STS_##_blk_sts:				\
		return BCH_FSCK_ERR_blk_sts_##_name;
	BCH_BLK_STS_SB_ERRS()
#undef x
	default:
		return BCH_FSCK_ERR_blk_sts_unknown;
	}
}

/* @err is a BCH_ERR_decompress errcode, as thrown by buf_uncompress() */
enum bch_sb_error_id bch2_decompress_sb_err(int err)
{
	switch (abs(err)) {
#define x(_errcode, _name)						\
	case BCH_ERR_##_errcode:					\
		return BCH_FSCK_ERR_data_decompress_err_##_name;
	BCH_DECOMPRESS_SB_ERRS()
#undef x
	default:
		return bch2_err_matches(err, BCH_ERR_zstd_error)
			? BCH_FSCK_ERR_data_decompress_err_zstd_unknown
			: BCH_FSCK_ERR_data_decompress_err_unknown;
	}
}

/*
 * Same errcode, same table, same names as bch2_decompress_sb_err() above - the
 * reason we stamp into a KEY_TYPE_error key when this is what we finally gave
 * up on. The sb counter says how often decompression failed this way; the key
 * says which extent it cost us.
 */
enum bch_key_type_errors bch2_decompress_key_type_error(int err)
{
	switch (abs(err)) {
#define x(_errcode, _name)						\
	case BCH_ERR_##_errcode:					\
		return KEY_TYPE_ERROR_decompress_##_name;
	BCH_DECOMPRESS_SB_ERRS()
#undef x
	default:
		return bch2_err_matches(err, BCH_ERR_zstd_error)
			? KEY_TYPE_ERROR_decompress_zstd_unknown
			: KEY_TYPE_ERROR_decompress_unknown;
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
