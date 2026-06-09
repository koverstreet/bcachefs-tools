/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_DATA_COMPRESS_WORKERS_H
#define _BCACHEFS_DATA_COMPRESS_WORKERS_H

struct bch_fs;
struct closure;
struct workqueue_struct;

struct bch_compress_worker {
	void			*workspace;
	void			*dst_buf;
	void			*verify_buf;
};

struct bch_compress_wq {
	struct workqueue_struct	*wq;
	struct bch_compress_worker *workers;
	unsigned		nr_workers;
	struct bch_fs		*c;
};

struct bch_compress_work {
	struct work_struct	work;
	struct bch_compress_wq	*cwq;

	unsigned		compression_opt;
	struct bpos		write_pos;
	const void		*src;
	size_t			src_len;

	void			*dst;
	size_t			dst_len;
	unsigned		compression_type;

	struct bch_compress_worker *worker;

	struct closure		*parent;
};

int bch2_compress_wq_init(struct bch_fs *);
void bch2_compress_wq_destroy(struct bch_fs *);

void bch2_compress_wq_submit(struct bch_compress_work *,
			     struct bch_compress_wq *,
			     struct closure *,
			     unsigned, struct bpos,
			     const void *, size_t,
			     void *, size_t,
			     struct bch_compress_worker *);

#endif /* _BCACHEFS_DATA_COMPRESS_WORKERS_H */
