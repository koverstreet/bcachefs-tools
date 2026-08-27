/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_SB_IO_H
#define _BCACHEFS_SB_IO_H

#include "data/extents.h"
#include "init/dev_types.h"
#include "sb/members.h"
#include "util/eytzinger.h"

#include <asm/byteorder.h>

#define BCH_SB_READ_SCRATCH_BUF_SIZE		4096

static inline bool bch2_version_compatible(u16 version)
{
	return BCH_VERSION_MAJOR(version) <= BCH_VERSION_MAJOR(bcachefs_metadata_version_current) &&
		version >= bcachefs_metadata_version_min;
}

void bch2_version_to_text(struct printbuf *, enum bcachefs_metadata_version);
enum bcachefs_metadata_version bch2_latest_compatible_version(enum bcachefs_metadata_version);

int bch2_set_version_incompat(struct bch_fs *, enum bcachefs_metadata_version);

static inline int bch2_request_incompat_feature(struct bch_fs *c,
						enum bcachefs_metadata_version version)
{
	return likely(version <= c->sb.version_incompat)
		? 0
		: bch2_set_version_incompat(c, version);
}

static inline size_t bch2_sb_field_bytes(struct bch_sb_field *f)
{
	return le32_to_cpu(f->u64s) * sizeof(u64);
}

#define field_to_type(_f, _name)					\
	container_of_or_null(_f, struct bch_sb_field_##_name, field)

struct bch_sb_field *bch2_sb_field_get_id(struct bch_sb *, enum bch_sb_field_type);
#define bch2_sb_field_get(_sb, _name)					\
	field_to_type(bch2_sb_field_get_id(_sb, BCH_SB_FIELD_##_name), _name)

struct bch_sb_field *bch2_sb_field_resize_id(struct bch_sb_handle *,
					     enum bch_sb_field_type, unsigned);
#define bch2_sb_field_resize(_sb, _name, _u64s)				\
	field_to_type(bch2_sb_field_resize_id(_sb, BCH_SB_FIELD_##_name, _u64s), _name)

struct bch_sb_field *bch2_sb_field_get_minsize_id(struct bch_sb_handle *,
					enum bch_sb_field_type, unsigned);
#define bch2_sb_field_get_minsize(_sb, _name, _u64s)				\
	field_to_type(bch2_sb_field_get_minsize_id(_sb, BCH_SB_FIELD_##_name, _u64s), _name)

#define bch2_sb_field_nr_entries(_f)					\
	(_f ? ((bch2_sb_field_bytes(&_f->field) - sizeof(*_f)) /	\
	       sizeof(_f->entries[0]))					\
	    : 0)

void bch2_sb_field_delete(struct bch_sb_handle *, enum bch_sb_field_type);

extern const char * const bch2_sb_fields[];

struct bch_sb_field_ops {
	int	(*validate)(struct bch_sb *, struct bch_sb_field *,
			    enum bch_validate_flags, struct printbuf *);
	void	(*to_text)(struct printbuf *, struct bch_fs *, struct bch_sb *, struct bch_sb_field *);
};

static inline __le64 bch2_sb_magic(struct bch_fs *c)
{
	__le64 ret;

	memcpy(&ret, &c->sb.uuid, sizeof(ret));
	return ret;
}

static inline __u64 jset_magic(struct bch_fs *c)
{
	return __le64_to_cpu(bch2_sb_magic(c) ^ JSET_MAGIC);
}

static inline __u64 bset_magic(struct bch_fs *c)
{
	return __le64_to_cpu(bch2_sb_magic(c) ^ BSET_MAGIC);
}

int bch2_sb_to_fs(struct bch_fs *, struct bch_sb *);
int bch2_sb_from_fs(struct bch_fs *, struct bch_dev *);

void bch2_free_super(struct bch_sb_handle *);
int bch2_sb_realloc(struct bch_sb_handle *, unsigned);

int bch2_sb_validate(struct bch_sb *, struct bch_opts *, u64,
		     enum bch_validate_flags, struct printbuf *);

int bch2_read_super(const char *, struct bch_opts *, struct bch_sb_handle *);
int bch2_read_super_silent(const char *, struct bch_opts *, struct bch_sb_handle *);
int bch2_write_super(struct bch_fs *);
int bch2_write_super_replicas(struct bch_fs *);
void __bch2_check_set_feature(struct bch_fs *, unsigned);

static inline void bch2_check_set_feature(struct bch_fs *c, unsigned feat)
{
	if (!(c->sb.features & (1ULL << feat)))
		__bch2_check_set_feature(c, feat);
}

bool bch2_check_version_downgrade(struct bch_fs *);
void bch2_sb_upgrade(struct bch_fs *, unsigned, bool);
void bch2_sb_upgrade_incompat(struct bch_fs *);

void __bch2_sb_field_to_text(struct printbuf *, struct bch_fs *, struct bch_sb *,
			     struct bch_sb_field *);
void bch2_sb_field_to_text(struct printbuf *, struct bch_fs *, struct bch_sb *,
			   struct bch_sb_field *);
void bch2_sb_layout_to_text(struct printbuf *, struct bch_sb_layout *);
void bch2_sb_to_text(struct printbuf *, struct bch_fs *, struct bch_sb *, bool, unsigned);

/*
 * Permission to modify a superblock field, and the thing that writes it back.
 * Hold sb_lock - guard(mutex_noio)(&c->sb_lock) - and declare one of these
 * under it. Declaration order is load-bearing: declared after the lock guard,
 * this destructs first, so the write happens while sb_lock is still held.
 *
 * The setters return whether the fact was NEW, which is the caller's business
 * (an fsck message unsuppresses on novelty). The write-back accumulates
 * separately, so forgetting to use that return can't lose a superblock write.
 */
struct sb_write {
	struct bch_fs	*c;
	bool		dirty;
};

static inline struct sb_write sb_write_init(struct bch_fs *c)
{
	lockdep_assert_held(&c->sb_lock.lock);
	return (struct sb_write) { .c = c };
}

static inline void sb_write_exit(struct sb_write *w)
{
	if (w->dirty)
		bch2_write_super(w->c);
}

DEFINE_CLASS(sb_write, struct sb_write,
	     sb_write_exit(&_T), sb_write_init(c), struct bch_fs *c)

/*
 * Two ways to dirty: sb_dirty() when the caller has already decided a field
 * changed, sb_record() when a test-and-set decides for it - and then reports
 * whether the fact was new, which is a different question from whether the
 * superblock needs writing.
 */
static inline void sb_dirty(struct sb_write *w)
{
	w->dirty = true;
}

static inline bool sb_record(struct sb_write *w, bool was_new)
{
	if (was_new)
		sb_dirty(w);
	return was_new;
}

/*
 * Write now and report, for the callers that propagate the error. Clears
 * @dirty, so the guard's scope exit becomes a no-op and stays a backstop for
 * anything dirtied afterwards - it can't double-write.
 */
static inline int sb_write_flush(struct sb_write *w)
{
	if (!w->dirty)
		return 0;

	w->dirty = false;
	return bch2_write_super(w->c);
}

static inline bool sb_set_err_silent(struct sb_write *w, unsigned err)
{
	struct bch_sb_field_ext *ext = bch2_sb_field_get(w->c->disk_sb.sb, ext);

	return sb_record(w, !__test_and_set_bit_le64(err, ext->errors_silent));
}

static inline bool sb_set_btrees_lost_data(struct sb_write *w, unsigned btree)
{
	struct bch_sb_field_ext *ext = bch2_sb_field_get(w->c->disk_sb.sb, ext);

	return sb_record(w, !__test_and_set_bit_le64(btree, &ext->btrees_lost_data));
}

static inline bool sb_set_btrees_lost_data_ever(struct sb_write *w, unsigned btree)
{
	struct bch_sb_field_ext *ext = bch2_sb_field_get(w->c->disk_sb.sb, ext);

	return sb_record(w, !__test_and_set_bit_le64(btree, &ext->btrees_lost_data_ever));
}

#endif /* _BCACHEFS_SB_IO_H */
