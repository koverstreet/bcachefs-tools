/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_DATA_COMPRESS_H
#define _BCACHEFS_DATA_COMPRESS_H

#include "extents_types.h"

static const unsigned __bch2_compression_opt_to_type[] = {
#define x(t, n) [BCH_COMPRESSION_OPT_##t] = BCH_COMPRESSION_TYPE_##t,
	BCH_COMPRESSION_OPTS()
#undef x
};

union bch_compression_opt {
	u8 value;
	struct {
#if defined(__LITTLE_ENDIAN_BITFIELD)
		u8 type:4, level:4;
#elif defined(__BIG_ENDIAN_BITFIELD)
		u8 level:4, type:4;
#endif
	};
};

static inline bool bch2_compression_opt_valid(unsigned v)
{
	union bch_compression_opt opt = { .value = v };

	return opt.type < ARRAY_SIZE(__bch2_compression_opt_to_type) && !(!opt.type && opt.level);
}

static inline enum bch_compression_type bch2_compression_opt_to_type(unsigned v)
{
	union bch_compression_opt opt = { .value = v };

	if (unlikely(opt.type >= ARRAY_SIZE(__bch2_compression_opt_to_type))) {
		/* As in bch2_csum_opt_to_type() - shouldn't happen, and
		 * shouldn't read off the end of the table if it does: */
		WARN_ONCE(1, "unknown compression opt %u", v);
		return BCH_COMPRESSION_TYPE_none;
	}

	return __bch2_compression_opt_to_type[opt.type];
}

/*
 * Bounce buffer for the encode/decode paths: either a direct mapping of a
 * bio's pages (BB_none/BB_vmap - no copy) or a private allocation the caller
 * can scribble on without touching the bio. Exported because the write path
 * assembles its own decode sequence out of these.
 */
struct bbuf {
	struct bch_fs	*c;
	void		*b;
	enum bbuf_type {
		BB_none,
		BB_vmap,
		BB_kmalloc,
		BB_mempool,
	}		type;
	int		rw;
};

void bch2_bbuf_exit(struct bbuf *);
struct bbuf bch2_bounce_alloc(struct bch_fs *, unsigned, int);
struct bbuf bch2_bio_bounce(struct bch_fs *, struct bio *, struct bvec_iter, int);
struct bbuf bch2_bio_map_or_bounce(struct bch_fs *, struct bio *, int);
int bch2_buf_uncompress(struct bch_fs *, void *, void *, struct bch_extent_crc_unpacked);
int bch2_decompress_err(struct bch_fs *, int);

int bch2_bio_uncompress(struct bch_fs *, struct bio *, struct bio *,
		       struct bvec_iter, struct bch_extent_crc_unpacked);

unsigned bch2_bio_compress(struct bch_fs *, struct bio *, size_t *,
			   struct bio *, size_t *, unsigned,
			   struct bpos, bool);

int bch2_check_set_has_compressed_data(struct bch_fs *, unsigned);
void bch2_fs_compress_exit(struct bch_fs *);
int bch2_fs_compress_init(struct bch_fs *);

void bch2_compression_opt_to_text(struct printbuf *, u64);

int bch2_opt_compression_parse(struct bch_fs *, const char *, u64 *, struct printbuf *);
void bch2_opt_compression_to_text(struct printbuf *, struct bch_fs *, struct bch_sb *, u64);
int bch2_opt_compression_validate(u64, struct printbuf *);

#define bch2_opt_compression (struct bch_opt_fn) {		\
	.parse		= bch2_opt_compression_parse,		\
	.to_text	= bch2_opt_compression_to_text,		\
	.validate	= bch2_opt_compression_validate,	\
}

#endif /* _BCACHEFS_DATA_COMPRESS_H */
